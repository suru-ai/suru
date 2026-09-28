//! Native work resumed by Claude after background commands finish.

use crate::support::{
    CLAUDE_MODELS, ScriptedClaude, agent_messages, conversation_fixture, discovery_arms,
    interrupt_arm, opened_session, session_where, settled_session, user_turn_arm,
};
use suru::{
    managed_client::ManagedClient,
    protocol::{
        Activity, ActivityStatus, AdmitPromptRequest, InitialPrompt, PromptDelivery, PromptId,
        SessionId, SessionSnapshot, SessionSummary, TranscriptItem, TurnId, TurnStatus,
        WatchOutcomeStatus,
    },
};

/// A Turn that leaves a background command running and ends its loop, then — once released — the
/// command settling as `status` with `summary` and waking the loop into a Continuation, which
/// holds at its first message until the `continuation` gate is released too.
fn background_command(status: &str, summary: &str) -> String {
    format!(
        r#"
      emit '{{"type":"system","subtype":"task_started","task_id":"tests","task_type":"local_bash","description":"Run cargo test"}}'
      emit '{{"type":"result","subtype":"success","is_error":false,"result":"Waiting for tests."}}'
      while [ ! -e "$CLAUDE_FIXTURE_RELEASE" ]; do sleep 0.01; done
      emit '{{"type":"system","subtype":"task_notification","task_id":"tests","status":"{status}","summary":"{summary}"}}'
      emit '{{"type":"stream_event","event":{{"type":"message_start"}}}}'
      while [ ! -e "$CLAUDE_FIXTURE_RELEASE-continuation" ]; do sleep 0.01; done
      emit '{{"type":"stream_event","event":{{"type":"content_block_start","index":0,"content_block":{{"type":"thinking","thinking":""}}}}}}'
      emit '{{"type":"stream_event","event":{{"type":"content_block_stop","index":0}}}}'
      emit '{{"type":"stream_event","event":{{"type":"content_block_start","index":1,"content_block":{{"type":"text","text":"Tests ran."}}}}}}'
      emit '{{"type":"stream_event","event":{{"type":"content_block_stop","index":1}}}}'
      emit '{{"type":"result","subtype":"success","is_error":false,"result":"Tests ran."}}'
"#
    )
}

/// The background command's completion as the CLI summarizes it.
const COMPLETED_SUMMARY: &str = r#"Background command \"cargo test\" completed (exit code 0)"#;

/// The Transcript entries of one Turn in presentation order: the Activity at the head of a
/// Continuation is the first entry the reader meets in it.
fn turn_entries(snapshot: &SessionSnapshot, turn_id: TurnId) -> Vec<TranscriptItem> {
    snapshot
        .transcript
        .iter()
        .copied()
        .filter(|item| match item {
            TranscriptItem::Message { message_id } => snapshot
                .messages
                .iter()
                .any(|message| message.id == *message_id && message.turn_id == turn_id),
            TranscriptItem::Activity { activity_id } => snapshot
                .activities
                .iter()
                .any(|activity| activity.id() == *activity_id && activity.turn_id() == turn_id),
        })
        .collect()
}

/// The Watch Outcome heading the Continuation at `turn_index`, which must be the Turn's first
/// Transcript entry.
pub(crate) fn heading_watch_outcome(snapshot: &SessionSnapshot, turn_index: usize) -> &Activity {
    let turn_id = snapshot.turns[turn_index].id;
    let Some(TranscriptItem::Activity { activity_id }) =
        turn_entries(snapshot, turn_id).first().copied()
    else {
        panic!("the Continuation begins with an Activity: {snapshot:#?}");
    };
    let activity = snapshot
        .activities
        .iter()
        .find(|activity| activity.id() == activity_id)
        .expect("the heading Activity is in the Session");
    assert!(
        matches!(activity, Activity::WatchOutcome { .. }),
        "the Continuation begins with a Watch Outcome, not {activity:?}"
    );
    activity
}

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
    let claude = conversation_fixture(&background_command("completed", COMPLETED_SUMMARY));
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
        heading_watch_outcome(&woken, 1),
        &Activity::WatchOutcome {
            id: heading_watch_outcome(&woken, 1).id(),
            turn_id: woken.turns[1].id,
            status: WatchOutcomeStatus::Completed,
            description: "Run cargo test".to_owned(),
            summary: Some(r#"Background command "cargo test" completed (exit code 0)"#.to_owned()),
        },
        "the Continuation the wake began opens with how the Watch settled, in Claude's words"
    );
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
    assert_eq!(agent_messages(&resumed)[0].content, "Tests ran.");
    assert!(resumed.activities.iter().any(|activity| matches!(activity,
        Activity::Reasoning { turn_id, status: ActivityStatus::Completed, .. }
            if *turn_id == resumed.turns[1].id
    )));
    let outcome = heading_watch_outcome(&resumed, 1).id();
    let continuation = turn_entries(&resumed, resumed.turns[1].id);
    assert!(
        continuation.len() > 2,
        "the Continuation's Reasoning and Message follow its Watch Outcome: {continuation:?}"
    );
    assert_eq!(
        resumed
            .activities
            .iter()
            .filter(|activity| matches!(activity, Activity::WatchOutcome { .. }))
            .map(Activity::id)
            .collect::<Vec<_>>(),
        vec![outcome],
        "the one Watch that woke the Agent records one Watch Outcome"
    );
    assert_eq!(resumed.session.working_since, None);
    assert_eq!(
        resumed.session.monitoring_since, None,
        "with its only Watch settled, the Session is neither Working nor Monitoring"
    );
    opened.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_failed_background_command_heads_its_continuation_with_a_failed_watch_outcome() {
    let claude = conversation_fixture(&background_command(
        "failed",
        r#"Background command \"cargo test\" failed with exit code 1"#,
    ));
    let opened = opened_session(&claude, "claude-failed-watch-outcome", "Run tests").await;
    settled_session(&opened.client, opened.session_id, 0).await;
    claude.release();
    claude.release_gate("continuation");

    let resumed = settled_session(&opened.client, opened.session_id, 1).await;
    assert_eq!(resumed.turns[1].prompt_id, None);
    let Activity::WatchOutcome {
        turn_id,
        status,
        description,
        summary,
        ..
    } = heading_watch_outcome(&resumed, 1)
    else {
        unreachable!("the heading Activity is a Watch Outcome");
    };
    assert_eq!(*turn_id, resumed.turns[1].id);
    assert_eq!(
        *status,
        WatchOutcomeStatus::Failed,
        "the failed command's Watch Outcome shows it failed"
    );
    assert_eq!(description, "Run cargo test");
    assert_eq!(
        summary.as_deref(),
        Some(r#"Background command "cargo test" failed with exit code 1"#)
    );
    assert!(
        !resumed.activities.iter().any(|activity| matches!(
            activity,
            Activity::WatchOutcome { turn_id, .. } if *turn_id == resumed.turns[0].id
        )),
        "the Turn that left the command running settled before it failed"
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
      emit '{"type":"result","subtype":"error_during_execution","is_error":true,"terminal_reason":"aborted_streaming"}'
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
                    attachments: Vec::new(),
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
                    attachments: Vec::new(),
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
