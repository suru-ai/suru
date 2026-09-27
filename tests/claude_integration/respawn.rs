//! Stopping work after an Agent Selection change has respawned the CLI.
//!
//! The Model and its Options are spawn-time flags, so a Turn under another Selection runs on a new
//! child resuming the same conversation. The old child's background work dies with its process
//! group, and the CLI keeps its task roster per process and announces nothing it inherited — so a
//! stop after the respawn is for the new child's own work alone, whether the interrupt asks for it
//! or a Subagent's Session does.

use crate::support::{
    CLAUDE_MODELS, ScriptedClaude, agent_messages, discovery_arms, interrupt_arm, opened_session,
    session_where, settled_session, stop_task_arm, user_turn_arm,
};
use suru::{
    managed_client::ManagedClient,
    protocol::{
        Activity, ActivityStatus, AdmitPromptRequest, AgentSelection, AgentSelectionOperationId,
        InitialPrompt, ModelId, ModelOptionChoiceId, ModelOptionId, ModelOptionSelection,
        ModelOptionValue, PromptDelivery, PromptId, ProviderId, SessionId, SessionSnapshot,
        TurnStatus, UpdateAgentSelectionRequest,
    },
};

/// A Turn on the first child that leaves a command running in the background and completes, the
/// way a Turn kicking off a long test run does: `task-old` is on that child's roster, and nothing
/// will ever report it settling.
const BACKGROUND_COMMAND_LEFT_RUNNING: &str = r#"      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_old","name":"Bash","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"cargo test\",\"run_in_background\":true}"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"task-old","tool_use_id":"toolu_old","description":"cargo test","is_backgrounded":true,"task_type":"local_bash","session_id":"prov-session"}'
      emit '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_old","content":"Command running in background with ID: task-old","is_error":false}]},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":40,"num_turns":1,"result":"Tests are running.","session_id":"prov-session"}'
"#;

/// A Turn on the respawned child that streams an answer and runs on until it is interrupted.
const STREAMING_WITH_NOTHING_IN_THE_BACKGROUND: &str = r#"      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Halfway"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
"#;

/// The same Turn with a command of its own running in the background, reported the way the first
/// child reported `task-old`.
const STREAMING_WITH_NEW_WORK_IN_THE_BACKGROUND: &str = r#"      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_new","name":"Bash","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"sleep 600\",\"run_in_background\":true}"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"task-new","tool_use_id":"toolu_new","description":"sleep 600","is_backgrounded":true,"task_type":"local_bash","session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"text","text":"Halfway"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
"#;

/// A Turn on the respawned child that spawns a Subagent and completes at once, leaving the
/// Subagent working.
const SUBAGENT_OUTLIVES_THE_RESPAWNED_TURN: &str = r#"      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"task_new","name":"Task","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"agent-task-new","tool_use_id":"task_new","description":"Audit dependencies","task_type":"local_agent","subagent_type":"general-purpose","session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":400,"num_turns":1,"result":"Kicked off the audit.","session_id":"prov-session"}'
"#;

/// A Turn on the first child that spawns a Subagent in the background and completes at once,
/// leaving it working when the child is replaced.
const SUBAGENT_OUTLIVES_THE_FIRST_TURN: &str = r#"      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"agent_1","name":"Agent","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"description\":\"Audit dependencies\",\"prompt\":\"Audit the dependencies.\",\"run_in_background\":true}"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"agent-task-1","tool_use_id":"agent_1","description":"Audit dependencies","task_type":"local_agent","subagent_type":"general-purpose","session_id":"prov-session"}'
      emit '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"agent_1","content":"Async agent launched successfully.","is_error":false}]},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":300,"num_turns":1,"result":"Launched.","session_id":"prov-session"}'
"#;

