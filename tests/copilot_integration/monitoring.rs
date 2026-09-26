//! Copilot's detached background shells as Watches (ADR 0030): a shell the agent runs with
//! `mode: "async", detach: true` outlives the loop without holding its idle back, so the Turn
//! settles and the Session reads Monitoring until the shell's completion notification wakes the
//! loop into a Continuation that begins with the Watch Outcome. An attached background shell is
//! no Watch — Copilot holds the idle back for it, so its Turn stays open and the Session Working —
//! and interrupting a Session that is only Monitoring cancels its detached shells through the task
//! roster rather than aborting a loop that is not running.

use crate::support::{
    Opened, ScriptedCopilot, agent_messages, conversation_arms, opened_session, opened_session_on,
    send_arm, session_where, settled_session,
};
use suru::protocol::{
    Activity, InterruptOutcome, SessionSnapshot, SessionStatus, TranscriptItem, TurnStatus,
    WatchOutcomeStatus,
};
use suru::provider::CopilotRuntime;
use tokio::time::Duration;

/// A Turn that starts a docs server as a detached shell and ends its loop, the way a live CLI
/// reports it: the shell tool's start, the task roster changing as the shell registers, the tool
/// returning at once with the shell's identity, and the loop going idle without waiting on it.
const DETACHED_SHELL_LEFT_RUNNING: &str = r#"      event e1 tool.execution_start '{"toolCallId":"t-serve","toolName":"bash","arguments":{"command":"mdbook serve","description":"Serve the docs","mode":"async","detach":true}}'
      event e2 session.background_tasks_changed '{}'
      event e3 tool.execution_complete '{"toolCallId":"t-serve","success":true,"result":{"content":"<command started in detached background with shellId: shell-1. You will be automatically notified when it completes.>"}}'
      event e4 assistant.message '{"messageId":"m1","content":"The docs server is running."}'
      event e5 session.idle '{}'
"#;

/// The shell `DETACHED_SHELL_LEFT_RUNNING` started, as `session.tasks.list` lists it.
const DETACHED_SHELL_ROSTER: &str = r#"[{"type":"shell","id":"shell-1","description":"Serve the docs","command":"mdbook serve","status":"running","startedAt":"2026-01-01T00:00:00Z","attachmentMode":"detached","executionMode":"background","pid":4242}]"#;

/// Once released, the detached shell completing and the loop its notification wakes.
const DETACHED_SHELL_COMPLETES: &str = r#"      while [ ! -e "$COPILOT_FIXTURE_RELEASE" ]; do sleep 0.01; done
      event e6 system.notification '{"content":"<system_notification>\nDetached shell \"Serve the docs\" (shellId: shell-1) has completed.\n</system_notification>","kind":{"type":"shell_detached_completed","shellId":"shell-1","description":"Serve the docs"}}'
      event e7 assistant.message '{"messageId":"m2","content":"The docs server exited."}'
      event e8 session.idle '{}'
"#;

/// A `session.tasks.list` arm answering with `tasks`, a JSON array literal, and then playing
/// `timeline`. The fixture reads one request at a time, so whatever the timeline waits on happens
/// only after the roster read has been answered.
fn task_roster_arm(tasks: &str, timeline: &str) -> String {
    format!(
        r#"    *'"method":"session.tasks.list"'*)
      reply '{{"jsonrpc":"2.0","id":'"$id"',"result":{{"tasks":{tasks}}}}}'
{timeline}      ;;
"#
    )
}

/// A `session.tasks.cancel` arm confirming the cancel, as Copilot does for a detached shell whose
/// process it knows.
fn task_cancel_arm() -> String {
    r#"    *'"method":"session.tasks.cancel"'*)
      reply '{"jsonrpc":"2.0","id":'"$id"',"result":{"cancelled":true}}'
      ;;
"#
    .to_owned()
}

