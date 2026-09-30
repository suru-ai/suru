//! Interrupting a Copilot Turn: the whole agentic loop stops, and everything it left running
//! settles with it.

use crate::server_support::PROGRESS_DEADLINE;
use crate::support::{
    LiveTurn, ScriptedCopilot, abort_arm, agent_messages, conversation_arms, send_arm,
    silent_abort_arm,
};
use suru::{
    protocol::{
        Activity, ActivityStatus, Cost, CostBasis, MessageStatus, NativeMeter, SessionStatus,
        TurnStatus, Usage,
    },
    provider::CopilotRuntime,
};
use tokio::time::{Duration, timeout};

/// A Turn that streams an answer, thinks, and starts a command and a search, then leaves them all
/// running.
const WORK_IN_FLIGHT: &str = r#"      event e1 assistant.message_start '{"messageId":"m1"}'
      event e2 assistant.message_delta '{"messageId":"m1","deltaContent":"Halfway"}'
      event e3 assistant.reasoning_delta '{"reasoningId":"r1","deltaContent":"**Weighing it up**\n\nStill going"}'
      event e4 tool.execution_start '{"toolCallId":"t1","toolName":"bash","arguments":{"command":"sleep 600"}}'
      event e5 tool.execution_start '{"toolCallId":"t2","toolName":"web_search","arguments":{"query":"suru"}}'
      event e6 assistant.usage '{"model":"claude-fixture","inputTokens":100,"outputTokens":20,"cost":0.5}'
"#;

/// What Copilot reports once the abort has stopped its loop.
const ABORTED_IDLE: &str = r#"      event e7 session.idle '{"aborted":true}'
"#;

#[tokio::test]
async fn an_interrupt_settles_the_turn_and_everything_it_left_running() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}",
        conversation_arms(),
        send_arm(WORK_IN_FLIGHT),
        abort_arm(ABORTED_IDLE),
    ));
    let mut live = LiveTurn::start(
        CopilotRuntime::new(copilot.executable()),
        "copilot-interruption",
        "Work until I stop you",
    )
    .await;

    live.client
        .interrupt_session(live.session_id)
        .await
        .expect("Copilot acknowledges the interrupt");

    let interrupted = live
        .wait_for("the interrupted Turn settles", |snapshot| {
            snapshot.turns[0].status != TurnStatus::Active
        })
        .await;

    assert_eq!(interrupted.turns[0].status, TurnStatus::Interrupted);
    assert_eq!(
        interrupted.turns[0].usage,
        Some(Usage {
            fresh_input_tokens: Some(100),
            output_tokens: Some(20),
            native_meter: NativeMeter::from_units(0.5),
            ..Usage::default()
        }),
        "the interrupted Turn keeps usage from calls Copilot completed before the abort"
    );
    assert_eq!(interrupted.turns[0].cost, Cost::from_usd(0.0036));
    assert_eq!(interrupted.turns[0].cost_basis, Some(CostBasis::Reported));
    assert_eq!(interrupted.session.status, SessionStatus::Idle);
    let [message] = agent_messages(&interrupted)[..] else {
        panic!(
            "the Message the Turn was streaming survives it, got {:?}",
            interrupted.messages
        );
    };
    assert_eq!(message.content, "Halfway");
    assert_eq!(
        message.status,
        MessageStatus::Completed,
        "an interrupted Turn leaves no Message streaming"
    );

    let [reasoning, ran, searched] = interrupted.activities.as_slice() else {
        panic!(
            "the interrupted work settles beside the Turn, got {:?}",
            interrupted.activities
        );
    };
    let Activity::Reasoning { status, title, .. } = reasoning else {
        panic!("the abandoned thinking is a Reasoning Activity, got {reasoning:?}");
    };
    assert_eq!(
        *status,
        ActivityStatus::Interrupted,
        "the Reasoning block the interrupt cut off settles interrupted"
    );
    assert_eq!(
        title.as_deref(),
        Some("Weighing it up"),
        "a block cut short keeps the title its split was still withholding"
    );
    let Activity::Command { status, .. } = ran else {
        panic!("the abandoned command is a Command Activity, got {ran:?}");
    };
    assert_eq!(
        *status,
        ActivityStatus::Interrupted,
        "a command the interrupt cut off settles interrupted with the Turn"
    );
    let Activity::ToolCall { status, .. } = searched else {
        panic!("the abandoned search is a Tool Call Activity, got {searched:?}");
    };
    assert_eq!(
        *status,
        ActivityStatus::Interrupted,
        "a Tool Call the interrupt cut off settles interrupted with the Turn"
    );

    let aborted = copilot.wait_for_request("session.abort").await;
    assert!(
        aborted["params"]["sessionId"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "the abort names the Copilot Session it stops: {aborted}"
    );
    assert_eq!(
        copilot
            .requests()
            .iter()
            .filter(|request| request["method"] == "session.abort")
            .count(),
        1,
        "one interrupt asks Copilot to stop once"
    );

    live.shutdown().await;
}

#[tokio::test]
async fn an_interrupt_copilot_never_answers_fails_the_turn_within_the_injected_timeout() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}",
        conversation_arms(),
        send_arm(WORK_IN_FLIGHT),
        silent_abort_arm(),
    ));
    let mut live = LiveTurn::start(
        CopilotRuntime::new(copilot.executable())
            .with_interrupt_request_timeout(Duration::from_millis(300)),
        "copilot-interrupt-timeout",
        "Work until I stop you",
    )
    .await;

    let error = timeout(
        PROGRESS_DEADLINE,
        live.client.interrupt_session(live.session_id),
    )
    .await
    .expect("an interrupt Copilot never answers gives up on its own")
    .expect_err("the unanswered interrupt reaches the client as a failure");
    assert!(
        error
            .to_string()
            .contains("timed out handling `session.abort`"),
        "the failure says Copilot never answered, got: {error:#}"
    );

    let failed = live
        .wait_for(
            "the Turn its interrupt could not stop settles",
            |snapshot| snapshot.turns[0].status != TurnStatus::Active,
        )
        .await;

    assert_eq!(failed.turns[0].status, TurnStatus::Failed);
    assert_eq!(failed.session.status, SessionStatus::Idle);
    let Some(Activity::Error { text, .. }) = failed
        .activities
        .iter()
        .find(|activity| matches!(activity, Activity::Error { .. }))
    else {
        panic!(
            "the interruption failure reads without opening the Log, got {:?}",
            failed.activities
        );
    };
    assert!(
        text.contains("timed out handling `session.abort`"),
        "the Error Activity says what went wrong, got: {text}"
    );

    live.shutdown().await;
}
