//! Which of Claude's background tasks keep a Session Monitoring once its Turn has settled (ADR
//! 0030): the ones whose settling the CLI delivers to the agent — a background shell, a Monitor, a
//! monitor over MCP or a WebSocket. An agent task is a Subagent, which keeps its Session Working
//! instead, and any other task is none of the Session's to wait on.

use crate::support::{conversation_fixture, opened_session, settled_session};
use suru::protocol::{SessionSnapshot, TurnStatus};

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
