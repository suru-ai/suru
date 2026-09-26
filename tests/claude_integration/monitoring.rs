//! Which of Claude's background tasks keep a Session Monitoring once its Turn has settled (ADR
//! 0030): the ones whose settling the CLI delivers to the agent — a background shell, a Monitor, a
//! monitor over MCP or a WebSocket. An agent task is a Subagent, which keeps its Session Working
//! instead, and any other task is none of the Session's to wait on. Interrupting a Session that is
//! only Monitoring stops those tasks one by one, and never interrupts a loop that is not running.
//! A task a subagent launched is that Subagent's Watch, which outlives it and keeps its own Session
//! Monitoring as well as the Sessions above it; interrupting that Session stops its Watches alone.

use crate::support::{
    CLAUDE_MODELS, ScriptedClaude, after_probe, conversation_fixture, discovery_arms,
    opened_session, session_where, settled_session, stop_task_arm, user_turn_arm,
};
use suru::protocol::{
    Activity, InterruptOutcome, SessionId, SessionSnapshot, SessionStatus, TurnStatus,
};

/// A Turn that starts one task of `task_type` in the background and ends its loop, leaving the
/// task running.
fn task_left_running(task_type: &str, description: &str) -> String {
    format!(
        r#"
      emit '{{"type":"system","subtype":"task_started","task_id":"task-1","description":"{description}","task_type":"{task_type}"}}'
      emit '{{"type":"result","subtype":"success","is_error":false,"result":"Left it running."}}'
"#
    )
}

/// The Session once the Turn that started `timeline`'s task has settled.
async fn settled_after(timeline: &str, channel: &'static str) -> SessionSnapshot {
    let claude = conversation_fixture(timeline);
    let opened = opened_session(&claude, channel, "Keep an eye on it").await;
    let settled = settled_session(&opened.client, opened.session_id, 0).await;
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    opened.server.shutdown().await.expect("shut down server");
    settled
}

fn assert_monitoring(settled: &SessionSnapshot, task: &str) {
    assert_eq!(settled.session.working_since, None);
    assert_eq!(
        settled.session.monitoring_since, settled.turns[0].settled_at,
        "{task} left running keeps the Session Monitoring from the Turn's settle"
    );
}

#[tokio::test]
async fn a_monitor_tool_task_keeps_its_session_monitoring() {
    // The Monitor tool runs its script as a background shell, which the CLI reports as the same
    // `local_bash` task a backgrounded Bash command is.
    let timeline = format!(
        r#"
      emit '{{"type":"stream_event","event":{{"type":"content_block_start","index":0,"content_block":{{"type":"tool_use","id":"toolu_monitor","name":"Monitor","input":{{}}}}}},"parent_tool_use_id":null}}'
      emit '{{"type":"stream_event","event":{{"type":"content_block_stop","index":0}},"parent_tool_use_id":null}}'
{}"#,
        task_left_running("local_bash", "Watch the deploy log for errors")
    );
    let settled = settled_after(&timeline, "claude-monitor-tool-monitoring").await;
    assert_monitoring(&settled, "a Monitor");
}

#[tokio::test]
async fn an_mcp_monitor_task_keeps_its_session_monitoring() {
    let settled = settled_after(
        &task_left_running("monitor_mcp", "Watch the build queue"),
        "claude-mcp-monitor-monitoring",
    )
    .await;
    assert_monitoring(&settled, "an MCP monitor");
}

#[tokio::test]
async fn a_websocket_monitor_task_keeps_its_session_monitoring() {
    let settled = settled_after(
        &task_left_running("monitor_ws", "Watch the deploy stream"),
        "claude-websocket-monitor-monitoring",
    )
    .await;
    assert_monitoring(&settled, "a WebSocket monitor");
}

#[tokio::test]
async fn a_background_agent_keeps_its_session_working_as_a_subagent_rather_than_monitoring() {
    let timeline = r#"
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"agent_1","name":"Agent","input":{}}},"parent_tool_use_id":null}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null}'
      emit '{"type":"system","subtype":"task_started","task_id":"agent-task-1","tool_use_id":"agent_1","description":"Audit dependencies","task_type":"local_agent","subagent_type":"general-purpose"}'
      emit '{"type":"result","subtype":"success","is_error":false,"result":"Launched."}'