/// A Turn on the respawned child that sends that same agent more through SendMessage, which starts
/// its task again under the same id — on the new process this time — and completes, leaving it
/// working.
const RESPAWNED_TURN_RESUMES_THE_SUBAGENT: &str = r#"      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"send_1","name":"SendMessage","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"to\":\"agent-task-1\",\"message\":\"Carry on with the audit.\",\"summary\":\"Carry on\"}"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"agent-task-1","tool_use_id":"send_1","description":"Audit dependencies","task_type":"local_agent","subagent_type":"general-purpose","session_id":"prov-session"}'
      emit '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"send_1","content":"{\"success\":true,\"message\":\"Resuming agent agent-task-1\"}","is_error":false}]},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":300,"num_turns":1,"result":"Resumed the audit.","session_id":"prov-session"}'
"#;

/// A Turn on the respawned child that answers and completes, leaving nothing running.
const RESPAWNED_TURN_COMPLETES: &str = r#"      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Carried on."}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":40,"num_turns":1,"result":"Carried on.","session_id":"prov-session"}'
"#;

/// What the CLI writes once the interrupt has stopped its loop.
const ABORTED_RESULT: &str = r#"      emit '{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"result","subtype":"error_during_execution","is_error":true,"duration_ms":11,"num_turns":1,"errors":["[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=null"],"terminal_reason":"aborted_streaming","session_id":"prov-session"}'
"#;

/// The Selection the second Turn runs under — different spawn flags from the default the first
/// Turn ran under, and so a child of its own.
fn another_selection() -> AgentSelection {
    AgentSelection {
        provider: ProviderId::new("claude"),
        model: ModelId::new("middling"),
        options: vec![ModelOptionSelection {
            id: ModelOptionId::new("reasoning_effort"),
            value: ModelOptionValue::Select {
                choice: ModelOptionChoiceId::new("low"),
            },
        }],
    }
}

/// A fixture whose Session children play `first` for the Prompt the first child receives and
/// `respawned` for any a child resuming the conversation receives, stopping every task it is
/// asked to and interrupting its loop into an aborted result.
fn respawning_fixture(first: &str, respawned: &str) -> ScriptedClaude {
    ScriptedClaude::new(&format!(
        "{}{}{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(&format!(
            "      case \"$*\" in\n      *--resume*)\n{respawned}        ;;\n      *)\n{first}        ;;\n      esac\n"
        )),
        stop_task_arm(),
        interrupt_arm(ABORTED_RESULT),
    ))
}

