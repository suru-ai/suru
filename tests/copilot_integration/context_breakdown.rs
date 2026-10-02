//! A Context Breakdown read on request from Copilot's `session.metadata.getContextAttribution`,
//! through the runtime, server and client contract.

use std::time::Duration;

use crate::support::{
    ScriptedCopilot, conversation_arms, opened_session_on, send_arm, settled_session,
};
use suru::{
    protocol::{
        ContextBreakdown, ContextFill, ContextPart, ContextSource, SessionError, SessionErrorCode,
    },
    provider::CopilotRuntime,
};

const TURN: &str = r#"      event answer assistant.message '{"messageId":"m1","content":"Measured"}'
      event idle session.idle '{}'
"#;

/// A Turn in which Copilot reports the window its Model is served at, then a reading that names
/// none, which leaves the window as last reported.
const MEASURED_TURN: &str = r#"      usage_info() {
        reply '{"jsonrpc":"2.0","method":"session.event","params":{"sessionId":"'"$sid"'","event":{"id":"'"$1"'","timestamp":"2026-01-01T00:00:00Z","ephemeral":true,"agentId":null,"type":"session.usage_info","data":'"$2"'}}}'
      }
      usage_info u1 '{"currentTokens":12400,"tokenLimit":200000,"messagesLength":3}'
      usage_info u2 '{"currentTokens":20500}'
      event answer assistant.message '{"messageId":"m1","content":"Measured"}'
      event idle session.idle '{}'
"#;

/// An attribution shaped as the CLI answered on 2026-10-02: its limits are the runtime default
/// whatever the Model, so free space and buffer are measured against nothing real.
const ATTRIBUTION: &str = r#"{"contextAttribution":{"totalTokens":20500,"modelId":"claude-fixture","modelSource":"selected","promptTokenLimit":128000,"limit":128000,"bufferTokens":30000,"compactionThreshold":100000,"categories":{"systemPrompt":6000,"customInstructions":1500,"systemTools":9000,"mcpTools":0,"messages":4000,"freeSpace":77500,"buffer":30000},"entries":[{"kind":"system","id":"system:prompt","label":"System","tokens":6000}],"compactions":{"count":0}}}"#;

fn attribution_arm(result: &str) -> String {
    format!(
        r#"    *'"method":"session.metadata.getContextAttribution"'*)
      reply '{{"jsonrpc":"2.0","id":'"$id"',"result":{result}}}'
      ;;
"#
    )
}

fn fixture(arm: &str) -> ScriptedCopilot {
    fixture_playing(TURN, arm)
}

fn fixture_playing(turn: &str, arm: &str) -> ScriptedCopilot {
    ScriptedCopilot::new(&format!("{}{}{arm}", conversation_arms(), send_arm(turn)))
}

fn parts() -> Vec<ContextPart> {
    vec![
        part(ContextSource::SystemPrompt, 6000),
        part(ContextSource::Instructions, 1500),
        part(ContextSource::SystemTools, 9000),
        part(ContextSource::McpTools, 0),
        part(ContextSource::Messages, 4000),
    ]
}

fn part(source: ContextSource, tokens: u64) -> ContextPart {
    ContextPart {
        source,
        tokens,
        items: Vec::new(),
    }
}

fn refusal(error: &anyhow::Error) -> Option<SessionErrorCode> {
    error.downcast_ref::<SessionError>().map(|error| error.code)
}

#[tokio::test]
async fn a_running_session_breaks_its_context_down_against_its_token_limit() {
    let copilot = fixture_playing(MEASURED_TURN, &attribution_arm(ATTRIBUTION));
    let opened = opened_session_on(
        CopilotRuntime::new(copilot.executable()),
        "copilot-context-breakdown",
        "Measure context",
    )
    .await;
    settled_session(&opened.client, opened.session_id, 0).await;

    let breakdown = opened
        .client
        .context_breakdown(opened.session_id)
        .await
        .unwrap();

    assert_eq!(
        breakdown,
        ContextBreakdown {
            fill: ContextFill {
                occupied_tokens: 20500,
                // tokenLimit, never the attribution's runtime-default limit of 128000.
                capacity_tokens: Some(200000),
            },
            // The attribution's buffer is measured against that default, so none is known.
            reserved_tokens: None,
            parts: parts(),
        }
    );
    opened.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_session_without_a_reported_token_limit_has_no_window() {
    let copilot = fixture(&attribution_arm(ATTRIBUTION));
    let opened = opened_session_on(
        CopilotRuntime::new(copilot.executable()),
        "copilot-context-breakdown-windowless",
        "Measure context",
    )
    .await;
    settled_session(&opened.client, opened.session_id, 0).await;

    let breakdown = opened
        .client
        .context_breakdown(opened.session_id)
        .await
        .unwrap();

    assert_eq!(breakdown.fill.capacity_tokens, None);
    assert_eq!(breakdown.parts, parts());
    opened.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_session_copilot_has_not_measured_fails_with_why() {
    let copilot = fixture(&attribution_arm(r#"{"contextAttribution":null}"#));
    let opened = opened_session_on(
        CopilotRuntime::new(copilot.executable()),
        "copilot-context-breakdown-unmeasured",
        "Measure context",
    )
    .await;
    settled_session(&opened.client, opened.session_id, 0).await;

    let refused = opened
        .client
        .context_breakdown(opened.session_id)
        .await
        .expect_err("nothing measured is no breakdown");

    assert_eq!(
        refusal(&refused),
        Some(SessionErrorCode::ContextBreakdownFailed),
        "{refused:#}"
    );
    assert!(
        format!("{refused:#}").contains("not measured"),
        "{refused:#}"
    );
    opened.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_cli_that_never_answers_fails_once_the_request_times_out() {
    let copilot = fixture("");
    let opened = opened_session_on(
        CopilotRuntime::new(copilot.executable())
            .with_interrupt_request_timeout(Duration::from_millis(50)),
        "copilot-context-breakdown-silent",
        "Measure context",
    )
    .await;
    settled_session(&opened.client, opened.session_id, 0).await;

    let refused = opened
        .client
        .context_breakdown(opened.session_id)
        .await
        .expect_err("silence is no breakdown");

    assert_eq!(
        refusal(&refused),
        Some(SessionErrorCode::ContextBreakdownFailed),
        "{refused:#}"
    );
    assert!(format!("{refused:#}").contains("timed out"), "{refused:#}");
    opened.server.shutdown().await.unwrap();
}