"#;
    let settled = settled_after(timeline, "claude-background-agent-working").await;
    assert!(
        settled.session.working_since.is_some(),
        "a Subagent outliving the Turn keeps the Session Working"
    );
    assert_eq!(
        settled.session.monitoring_since, None,
        "a Subagent is never a Watch"
    );
}

#[tokio::test]
async fn a_task_that_cannot_wake_the_agent_leaves_its_session_neither_working_nor_monitoring() {
    let timeline = [
        "dream",
        "local_workflow",
        "remote_agent",
        "in_process_teammate",
        "a_task_type_this_build_has_never_heard_of",
    ]
    .iter()
    .enumerate()
    .map(|(index, task_type)| {
        format!(
            "      emit '{{\"type\":\"system\",\"subtype\":\"task_started\",\"task_id\":\"task-{index}\",\"description\":\"Background work\",\"task_type\":\"{task_type}\"}}'\n"
        )
    })
    .chain([
        "      emit '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"Done.\"}'\n"
            .to_owned(),
    ])
    .collect::<String>();
    let settled = settled_after(&timeline, "claude-unwatched-tasks").await;
    assert_eq!(settled.session.working_since, None);
    assert_eq!(
        settled.session.monitoring_since, None,
        "none of these tasks is a Watch, so nothing is left for the Session to wait on"
    );
}

/// A Turn that leaves two background shells running — a test run and a log tail — and ends its
/// loop, the way a live CLI reports each: the Bash tool use, then the task it started.
const TWO_WATCHES_LEFT_RUNNING: &str = r#"      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_tests","name":"Bash","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"cargo test\",\"run_in_background\":true}"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"task-tests","tool_use_id":"toolu_tests","description":"cargo test","task_type":"local_bash","session_id":"prov-session"}'
      emit '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_tests","content":"Command running in background with ID: task-tests"}]},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"task-log","description":"tail -f server.log","task_type":"local_bash","session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":30,"num_turns":1,"result":"Left them running.","session_id":"prov-session"}'
"#;

/// A `stop_task` arm that acknowledges the stop and then, as a live CLI does, notifies that the
/// task it named stopped.
fn stop_task_then_notify_arm() -> String {
    r#"    *'"subtype":"stop_task"'*)
      task_id=$(printf '%s' "$line" | sed -n 's/.*"task_id":"\([^"]*\)".*/\1/p')
      printf '%s\n' '{"type":"control_response","response":{"subtype":"success","request_id":"'"$request_id"'","response":{}}}'
      emit '{"type":"system","subtype":"task_notification","task_id":"'"$task_id"'","status":"stopped","summary":"Background command stopped","session_id":"prov-session"}'
      ;;
"#
    .to_owned()
}

/// Interrupts the Session `claude` left Monitoring on two Watches, and answers the Session once it
/// has stopped Monitoring, beside the one it was before the interrupt.
async fn interrupted_while_monitoring(
    claude: &ScriptedClaude,
    channel: &'static str,
) -> (SessionSnapshot, SessionSnapshot) {
    let opened = opened_session(claude, channel, "Run the tests and tail the log").await;
    let (client, session_id) = (&opened.client, opened.session_id);
    let monitoring = settled_session(client, session_id, 0).await;
    assert_eq!(monitoring.turns[0].status, TurnStatus::Completed);
    assert!(monitoring.session.monitoring_since.is_some());
    let mut described = monitoring
        .watches
        .iter()
        .map(|watch| watch.description.as_str())
        .collect::<Vec<_>>();
    described.sort_unstable();
    assert_eq!(described, ["cargo test", "tail -f server.log"]);

    let mut feed = client
        .subscribe_session(session_id)
        .await
        .expect("subscribe to the Session");
    let outcome = client
        .interrupt_session_reporting_outcome(session_id)
        .await
        .expect("the interrupt of a Monitoring Session succeeds");
    assert_eq!(outcome, InterruptOutcome::StoppedWork);
    let idle = session_where(
        client,
        &mut feed,
        session_id,
        "the stopped Watches end Monitoring",
        |snapshot| snapshot.session.monitoring_since.is_none(),
    )
    .await;
    opened.server.shutdown().await.expect("shut down server");
    (monitoring, idle)
}

