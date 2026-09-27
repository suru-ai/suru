//! Deleting a Session whose tree holds a brokered Subagent. The Subagent's
//! Session is deleted with its parent, as any Subagent's is, and because it
//! runs on a Provider actor of its own (ADR 0035), that actor closes along
//! with the parent's: nothing is left running for a Session that no longer
//! exists, and no Broker token outlives the Provider it was handed to.

use suru::protocol::{SessionError, SessionErrorCode, SessionListItem};

use super::*;

/// Asks the Server to delete `session_id`, as a client does, answering with
/// the raw response.
async fn delete(descriptor: &RuntimeDescriptor, session_id: SessionId) -> reqwest::Response {
    reqwest::Client::new()
        .delete(format!("{}/v1/sessions/{session_id}", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("send Session deletion")
}

/// What the Server answers a read of `session_id` with.
async fn read_status(descriptor: &RuntimeDescriptor, session_id: SessionId) -> StatusCode {
    reqwest::Client::new()
        .get(format!("{}/v1/sessions/{session_id}", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("read Session")
        .status()
}

/// Every Session the Server's listing names.
async fn listed(descriptor: &RuntimeDescriptor) -> Vec<SessionId> {
    reqwest::Client::new()
        .get(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("list Sessions")
        .error_for_status()
        .expect("the listing answers")
        .json::<Vec<SessionListItem>>()
        .await
        .expect("decode the Session listing")
        .iter()
        .filter_map(|item| item.readable().map(|summary| summary.session.id))
        .collect()
}

/// Why the Server refused a deletion.
async fn refusal(response: reqwest::Response) -> SessionErrorCode {
    assert_eq!(response.status(), StatusCode::CONFLICT);
    response
        .json::<SessionError>()
        .await
        .expect("decode the refusal")
        .code
}

#[tokio::test]
async fn deleting_the_parent_closes_both_actors_and_removes_both_sessions() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "broker-delete-parent";
    let mut delegating = delegating(state_dir.path(), channel, None).await;
    let descriptor = delegating.descriptor.clone();
    let child_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let start = next_start(&mut delegating.hosted.codex).await;
    let child_handoff = start
        .broker()
        .cloned()
        .expect("a brokered Subagent is handed the Broker too");
    let mut child_provider = start.succeed(AgentIdentity {
        agent: AgentId::new("codex-agent"),
        selection: codex_selection("high"),
    });
    timeout(PROGRESS_DEADLINE, child_provider.next_turn())
        .await
        .expect("the Delegation reaches the Subagent's Provider")
        .succeed();

    // The tree is Working while the Subagent works, whatever its parent's
    // own Turn has done, so it cannot be deleted yet.
    delegating
        .caller_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    read_until(
        &descriptor,
        delegating.caller,
        "the parent's Turn settles while its brokered Subagent works",
        |snapshot| snapshot.turns[0].status == TurnStatus::Completed,
    )
    .await;
    assert_eq!(
        refusal(delete(&descriptor, delegating.caller).await).await,
        SessionErrorCode::WorkingSession
    );
    assert!(
        !delegating.caller_provider.was_shut_down() && !child_provider.was_shut_down(),
        "a refused deletion closes nothing"
    );

    child_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    read_until(
        &descriptor,
        delegating.caller,
        "nothing in the tree works any more",
        |snapshot| snapshot.working_since().is_none(),
    )
    .await;
    assert_eq!(
        refusal(delete(&descriptor, child_id).await).await,
        SessionErrorCode::SubagentSession,
        "the Subagent's Session is deleted only with its parent"
    );
    assert!(!child_provider.was_shut_down());

    assert_eq!(
        delete(&descriptor, delegating.caller).await.status(),
        StatusCode::NO_CONTENT
    );
    timeout(
        PROGRESS_DEADLINE,
        delegating.caller_provider.next_shutdown(),
    )
    .await
    .expect("the parent's Provider Session is shut down");
    timeout(PROGRESS_DEADLINE, child_provider.next_shutdown())
        .await
        .expect("and so is the brokered Subagent's, on the actor it owns");
    for (handoff, whose) in [
        (&delegating.handoff, "the parent's"),
        (&child_handoff, "the Subagent's"),
    ] {
        assert_eq!(
            McpClient::handed(handoff).initialize_status().await,
            StatusCode::UNAUTHORIZED,
            "{whose} Broker token is retired with the Provider it was handed to"
        );
    }
    for session_id in [delegating.caller, child_id] {
        assert_eq!(
            read_status(&descriptor, session_id).await,
            StatusCode::NOT_FOUND,
            "{session_id} is gone"
        );
    }
    assert_eq!(listed(&descriptor).await, []);

    // Nothing of either is left in storage for the next process to restore.
    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
    let restarted = host_providers(state_dir.path(), channel, None).await;
    let descriptor = restarted.server.descriptor().clone();
    for session_id in [delegating.caller, child_id] {
        assert_eq!(
            read_status(&descriptor, session_id).await,
            StatusCode::NOT_FOUND,
            "{session_id} is not restored"
        );
    }
    assert_eq!(listed(&descriptor).await, []);

    restarted.server.shutdown().await.expect("shut down server");
}