/// The Session once the Turn that left the detached shell running has settled, which must leave it
/// Monitoring that shell.
async fn monitoring_the_detached_shell(opened: &Opened) -> SessionSnapshot {
    let settled = settled_session(&opened.client, opened.session_id, 0).await;
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    assert_eq!(settled.session.working_since, None);
    assert_eq!(
        settled.session.monitoring_since, settled.turns[0].settled_at,
        "the detached shell left running keeps the Session Monitoring from the Turn's settle"
    );
    assert_eq!(
        settled
            .watches
            .iter()
            .map(|watch| watch.description.as_str())
            .collect::<Vec<_>>(),
        ["Serve the docs"],
        "the Session names the shell it waits on in the words the shell tool was given"
    );
    settled
}

/// The first entry of the Turn at `turn_index` in Transcript order.
fn heading_activity(snapshot: &SessionSnapshot, turn_index: usize) -> &Activity {
    let turn_id = snapshot.turns[turn_index].id;
    let heading = snapshot.transcript.iter().find_map(|item| match item {
        TranscriptItem::Message { message_id } => snapshot
            .messages
            .iter()
            .find(|message| message.id == *message_id && message.turn_id == turn_id)
            .map(|_| None),
        TranscriptItem::Activity { activity_id } => snapshot
            .activities
            .iter()
            .find(|activity| activity.id() == *activity_id && activity.turn_id() == turn_id)
            .map(Some),
    });
    heading
        .flatten()
        .unwrap_or_else(|| panic!("the Turn begins with an Activity: {snapshot:#?}"))
}

/// The detached shell's completion, played without waiting: the shell ran shorter than it took
/// Suru to read the roster, or the read never came back.
const DETACHED_SHELL_COMPLETES_AT_ONCE: &str = r#"      event e6 system.notification '{"content":"<system_notification>\nDetached shell \"Serve the docs\" (shellId: shell-1) has completed.\n</system_notification>","kind":{"type":"shell_detached_completed","shellId":"shell-1","description":"Serve the docs"}}'
      event e7 assistant.message '{"messageId":"m2","content":"The docs server exited."}'
      event e8 session.idle '{}'
"#;

/// The Session once the loop the detached shell's completion woke has settled, which must have
/// shown that work as a Continuation headed by the shell's Watch Outcome and left nothing to
/// Monitor.
async fn woken_by_the_detached_shell(opened: &Opened) -> SessionSnapshot {
    let woken = settled_session(&opened.client, opened.session_id, 1).await;
    let continuation = &woken.turns[1];
    assert_eq!(
        continuation.prompt_id, None,
        "the woken loop's work is a Continuation"
    );
    assert_eq!(continuation.status, TurnStatus::Completed);
    let heading = heading_activity(&woken, 1);
    assert_eq!(
        heading,
        &Activity::WatchOutcome {
            id: heading.id(),
            turn_id: continuation.id,
            status: WatchOutcomeStatus::Completed,
            description: "Serve the docs".to_owned(),
            summary: Some(
                r#"Detached shell "Serve the docs" (shellId: shell-1) has completed."#.to_owned()
            ),
        },
        "the Continuation opens with how the shell settled, in Copilot's words out of their wrapper"
    );
    let late = agent_messages(&woken);
    let [_, message] = late.as_slice() else {
        panic!("the woken loop's answer joins the Session, got {late:?}");
    };
    assert_eq!(message.content, "The docs server exited.");
    assert_eq!(message.turn_id, continuation.id);
    assert_eq!(
        woken.session.monitoring_since, None,
        "the shell that woke the Agent is settled, so nothing is left to Monitor"
    );
    assert!(woken.watches.is_empty());
    woken
}

