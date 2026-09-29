//! The Working Indicator of a Session whose Agent is waiting on its Subagents through the
//! Broker's `wait_subagents` (ADR 0035): its Turn is still open, but the only work in it is the
//! wait, so the indicator says what the Working is spent on rather than that the Agent works.

use crate::support::{
    connected_application, failed_session_snapshot, rendered_application_rows_at, workspace_dir,
};
use suru::{
    managed_client::SessionEvent,
    protocol::{
        Activity, ActivityId, ActivityStatus, PromptId, SessionChange, SessionId, SessionRevision,
        SessionSnapshot, SessionStatus, SessionTimestamp, SessionUpdate, TranscriptItem, TurnId,
        TurnStatus,
    },
    tui::{Application, ApplicationEvent},
};

/// A Session Working since 75 seconds ago whose Turn holds one working brokered Subagent, its
/// Agent waiting on it.
fn waiting_snapshot(workspace: &std::path::Path) -> SessionSnapshot {
    let mut snapshot = failed_session_snapshot(
        SessionId::new(),
        PromptId::new(),
        "Split the work between two Subagents",
        workspace,
    );
    let turn_id = snapshot.turns[0].id;
    snapshot.turns[0].status = TurnStatus::Active;
    snapshot.session.status = SessionStatus::Active;
    snapshot.session.working_since = Some(SessionTimestamp(SessionTimestamp::now().0 - 75_000));
    let subagent = Activity::Subagent {
        id: ActivityId::new(),
        turn_id,
        status: ActivityStatus::Active,
        name: "Researcher".to_owned(),
        description: "Survey the Provider seams".to_owned(),
        model: None,
        session_id: SessionId::new(),
        brokered: true,
        duration_ms: None,
    };
    snapshot.transcript.push(TranscriptItem::Activity {
        activity_id: subagent.id(),
    });
    snapshot.activities.push(subagent);
    snapshot.waiting_on_subagents = Some(turn_id);
    snapshot
}

/// An Active Tool Call in `snapshot`'s Turn, on `server` where it has one.
fn running_tool_call(
    snapshot: &mut SessionSnapshot,
    server: Option<&str>,
    name: &str,
    input: &str,
) -> ActivityId {
    let tool_call = Activity::ToolCall {
        id: ActivityId::new(),
        turn_id: snapshot.turns[0].id,
        status: ActivityStatus::Active,
        name: name.to_owned(),
        server: server.map(str::to_owned),
        input: input.to_owned(),
        input_truncated: false,
        output: String::new(),
        output_truncated: false,
        omitted_parts: 0,
    };
    let id = tool_call.id();
    snapshot
        .transcript
        .push(TranscriptItem::Activity { activity_id: id });
    snapshot.activities.push(tool_call);
    id
}

/// The same Session once its Agent's Provider has recorded the Broker call it waits in, still
/// open, as the Tool Call every Provider records it as.
fn waiting_in_its_tool_call(workspace: &std::path::Path) -> SessionSnapshot {
    let mut snapshot = waiting_snapshot(workspace);
    running_tool_call(&mut snapshot, Some("suru"), "wait_subagents", "");
    snapshot
}

fn attached(workspace: &std::path::Path, snapshot: SessionSnapshot) -> Application {
    let mut application = connected_application(workspace);
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach the waiting Session");
    application
}

fn updated(
    application: &mut Application,
    session_id: SessionId,
    revision: &mut SessionRevision,
    changes: Vec<SessionChange>,
) {
    *revision = SessionRevision(revision.0 + 1);
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: *revision,
                changes,
            },
        )))
        .expect("the change arrives over the Session stream");
}

/// The Working Indicator's line, as drawn.
fn indicator(application: &Application) -> String {
    let rows = rendered_application_rows_at(application, 100, 18);
    rows.iter()
        .find(|row| row.contains("to interrupt"))
        .unwrap_or_else(|| panic!("the Working Indicator is drawn: {rows:?}"))
        .trim()
        .to_owned()
}

#[test]
fn a_turn_whose_only_work_is_a_wait_on_subagents_reads_waiting_for_subagents() {
    let workspace = workspace_dir();
    let application = attached(workspace.path(), waiting_snapshot(workspace.path()));

    let line = indicator(&application);
    assert!(
        line.starts_with("Waiting for subagents (1m ") && line.ends_with("s • Esc to interrupt)"),
        "the wait is named in Working's place, counting Working's own time and interrupting \
         the Turn as ever: {line}"
    );
}