fn assert_stopped_each_watch_and_interrupted_nothing(claude: &ScriptedClaude) {
    assert_eq!(
        claude
            .control_subtypes()
            .into_iter()
            .filter(|subtype| subtype != "get_context_usage")
            .collect::<Vec<_>>(),
        after_probe(["list_models", "stop_task", "stop_task"]),
        "one stop per live Watch, and no loop interrupt: the Session was running no loop"
    );
    let mut stopped = claude
        .requests()
        .into_iter()
        .filter(|request| request.pointer("/request/subtype") == Some(&"stop_task".into()))
        .filter_map(|request| {
            request
                .pointer("/request/task_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    stopped.sort_unstable();
    assert_eq!(stopped, ["task-log", "task-tests"]);
}

fn assert_idle_and_unchanged(monitoring: &SessionSnapshot, idle: &SessionSnapshot) {
    assert_eq!(idle.session.status, SessionStatus::Idle);
    assert_eq!(idle.session.working_since, None);
    assert!(idle.watches.is_empty(), "{:?}", idle.watches);
    assert_eq!(
        idle.turns, monitoring.turns,
        "stopping Watches settles no Turn and begins none"
    );
    assert_eq!(
        idle.transcript, monitoring.transcript,
        "a stopped Watch wakes nothing, so the Transcript is unchanged"
    );
    assert_eq!(idle.messages, monitoring.messages);
    assert_eq!(idle.activities, monitoring.activities);
}

#[tokio::test]
async fn escape_on_a_monitoring_session_stops_each_watch_and_interrupts_no_loop() {
    let claude = ScriptedClaude::new(&format!(
        "{}{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(TWO_WATCHES_LEFT_RUNNING),
        stop_task_then_notify_arm(),
    ));
    let (monitoring, idle) =
        interrupted_while_monitoring(&claude, "claude-monitoring-interrupt").await;

    assert_stopped_each_watch_and_interrupted_nothing(&claude);
    assert_idle_and_unchanged(&monitoring, &idle);
}

#[tokio::test]
async fn an_acknowledged_stop_ends_monitoring_even_when_the_cli_never_notifies() {
    let claude = ScriptedClaude::new(&format!(
        "{}{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(TWO_WATCHES_LEFT_RUNNING),
        stop_task_arm(),
    ));
    let (monitoring, idle) =
        interrupted_while_monitoring(&claude, "claude-monitoring-acknowledged-stop").await;

    assert_stopped_each_watch_and_interrupted_nothing(&claude);
    assert_idle_and_unchanged(&monitoring, &idle);
}

/// A background Subagent that starts a background shell and settles, leaving the shell running,
/// before the loop's own result settles the Turn: the Agent tool use spawns the agent, whose
/// conversation runs `cargo test` in the background, which the CLI reports as a task the subagent
/// owns. `before_result` plays just before the result, for a Watch of the loop's own.
fn subagent_leaves_a_shell_running(before_result: &str) -> String {
    format!(
        r#"      emit '{{"type":"stream_event","event":{{"type":"content_block_start","index":0,"content_block":{{"type":"tool_use","id":"agent_bg","name":"Agent","input":{{}}}}}},"parent_tool_use_id":null,"session_id":"prov-session"}}'
      emit '{{"type":"stream_event","event":{{"type":"content_block_stop","index":0}},"parent_tool_use_id":null,"session_id":"prov-session"}}'
      emit '{{"type":"system","subtype":"task_started","task_id":"agent-task-bg","tool_use_id":"agent_bg","description":"Run the suite","task_type":"local_agent","subagent_type":"general-purpose","session_id":"prov-session"}}'
      emit '{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"toolu_sub_tests","name":"Bash","input":{{"command":"cargo test","run_in_background":true}}}}]}},"parent_tool_use_id":"agent_bg","session_id":"prov-session"}}'
      emit '{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_sub_tests","content":"Command running in background with ID: task-sub-tests"}}]}},"parent_tool_use_id":"agent_bg","session_id":"prov-session"}}'
      emit '{{"type":"system","subtype":"task_started","task_id":"task-sub-tests","tool_use_id":"toolu_sub_tests","description":"cargo test","task_type":"local_bash","owned_by_subagent":true,"session_id":"prov-session"}}'
      emit '{{"type":"system","subtype":"task_notification","task_id":"agent-task-bg","status":"completed","summary":"Started the suite","session_id":"prov-session"}}'
{before_result}      emit '{{"type":"result","subtype":"success","is_error":false,"duration_ms":30,"num_turns":1,"result":"The suite is running.","session_id":"prov-session"}}'
"#
    )
}

/// The Subagent's Session `snapshot`'s one Subagent row leads into.
fn subagent_session(snapshot: &SessionSnapshot) -> SessionId {
    snapshot
        .activities
        .iter()
        .find_map(|activity| match activity {
            Activity::Subagent { session_id, .. } => Some(*session_id),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the spawn left a Subagent row: {:?}", snapshot.activities))
}

fn watched(snapshot: &SessionSnapshot) -> Vec<&str> {
    let mut described = snapshot
        .watches
        .iter()
        .map(|watch| watch.description.as_str())
        .collect::<Vec<_>>();
    described.sort_unstable();
    described
}

#[tokio::test]
async fn a_background_subagents_shell_keeps_its_own_session_and_the_top_level_monitoring() {
    let claude = conversation_fixture(&subagent_leaves_a_shell_running(""));
    let opened = opened_session(&claude, "claude-subagent-watch-monitoring", "Run the suite").await;
    let client = &opened.client;
    let settled = settled_session(client, opened.session_id, 0).await;
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    let child_id = subagent_session(&settled);
    let child = settled_session(client, child_id, 0).await;

    assert_eq!(child.turns[0].status, TurnStatus::Completed);
    assert_eq!(child.session.working_since, None);
    assert!(
        child.session.monitoring_since.is_some(),
        "the settled Subagent's own Session waits on the shell it left running"
    );
    assert_eq!(watched(&child), ["cargo test"]);
    let top_level = client
        .read_session(opened.session_id)
        .await
        .expect("read the top-level Session");
    assert_eq!(top_level.session.working_since, None);
    assert!(
        top_level.session.monitoring_since.is_some(),
        "and the top-level Session Monitors through it"
    );
    assert_eq!(watched(&top_level), ["cargo test"]);
    opened.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn escape_in_a_subagents_session_stops_only_that_subagents_watches() {
    let top_level_shell = r#"      emit '{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_log","name":"Bash","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"tail -f server.log\",\"run_in_background\":true}"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":1},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"task-log","tool_use_id":"toolu_log","description":"tail -f server.log","task_type":"local_bash","session_id":"prov-session"}'
"#;
    let claude = ScriptedClaude::new(&format!(
        "{}{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(&subagent_leaves_a_shell_running(top_level_shell)),
        stop_task_then_notify_arm(),
    ));
    let opened = opened_session(&claude, "claude-subagent-watch-interrupt", "Run the suite").await;
    let (client, session_id) = (&opened.client, opened.session_id);
    let settled = settled_session(client, session_id, 0).await;
    assert_eq!(watched(&settled), ["cargo test", "tail -f server.log"]);
    let child_id = subagent_session(&settled);
    let child = settled_session(client, child_id, 0).await;
    assert_eq!(watched(&child), ["cargo test"]);

    let mut child_feed = client
        .subscribe_session(child_id)
        .await
        .expect("subscribe to the Subagent's Session");
    let outcome = client
        .interrupt_session_reporting_outcome(child_id)
        .await
        .expect("the interrupt of a Monitoring Subagent's Session succeeds");
    assert_eq!(outcome, InterruptOutcome::StoppedWork);
    let child_idle = session_where(
        client,
        &mut child_feed,
        child_id,
        "the Subagent's stopped Watch ends its Monitoring",
        |snapshot| snapshot.session.monitoring_since.is_none(),
    )
    .await;
    assert!(child_idle.watches.is_empty());
    assert_eq!(
        child_idle.turns, child.turns,
        "stopping its Watch begins no Turn"
    );

    let mut feed = client
        .subscribe_session(session_id)
        .await
        .expect("subscribe to the top-level Session");
    let top_level = session_where(
        client,
        &mut feed,
        session_id,
        "the top-level Session waits on its own Watch alone",
        |snapshot| watched(snapshot) == ["tail -f server.log"],
    )
    .await;
    assert!(
        top_level.session.monitoring_since.is_some(),
        "the top-level Session's own Watch keeps running"
    );
    assert_eq!(
        claude
            .control_subtypes()
            .into_iter()
            .filter(|subtype| subtype != "get_context_usage")
            .collect::<Vec<_>>(),
        after_probe(["list_models", "stop_task"]),
        "one stop, for the Subagent's one Watch, and no loop interrupt"
    );
    let stopped = claude
        .requests()
        .into_iter()
        .filter(|request| request.pointer("/request/subtype") == Some(&"stop_task".into()))
        .filter_map(|request| {
            request
                .pointer("/request/task_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    assert_eq!(stopped, ["task-sub-tests"]);
    opened.server.shutdown().await.expect("shut down server");
}
