//! Native work resumed by Claude after background commands finish.

use crate::support::{
    CLAUDE_MODELS, ScriptedClaude, agent_messages, conversation_fixture, discovery_arms,
    interrupt_arm, opened_session, session_where, settled_session, user_turn_arm,
};
use suru::{
    managed_client::ManagedClient,
    protocol::{
        Activity, ActivityStatus, AdmitPromptRequest, InitialPrompt, PromptDelivery, PromptId,
        SessionId, SessionSummary, TurnStatus,
    },
};

/// A Turn that leaves a background command running and ends its loop, then — once released — the
/// command's completion waking the loop into a Continuation, which holds at its first message
/// until the `continuation` gate is released too.
const BACKGROUND_COMMAND: &str = r#"
      emit '{"type":"system","subtype":"task_started","task_id":"tests","task_type":"local_bash"}'
      emit '{"type":"result","subtype":"success","is_error":false,"result":"Waiting for tests."}'
      while [ ! -e "$CLAUDE_FIXTURE_RELEASE" ]; do sleep 0.01; done
      emit '{"type":"system","subtype":"task_notification","task_id":"tests","status":"completed","summary":"Background command \"cargo test\" completed (exit code 0)"}'
      emit '{"type":"stream_event","event":{"type":"message_start"}}'
      while [ ! -e "$CLAUDE_FIXTURE_RELEASE-continuation" ]; do sleep 0.01; done
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0}}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"text","text":"Tests passed."}}}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":1}}'
      emit '{"type":"result","subtype":"success","is_error":false,"result":"Tests passed."}'
"#;

/// The Session's row in the client's listing.
async fn listed(client: &ManagedClient, session_id: SessionId) -> SessionSummary {
    client
        .list_sessions(None)
        .await
        .expect("list Sessions")
        .into_iter()
        .find_map(|item| {
            item.readable()
                .filter(|summary| summary.session.id == session_id)
                .cloned()
        })
        .expect("the Session is listed and readable")
}

#[tokio::test]
async fn a_background_command_completion_resumes_into_a_visible_continuation() {
    let claude = conversation_fixture(BACKGROUND_COMMAND);
    let opened = opened_session(&claude, "claude-command-continuation", "Run tests").await;
    let first = settled_session(&opened.client, opened.session_id, 0).await;
    assert_eq!(first.turns[0].status, TurnStatus::Completed);
    assert_eq!(first.session.working_since, None);
    assert_eq!(
        first.session.monitoring_since, first.turns[0].settled_at,
        "the background command left running keeps the Session Monitoring from the Turn's settle"
    );
    assert_eq!(
        listed(&opened.client, opened.session_id)
            .await
            .session
            .monitoring_since,
        first.turns[0].settled_at,
        "the listing reads Monitoring as the Session does"
    );
    claude.release();

    let mut feed = opened
        .client
        .subscribe_session(opened.session_id)
        .await
        .expect("subscribe to Session SSE");
    let woken = session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "the command's completion wakes Claude into a Continuation",
        |snapshot| snapshot.turns.len() == 2,
    )
    .await;
    assert_eq!(woken.turns[1].status, TurnStatus::Active);
    assert_eq!(
        woken.session.monitoring_since, None,
        "the Watch that woke the Agent is settled, and the Session is Working again"
    );
    assert_eq!(
        woken.session.working_since, woken.turns[1].started_at,
        "Working after the wake counts from the Continuation's start"
    );
    assert!(
        woken.session.working_since > first.turns[0].started_at,
        "Working does not carry on from the Turn before the wake"
    );
    let listed_woken = listed(&opened.client, opened.session_id).await;
    assert_eq!(listed_woken.session.monitoring_since, None);
    assert_eq!(
        listed_woken.session.working_since,
        woken.turns[1].started_at
    );
    claude.release_gate("continuation");

    let resumed = settled_session(&opened.client, opened.session_id, 1).await;
    assert_eq!(resumed.turns[1].prompt_id, None);
    assert_eq!(resumed.turns[1].status, TurnStatus::Completed);
    assert_eq!(agent_messages(&resumed)[0].content, "Tests passed.");
    assert!(resumed.activities.iter().any(|activity| matches!(activity,
        Activity::Reasoning { turn_id, status: ActivityStatus::Completed, .. }
            if *turn_id == resumed.turns[1].id
    )));
    assert_eq!(resumed.session.working_since, None);
    assert_eq!(
        resumed.session.monitoring_since, None,
        "with its only Watch settled, the Session is neither Working nor Monitoring"
    );
    opened.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_prompt_during_resumed_reasoning_waits_for_the_native_interrupt_boundary() {
    let claude = ScriptedClaude::new(&format!(
        "{}{}{}",
        discovery_arms(CLAUDE_MODELS),
        interrupt_arm(
            r#"
      interrupted=yes
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0}}'
      emit '{"type":"result","subtype":"success","terminal_reason":"aborted_streaming"}'
"#
        ),
        user_turn_arm(
            r#"
      prompts=$(( ${prompts:-0} + 1 ))
      if [ "$prompts" -eq 1 ]; then
        emit '{"type":"system","subtype":"task_started","task_id":"tests","task_type":"local_bash"}'
        emit '{"type":"result","subtype":"success","result":"Waiting for tests."}'
        while [ ! -e "$CLAUDE_FIXTURE_RELEASE" ]; do sleep 0.01; done
        emit '{"type":"system","subtype":"task_notification","task_id":"tests","status":"completed"}'
        emit '{"type":"stream_event","event":{"type":"message_start"}}'
        emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}}'
      elif [ "${interrupted:-no}" = yes ]; then
        emit '{"type":"stream_event","event":{"type":"message_start"}}'
        emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Tests passed."}}}'
        emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0}}'
        emit '{"type":"result","subtype":"success","result":"Tests passed."}'
      else
        emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0}}'
        emit '{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["Prompt crossed the running native loop"]}'
      fi