/// Settles the Session's first Turn, then puts [`another_selection`] in force and delivers the
/// second Prompt under it — which the runtime runs on a respawned child.
async fn prompt_the_respawned_child(client: &ManagedClient, session_id: SessionId) {
    let first = settled_session(client, session_id, 0).await;
    assert_eq!(
        first.turns[0].status,
        TurnStatus::Completed,
        "the first child's Turn completes, leaving its background work behind: {:?}",
        first.activities
    );
    // The Agent Selection is normalized against the catalog, so discover it before naming one.
    client.list_models().await.expect("discover Claude Models");
    client
        .update_agent_selection(
            session_id,
            UpdateAgentSelectionRequest {
                operation_id: AgentSelectionOperationId::new(),
                selection: another_selection(),
            },
        )
        .await
        .expect("choose another Agent Selection between Turns");
    client
        .admit_prompt(
            session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Carry on under another Model".to_owned(),
                    skill_invocations: Vec::new(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("admit a Prompt under the new Selection");
}

/// The Session once the respawned child's Turn is streaming its answer.
async fn respawned_turn_streaming(
    client: &ManagedClient,
    session_id: SessionId,
) -> SessionSnapshot {
    let mut feed = client
        .subscribe_session(session_id)
        .await
        .expect("subscribe to Session SSE");
    session_where(
        client,
        &mut feed,
        session_id,
        "the respawned child's Turn streams its answer",
        |snapshot| {
            snapshot
                .turns
                .get(1)
                .is_some_and(|turn| turn.status == TurnStatus::Active)
                && agent_messages(snapshot)
                    .iter()
                    .any(|message| message.content == "Halfway")
        },
    )
    .await
}

/// The task ids of every `stop_task` the fixture received, in the order they arrived — from any
/// child, since the first one was never asked to stop anything.
fn stopped_tasks(claude: &ScriptedClaude) -> Vec<String> {
    claude
        .requests()
        .into_iter()
        .filter(|request| request.pointer("/request/subtype") == Some(&"stop_task".into()))
        .map(|request| {
            request
                .pointer("/request/task_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_else(|| panic!("a stop names the task it stops: {request}"))
                .to_owned()
        })
        .collect()
}

/// The control-request subtypes the fixture received from the respawned child's Turn onwards —
/// everything after the last `list_models` the Selection change's discovery asked for.
fn subtypes_after_the_respawn(claude: &ScriptedClaude) -> Vec<String> {
    let subtypes = claude
        .control_subtypes()
        .into_iter()
        .filter(|subtype| subtype != "get_context_usage")
        .collect::<Vec<_>>();
    let discovered = subtypes
        .iter()
        .rposition(|subtype| subtype == "list_models")
        .expect("the Selection change was normalized against a discovered catalog");
    subtypes[discovered + 1..].to_vec()
}

fn assert_second_child_resumed(claude: &ScriptedClaude) {
    let resumed = claude.launch_carrying("--resume");
    assert_eq!(
        resumed.value("--model"),
        "middling",
        "the second Turn ran on a child spawned under the new Selection: {resumed:?}"
    );
}

#[tokio::test]
async fn an_interrupt_after_a_selection_change_asks_the_respawned_cli_to_stop_none_of_the_old_tasks()
 {
    let claude = respawning_fixture(
        BACKGROUND_COMMAND_LEFT_RUNNING,
        STREAMING_WITH_NOTHING_IN_THE_BACKGROUND,
    );
    let opened = opened_session(
        &claude,
        "claude-respawn-forgets-old-tasks",
        "Run the tests in the background",
    )
    .await;
    let (client, session_id) = (&opened.client, opened.session_id);
    prompt_the_respawned_child(client, session_id).await;
    respawned_turn_streaming(client, session_id).await;

    client
        .interrupt_session(session_id)
        .await
        .expect("the respawned CLI acknowledges the interrupt");
    let interrupted = settled_session(client, session_id, 1).await;

    assert_eq!(interrupted.turns[1].status, TurnStatus::Interrupted);
    assert_second_child_resumed(&claude);
    assert_eq!(
        stopped_tasks(&claude),
        Vec::<String>::new(),
        "`task-old` died with the child that ran it, so the respawned CLI is never asked to stop it"
    );
    assert_eq!(
        subtypes_after_the_respawn(&claude),
        ["interrupt"],
        "a respawned child running nothing in the background is stopped by the interrupt alone"
    );

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn background_work_the_respawned_cli_starts_is_still_stopped_before_its_loop() {
    let claude = respawning_fixture(
        BACKGROUND_COMMAND_LEFT_RUNNING,
        STREAMING_WITH_NEW_WORK_IN_THE_BACKGROUND,
    );
    let opened = opened_session(
        &claude,
        "claude-respawn-stops-new-tasks",
        "Run the tests in the background",
    )
    .await;
    let (client, session_id) = (&opened.client, opened.session_id);
    prompt_the_respawned_child(client, session_id).await;
    respawned_turn_streaming(client, session_id).await;

    client
        .interrupt_session(session_id)
        .await
        .expect("the respawned CLI acknowledges the interrupt");
    let interrupted = settled_session(client, session_id, 1).await;

    assert_eq!(interrupted.turns[1].status, TurnStatus::Interrupted);
    assert_second_child_resumed(&claude);
    assert_eq!(
        stopped_tasks(&claude),
        ["task-new"],
        "the respawned CLI is asked to stop the work it reported, and only that"
    );
    assert_eq!(
        subtypes_after_the_respawn(&claude),
        ["stop_task", "interrupt"],
        "the respawned child's background work is stopped before its loop is"
    );

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn stopping_a_subagent_the_respawned_cli_spawned_by_its_session_stops_only_its_task() {
    let claude = respawning_fixture(
        BACKGROUND_COMMAND_LEFT_RUNNING,
        SUBAGENT_OUTLIVES_THE_RESPAWNED_TURN,
    );
    let opened = opened_session(
        &claude,
        "claude-respawn-stops-new-subagent",
        "Run the tests in the background",
    )
    .await;
    let (client, session_id) = (&opened.client, opened.session_id);
    prompt_the_respawned_child(client, session_id).await;
    let settled = settled_session(client, session_id, 1).await;
    assert_eq!(settled.turns[1].status, TurnStatus::Completed);
    let Some(Activity::Subagent {
        session_id: child_id,
        status: ActivityStatus::Active,
        ..
    }) = settled
        .activities
        .iter()
        .find(|activity| matches!(activity, Activity::Subagent { .. }))
    else {
        panic!(
            "the respawned child's Subagent works on past its Turn, got {:?}",
            settled.activities
        );
    };
    let child_id = *child_id;

    // Interrupting the Subagent's own Session is the Picker row's stop.
    client
        .interrupt_session(child_id)
        .await
        .expect("the Subagent's Session accepts the stop");
    let stopped_child = settled_session(client, child_id, 0).await;

    assert_eq!(stopped_child.turns[0].status, TurnStatus::Interrupted);
    assert_second_child_resumed(&claude);
    assert_eq!(
        stopped_tasks(&claude),
        ["agent-task-new"],
        "the stop names the respawned CLI's task running the Subagent, and nothing the first \
         child ran"
    );
    assert_eq!(
        subtypes_after_the_respawn(&claude),
        ["stop_task"],
        "one stop goes out, and no interrupt follows it"
    );

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn stopping_a_subagent_the_respawned_cli_resumed_by_its_session_stops_its_task_there() {
    let claude = respawning_fixture(
        SUBAGENT_OUTLIVES_THE_FIRST_TURN,
        RESPAWNED_TURN_RESUMES_THE_SUBAGENT,
    );
    let opened = opened_session(
        &claude,
        "claude-respawn-stops-resumed-subagent",
        "Audit the dependencies in the background",
    )
    .await;
    let (client, session_id) = (&opened.client, opened.session_id);
    prompt_the_respawned_child(client, session_id).await;
    let settled = settled_session(client, session_id, 1).await;
    assert_eq!(settled.turns[1].status, TurnStatus::Completed);
    let Some(Activity::Subagent {
        session_id: child_id,
        ..
    }) = settled
        .activities
        .iter()
        .find(|activity| matches!(activity, Activity::Subagent { .. }))
    else {
        panic!(
            "the first child's Turn spawned a Subagent, got {:?}",
            settled.activities
        );
    };
    let child_id = *child_id;

    client
        .interrupt_session(child_id)
        .await
        .expect("the Subagent's Session accepts the stop");
    settled_session(client, child_id, 0).await;

    assert_second_child_resumed(&claude);
    assert_eq!(
        stopped_tasks(&claude),
        ["agent-task-1"],
        "the Subagent's task runs on the respawned CLI now, which is asked to stop it"
    );

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn a_respawn_for_a_selection_change_ends_monitoring_because_the_old_childs_watches_are_lost()
{
    let claude = respawning_fixture(BACKGROUND_COMMAND_LEFT_RUNNING, RESPAWNED_TURN_COMPLETES);
    let opened = opened_session(
        &claude,
        "claude-respawn-loses-watches",
        "Run the tests in the background",
    )
    .await;
    let (client, session_id) = (&opened.client, opened.session_id);
    let first = settled_session(client, session_id, 0).await;
    assert_eq!(
        first.session.monitoring_since, first.turns[0].settled_at,
        "`task-old` left running keeps the Session Monitoring once its Turn settles"
    );

    prompt_the_respawned_child(client, session_id).await;
    let second = settled_session(client, session_id, 1).await;

    assert_eq!(second.turns[1].status, TurnStatus::Completed);
    assert_second_child_resumed(&claude);
    assert_eq!(second.session.working_since, None);
    assert_eq!(
        second.session.monitoring_since, None,
        "`task-old` died with the child that ran it, so it is settled as lost and nothing is \
         left for the Session to wait on"
    );
    let listed = client
        .list_sessions(None)
        .await
        .expect("list Sessions")
        .into_iter()
        .find_map(|item| {
            item.readable()
                .filter(|summary| summary.session.id == session_id)
                .cloned()
        })
        .expect("the Session is listed and readable");
    assert_eq!(listed.session.monitoring_since, None);

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}