#[test]
fn work_of_the_turns_own_beside_the_wait_reads_working_until_it_settles() {
    let workspace = workspace_dir();
    let snapshot = waiting_snapshot(workspace.path());
    let session_id = snapshot.session.id;
    let turn_id = snapshot.turns[0].id;
    let mut revision = snapshot.revision;
    let mut application = attached(workspace.path(), snapshot);
    let command = ActivityId::new();

    updated(
        &mut application,
        session_id,
        &mut revision,
        vec![SessionChange::ActivityAdded {
            activity: Activity::Command {
                id: command,
                turn_id,
                status: ActivityStatus::Active,
                command: "cargo test".to_owned(),
                cwd: None,
                output: String::new(),
                output_truncated: false,
                exit_status: None,
            },
        }],
    );
    let line = indicator(&application);
    assert!(
        line.starts_with("Working (1m "),
        "a Command running beside the wait is the Agent's own work: {line}"
    );

    updated(
        &mut application,
        session_id,
        &mut revision,
        vec![SessionChange::CommandStatusChanged {
            activity_id: command,
            status: ActivityStatus::Completed,
            exit_status: Some(0),
        }],
    );
    let line = indicator(&application);
    assert!(
        line.starts_with("Waiting for subagents (1m "),
        "and once it settles only the wait is left: {line}"
    );
}

#[test]
fn a_wait_that_ends_reads_working_again_and_one_from_another_turn_never_reads_waiting() {
    let workspace = workspace_dir();
    let snapshot = waiting_snapshot(workspace.path());
    let session_id = snapshot.session.id;
    let mut revision = snapshot.revision;
    let mut application = attached(workspace.path(), snapshot);

    updated(
        &mut application,
        session_id,
        &mut revision,
        vec![SessionChange::SessionWaitingOnSubagentsChanged {
            waiting_on_subagents: None,
        }],
    );
    let line = indicator(&application);
    assert!(
        line.starts_with("Working (1m "),
        "the Agent answered works on, with no break in Working's time: {line}"
    );

    // A wait still open from a Turn that has settled — its client not yet
    // known to be gone — says nothing of the Turn working now.
    updated(
        &mut application,
        session_id,
        &mut revision,
        vec![SessionChange::SessionWaitingOnSubagentsChanged {
            waiting_on_subagents: Some(TurnId::new()),
        }],
    );
    let line = indicator(&application);
    assert!(line.starts_with("Working (1m "), "{line}");
}

/// The wait is itself a Broker call, which the Agent's Provider records as a Tool Call open for as
/// long as the wait is: that Tool Call is the wait, not work of the Turn's own beside it.
#[test]
fn the_waits_own_tool_call_is_the_wait_and_reads_waiting_for_subagents() {
    let workspace = workspace_dir();
    let application = attached(workspace.path(), waiting_in_its_tool_call(workspace.path()));

    let line = indicator(&application);
    assert!(
        line.starts_with("Waiting for subagents (1m "),
        "the open wait_subagents Tool Call is the wait itself: {line}"
    );
}

#[test]
fn a_tool_call_of_the_turns_own_beside_the_wait_reads_working_until_it_settles() {
    let workspace = workspace_dir();
    let mut snapshot = waiting_in_its_tool_call(workspace.path());
    let session_id = snapshot.session.id;
    let mut revision = snapshot.revision;
    let read = running_tool_call(&mut snapshot, None, "Read", "file_path=src/lib.rs");
    let mut application = attached(workspace.path(), snapshot);

    let line = indicator(&application);
    assert!(
        line.starts_with("Working (1m "),
        "a Tool Call running beside the wait is the Agent's own work, as a Command is: {line}"
    );

    updated(
        &mut application,
        session_id,
        &mut revision,
        vec![SessionChange::ToolCallStatusChanged {
            activity_id: read,
            status: ActivityStatus::Completed,
            omitted_parts: 0,
        }],
    );
    let line = indicator(&application);
    assert!(
        line.starts_with("Waiting for subagents (1m "),
        "and once it settles only the wait is left: {line}"
    );
}