"#
        ),
    ));
    let opened = opened_session(&claude, "claude-prompt-continuation", "Run tests").await;
    settled_session(&opened.client, opened.session_id, 0).await;
    let mut feed = opened
        .client
        .subscribe_session(opened.session_id)
        .await
        .expect("subscribe");
    claude.release();
    let resumed = session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "resumed Reasoning is visible",
        |snapshot| {
            snapshot.activities.iter().any(|activity| {
                matches!(
                    activity,
                    Activity::Reasoning {
                        status: ActivityStatus::Active,
                        ..
                    }
                )
            })
        },
    )
    .await;
    assert_eq!(resumed.turns[1].prompt_id, None);

    let prompt_id = PromptId::new();
    opened
        .client
        .admit_prompt(
            opened.session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: prompt_id,
                    text: "How did the run go?".to_owned(),
                    skill_invocations: Vec::new(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("prompt during the Continuation");
    let answered = settled_session(&opened.client, opened.session_id, 2).await;
    assert_eq!(answered.turns[1].status, TurnStatus::Interrupted);
    assert_eq!(answered.turns[2].prompt_id, Some(prompt_id));
    assert_eq!(answered.turns[2].status, TurnStatus::Completed);
    assert_eq!(agent_messages(&answered)[0].content, "Tests passed.");
    assert!(
        !answered
            .activities
            .iter()
            .any(|activity| matches!(activity, Activity::Error { .. }))
    );
    opened.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn buffered_native_work_cannot_complete_a_queued_prompt() {
    let claude = conversation_fixture(
        r#"
      prompts=$(( ${prompts:-0} + 1 ))
      if [ "$prompts" -eq 1 ]; then
        emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Waiting for tests."}}}'
        emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0}}'
        while [ ! -e "$CLAUDE_FIXTURE_RELEASE" ]; do sleep 0.01; done
        emit '{"type":"result","subtype":"success","result":"Waiting for tests."}
{"type":"stream_event","event":{"type":"message_start"}}
{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Background result."}}}
{"type":"stream_event","event":{"type":"content_block_stop","index":0}}
{"type":"result","subtype":"success","result":"Background result."}'
      else
        emit '{"type":"stream_event","event":{"type":"message_start"}}'
        emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Answer to queued prompt."}}}'
        emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0}}'
        emit '{"type":"result","subtype":"success","result":"Answer to queued prompt."}'
      fi
"#,
    );
    let opened = opened_session(&claude, "claude-queued-continuation", "Run tests").await;
    let mut feed = opened
        .client
        .subscribe_session(opened.session_id)
        .await
        .expect("subscribe");
    session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "initial answer arrives",
        |snapshot| !agent_messages(snapshot).is_empty(),
    )
    .await;
    let prompt_id = PromptId::new();
    opened
        .client
        .admit_prompt(
            opened.session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: prompt_id,
                    text: "Next question".to_owned(),
                    skill_invocations: Vec::new(),
                },
                delivery: PromptDelivery::Queue,
            },
        )
        .await
        .expect("queue the next Prompt");
    claude.release();
    let answered = session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "queued answer arrives",
        |snapshot| {
            agent_messages(snapshot)
                .iter()
                .any(|message| message.content == "Answer to queued prompt.")
        },
    )
    .await;
    let prompt_turn = answered
        .turns
        .iter()
        .find(|turn| turn.prompt_id == Some(prompt_id))
        .expect("queued Prompt has a Turn");
    let answer = agent_messages(&answered)
        .into_iter()
        .find(|message| message.content == "Answer to queued prompt.")
        .expect("answer");
    assert_eq!(
        answer.turn_id, prompt_turn.id,
        "only the queued Prompt's response belongs to its Turn"
    );
    let background = agent_messages(&answered)
        .into_iter()
        .find(|message| message.content == "Background result.")
        .expect("background work is visible");
    assert_ne!(background.turn_id, prompt_turn.id);
    opened.server.shutdown().await.expect("shut down server");
}
