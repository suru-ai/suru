//! A Context Breakdown asked of a Session's Provider through the server, whatever the Provider.
use crate::support::{WorkingTurn, read_session_until, the_subagent_row, working_turn};
use suru::{
    protocol::{
        Activity, ContextBreakdown, ContextFill, ContextPart, ContextSource, SessionError,
        SessionErrorCode, SessionId,
    },
    provider::{ProviderEvent, ProviderSubagentId},
};
use tokio::task::JoinHandle;

fn breakdown() -> ContextBreakdown {
    ContextBreakdown {
        fill: ContextFill {
            occupied_tokens: 9_000,
            capacity_tokens: Some(200_000),
        },
        reserved_tokens: Some(30_000),
        parts: vec![ContextPart {
            source: ContextSource::Messages,
            tokens: 9_000,
            items: Vec::new(),
        }],
    }
}

/// Asks for `session_id`'s Context Breakdown on a task of its own, so a test
/// can answer for the Provider while the request waits.
fn ask(fixture: &WorkingTurn, session_id: SessionId) -> JoinHandle<reqwest::Response> {
    let client = fixture.client.clone();
    let descriptor = fixture.server.descriptor().clone();
    tokio::spawn(async move {
        client
            .get(format!(
                "{}/v1/sessions/{session_id}/context",
                descriptor.base_url
            ))
            .bearer_auth(&descriptor.token)
            .send()
            .await
            .expect("send the Context Breakdown request")
    })
}

async fn refusal(response: reqwest::Response) -> SessionError {
    assert!(!response.status().is_success(), "{}", response.status());
    response.json().await.expect("a refusal is a Session error")
}

#[tokio::test]
async fn a_working_sessions_provider_answers_without_holding_up_the_turn() {
    let state = tempfile::tempdir().unwrap();
    let mut fixture = working_turn(state.path(), "context-breakdown-working").await;
    let asked = ask(&fixture, fixture.session_id);
    let held = fixture.provider_session.next_context_breakdown().await;

    // The Turn's own output is taken while the Provider has yet to answer.
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentStarted {
            subagent_id: ProviderSubagentId::new("reader"),
            name: "Reader".into(),
            description: "Read".into(),
            delegation: None,
        })
        .await;
    held.answer(breakdown());

    let response = asked.await.unwrap();
    assert!(response.status().is_success(), "{}", response.status());
    assert_eq!(
        response.json::<ContextBreakdown>().await.unwrap(),
        breakdown()
    );
    fixture.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_provider_that_attributes_nothing_is_refused_without_being_asked() {
    let state = tempfile::tempdir().unwrap();
    let fixture = working_turn(state.path(), "context-breakdown-unsupported").await;
    fixture.runtime.withdraw_context_breakdown();

    let refused = refusal(ask(&fixture, fixture.session_id).await.unwrap()).await;

    assert_eq!(refused.code, SessionErrorCode::ContextBreakdownUnsupported);
    fixture.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_failed_provider_request_is_refused_with_its_reason() {
    let state = tempfile::tempdir().unwrap();
    let mut fixture = working_turn(state.path(), "context-breakdown-failed").await;
    let asked = ask(&fixture, fixture.session_id);
    fixture
        .provider_session
        .next_context_breakdown()
        .await
        .fail("the native request timed out");

    let refused = refusal(asked.await.unwrap()).await;

    assert_eq!(refused.code, SessionErrorCode::ContextBreakdownFailed);
    assert!(
        refused.message.contains("the native request timed out"),
        "{}",
        refused.message
    );
    fixture.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_native_subagents_session_is_refused_since_its_parents_provider_holds_it() {
    let state = tempfile::tempdir().unwrap();
    let fixture = working_turn(state.path(), "context-breakdown-subagent").await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentStarted {
            subagent_id: ProviderSubagentId::new("reader"),
            name: "Reader".into(),
            description: "Read".into(),
            delegation: None,
        })
        .await;
    let parent = read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "the Subagent opens",
        |s| !s.activities.is_empty(),
    )
    .await;
    let Activity::Subagent {
        session_id: child, ..
    } = the_subagent_row(&parent)
    else {
        unreachable!()
    };

    let refused = refusal(ask(&fixture, *child).await.unwrap()).await;

    assert_eq!(refused.code, SessionErrorCode::ContextBreakdownUnavailable);
    fixture.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_unknown_session_is_not_found() {
    let state = tempfile::tempdir().unwrap();
    let fixture = working_turn(state.path(), "context-breakdown-unknown").await;

    let refused = refusal(ask(&fixture, SessionId::new()).await.unwrap()).await;

    assert_eq!(refused.code, SessionErrorCode::SessionNotFound);
    fixture.server.shutdown().await.unwrap();
}