#[tokio::test]
async fn a_detached_shells_completion_wakes_the_monitoring_session_into_a_continuation_headed_by_its_watch_outcome()
 {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}",
        conversation_arms(),
        send_arm(DETACHED_SHELL_LEFT_RUNNING),
        task_roster_arm(DETACHED_SHELL_ROSTER, DETACHED_SHELL_COMPLETES),
    ));
    let opened = opened_session(&copilot, "copilot-detached-shell-watch", "Serve the docs").await;
    monitoring_the_detached_shell(&opened).await;

    copilot.release();
    woken_by_the_detached_shell(&opened).await;

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn a_detached_shell_that_finished_before_the_roster_read_still_wakes_a_continuation_headed_by_its_watch_outcome()
 {
    let finished =
        DETACHED_SHELL_ROSTER.replace(r#""status":"running""#, r#""status":"completed""#);
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}",
        conversation_arms(),
        send_arm(DETACHED_SHELL_LEFT_RUNNING),
        task_roster_arm(&finished, DETACHED_SHELL_COMPLETES_AT_ONCE),
    ));
    let opened = opened_session(&copilot, "copilot-short-detached-shell", "Serve the docs").await;

    let woken = woken_by_the_detached_shell(&opened).await;
    assert_eq!(
        woken.turns[0].status,
        TurnStatus::Completed,
        "the Turn settled on its own idle, with no Watch left for the Session to Monitor"
    );

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn a_roster_read_copilot_never_answers_gives_up_within_the_injected_timeout_and_the_completion_still_wakes_a_continuation()
 {
    // The roster arm plays the rest of the timeline without ever answering the read.
    let silent_roster_arm = format!(
        r#"    *'"method":"session.tasks.list"'*)
{DETACHED_SHELL_COMPLETES_AT_ONCE}      ;;
"#
    );
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}",
        conversation_arms(),
        send_arm(DETACHED_SHELL_LEFT_RUNNING),
        silent_roster_arm,
    ));
    let opened = opened_session_on(
        CopilotRuntime::new(copilot.executable())
            .with_interrupt_request_timeout(Duration::from_millis(300)),
        "copilot-silent-task-roster",
        "Serve the docs",
    )
    .await;

    let woken = woken_by_the_detached_shell(&opened).await;
    assert_eq!(woken.turns[0].status, TurnStatus::Completed);

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn an_attached_background_shell_keeps_its_turn_working_rather_than_monitoring() {
    // Copilot holds the loop's idle back while an attached shell runs, so none follows here.
    let timeline = r#"      event e1 tool.execution_start '{"toolCallId":"t-watch","toolName":"bash","arguments":{"command":"cargo watch","description":"Rebuild on change","mode":"async"}}'
      event e2 session.background_tasks_changed '{}'
      event e3 tool.execution_complete '{"toolCallId":"t-watch","success":true,"result":{"content":"<command started in background with shellId: shell-2>"}}'
      event e4 assistant.message '{"messageId":"m1","content":"Rebuilding on every change."}'
"#;
    let roster = r#"[{"type":"shell","id":"shell-2","description":"Rebuild on change","command":"cargo watch","status":"running","startedAt":"2026-01-01T00:00:00Z","attachmentMode":"attached","executionMode":"background"}]"#;
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}",
        conversation_arms(),
        send_arm(timeline),
        task_roster_arm(roster, ""),
    ));
    let opened = opened_session(&copilot, "copilot-attached-shell", "Rebuild on change").await;
    let mut feed = opened
        .client
        .subscribe_session(opened.session_id)
        .await
        .expect("subscribe to the Session");
    // The Message follows the roster change on the timeline, so once it is in, the roster the
    // change sent Suru to read has been read.
    let running = session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "the loop answers after the roster changes",
        |snapshot| {
            agent_messages(snapshot)
                .iter()
                .any(|message| message.content == "Rebuilding on every change.")
        },
    )
    .await;
    copilot.wait_for_request("session.tasks.list").await;

    assert_eq!(running.turns[0].status, TurnStatus::Active);
    assert!(
        running.session.working_since.is_some(),
        "the attached shell keeps its Turn open, and the Session Working"
    );
    assert_eq!(
        running.session.monitoring_since, None,
        "an attached shell is no Watch"
    );
    assert!(running.watches.is_empty());

    drop(feed);
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn interrupting_a_session_monitoring_a_detached_shell_cancels_the_shell_and_leaves_it_idle() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}{}",
        conversation_arms(),
        send_arm(DETACHED_SHELL_LEFT_RUNNING),
        task_roster_arm(DETACHED_SHELL_ROSTER, ""),
        task_cancel_arm(),
    ));
    let opened = opened_session(&copilot, "copilot-detached-shell-stop", "Serve the docs").await;
    let monitoring = monitoring_the_detached_shell(&opened).await;
    let mut feed = opened
        .client
        .subscribe_session(opened.session_id)
        .await
        .expect("subscribe to the Session");

    let outcome = opened
        .client
        .interrupt_session_reporting_outcome(opened.session_id)
        .await
        .expect("the interrupt of a Monitoring Session succeeds");
    assert_eq!(outcome, InterruptOutcome::StoppedWork);
    let idle = session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "the cancelled shell ends Monitoring",
        |snapshot| snapshot.session.monitoring_since.is_none(),
    )
    .await;

    assert_eq!(idle.session.status, SessionStatus::Idle);
    assert_eq!(idle.session.working_since, None);
    assert!(idle.watches.is_empty());
    assert_eq!(
        idle.turns, monitoring.turns,
        "stopping the Watch settles no Turn and begins none"
    );
    assert!(
        !idle
            .activities
            .iter()
            .any(|activity| matches!(activity, Activity::WatchOutcome { .. })),
        "a Watch stopped by an interrupt wakes nothing, so it records no Watch Outcome"
    );
    let cancel = copilot.wait_for_request("session.tasks.cancel").await;
    assert_eq!(
        cancel["params"]["id"], "shell-1",
        "the cancel names the shell by the identity the roster listed it under: {cancel}"
    );
    assert!(
        !copilot
            .methods()
            .iter()
            .any(|method| method == "session.abort"),
        "no loop is running to abort"
    );

    drop(feed);
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn a_detached_shell_copilot_declines_to_cancel_leaves_the_session_monitoring_and_reports_the_refusal()
 {
    // Copilot cannot signal a detached shell whose process identity it never learned.
    let declining_cancel_arm = r#"    *'"method":"session.tasks.cancel"'*)
      reply '{"jsonrpc":"2.0","id":'"$id"',"result":{"cancelled":false}}'
      ;;
"#;
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}{}",
        conversation_arms(),
        send_arm(DETACHED_SHELL_LEFT_RUNNING),
        task_roster_arm(DETACHED_SHELL_ROSTER, ""),
        declining_cancel_arm,
    ));
    let opened = opened_session(&copilot, "copilot-detached-shell-refused", "Serve the docs").await;
    let monitoring = monitoring_the_detached_shell(&opened).await;

    let error = opened
        .client
        .interrupt_session_reporting_outcome(opened.session_id)
        .await
        .expect_err("a stop Copilot declines reaches the client as a failure");
    assert!(
        error
            .to_string()
            .contains("declined to cancel detached shell `shell-1`"),
        "the failure says which shell Copilot would not stop, got: {error:#}"
    );
    copilot.wait_for_request("session.tasks.cancel").await;

    let still = opened
        .client
        .read_session(opened.session_id)
        .await
        .expect("read the Session");
    assert_eq!(
        still.session.monitoring_since, monitoring.session.monitoring_since,
        "the shell still runs, so the Session is still Monitoring it"
    );
    assert_eq!(still.watches, monitoring.watches);

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn a_cancel_copilot_never_answers_fails_the_stop_within_the_injected_timeout_and_leaves_the_session_monitoring()
 {
    let silent_cancel_arm = r#"    *'"method":"session.tasks.cancel"'*)
      :
      ;;
"#;
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}{}",
        conversation_arms(),
        send_arm(DETACHED_SHELL_LEFT_RUNNING),
        task_roster_arm(DETACHED_SHELL_ROSTER, ""),
        silent_cancel_arm,
    ));
    let opened = opened_session_on(
        CopilotRuntime::new(copilot.executable())
            .with_interrupt_request_timeout(Duration::from_millis(300)),
        "copilot-silent-shell-cancel",
        "Serve the docs",
    )
    .await;
    let monitoring = monitoring_the_detached_shell(&opened).await;

    let error = opened
        .client
        .interrupt_session_reporting_outcome(opened.session_id)
        .await
        .expect_err("a cancel Copilot never answers reaches the client as a failure");
    assert!(
        error
            .to_string()
            .contains("timed out handling `session.tasks.cancel`"),
        "the failure says Copilot never answered, got: {error:#}"
    );

    let still = opened
        .client
        .read_session(opened.session_id)
        .await
        .expect("read the Session");
    assert_eq!(
        still.session.monitoring_since,
        monitoring.session.monitoring_since
    );
    assert_eq!(still.watches, monitoring.watches);

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}
