//! The Working Indicator of a Session that is Monitoring (ADR 0030): what it waits on, for how
//! long, and the gesture that stops it — stopping its Watches rather than interrupting a Turn.

use crate::support::{
    connected_application, failed_session_snapshot, rendered_application_buffer,
    rendered_application_rows_at, text_position, workspace_dir,
};
use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use suru::{
    managed_client::SessionEvent,
    protocol::{
        PromptId, SessionChange, SessionId, SessionRevision, SessionSnapshot, SessionTimestamp,
        SessionUpdate, TurnStatus, WatchSummary,
    },
    tui::{Application, ApplicationEvent, ApplicationTransition},
};

/// A Session whose Turn has settled while the Watches named by `descriptions` are still live,
/// Monitoring since `since`.
fn monitoring_snapshot(
    workspace: &std::path::Path,
    since: SessionTimestamp,
    descriptions: &[&str],
) -> SessionSnapshot {
    let mut snapshot = failed_session_snapshot(
        SessionId::new(),
        PromptId::new(),
        "Run the tests in the background",
        workspace,
    );
    snapshot.turns[0].status = TurnStatus::Completed;
    snapshot.session.working_since = None;
    snapshot.session.monitoring_since = Some(since);
    snapshot.watches = descriptions
        .iter()
        .map(|description| WatchSummary {
            description: (*description).to_owned(),
            started_at: since,
        })
        .collect();
    snapshot
}

fn attached(workspace: &std::path::Path, snapshot: SessionSnapshot) -> Application {
    let mut application = connected_application(workspace);
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("attach the Monitoring Session");
    application
}

fn drawn(application: &Application) -> String {
    rendered_application_rows_at(application, 100, 16).join("\n")
}

fn press_escape(application: &mut Application) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )))
        .expect("press Escape")
}

/// A moment `seconds` before now, on the clock the Working Indicator reads elapsed time against.
fn seconds_ago(seconds: u64) -> SessionTimestamp {
    SessionTimestamp(SessionTimestamp::now().0 - seconds * 1_000)
}

#[test]
fn the_monitoring_line_names_its_one_watch_by_its_description() {
    let workspace = workspace_dir();
    let application = attached(
        workspace.path(),
        monitoring_snapshot(workspace.path(), SessionTimestamp::now(), &["cargo test"]),
    );

    let screen = drawn(&application);
    assert!(
        screen.contains("Monitoring cargo test (0s • Esc to stop)"),
        "the line names the Watch it waits on and offers to stop it: {screen}"
    );
    assert!(
        !screen.contains("Working") && !screen.contains("to interrupt"),
        "a Session that is only Monitoring is not Working, and stopping it interrupts no Turn: \
         {screen}"
    );
}

#[test]
fn the_monitoring_line_counts_several_watches_to_stay_one_line() {
    let workspace = workspace_dir();
    let application = attached(
        workspace.path(),
        monitoring_snapshot(
            workspace.path(),
            SessionTimestamp::now(),
            &["cargo test", "tail -f server.log", "Watch the deploy"],
        ),
    );

    let screen = drawn(&application);
    assert!(
        screen.contains("Monitoring 3 Watches (0s • Esc to stop)"),
        "several Watches are counted rather than named: {screen}"
    );
    assert!(!screen.contains("cargo test"), "{screen}");
}

#[test]
fn the_monitoring_line_counts_its_elapsed_time_from_when_monitoring_began() {
    let workspace = workspace_dir();
    let application = attached(
        workspace.path(),
        monitoring_snapshot(workspace.path(), seconds_ago(75), &["cargo test"]),
    );

    let screen = drawn(&application);
    let line = screen
        .lines()
        .find(|row| row.contains("Monitoring cargo test"))
        .unwrap_or_else(|| panic!("the Monitoring line is drawn: {screen}"));
    assert!(
        line.contains("(1m ") && line.contains("s • Esc to stop)"),
        "Monitoring counts from monitoring_since, not from the Turn before it: {line}"
    );
}

#[test]
fn the_monitoring_line_shimmers_only_its_label() {
    let workspace = workspace_dir();
    let mut application = attached(
        workspace.path(),
        monitoring_snapshot(workspace.path(), SessionTimestamp::now(), &["cargo test"]),
    );

    let styles_of = |buffer: &ratatui::buffer::Buffer, (x, y): (u16, u16), width: usize| {
        (0..width)
            .map(|offset| buffer.cell((x + offset as u16, y)).unwrap().style())
            .collect::<Vec<_>>()
    };
    let before = rendered_application_buffer(&application, 100, 16);
    let label = text_position(&before, "Monitoring");
    let description = text_position(&before, "cargo test");
    let metadata = text_position(&before, "Esc to stop");
    let label_before = styles_of(&before, label, "Monitoring".len());
    let description_before = styles_of(&before, description, "cargo test".len());
    let metadata_before = styles_of(&before, metadata, "Esc to stop".len());

    for _ in 0..10 {
        application
            .handle_event(ApplicationEvent::SpinnerTick)
            .expect("advance presentation animation");
    }
    let after = rendered_application_buffer(&application, 100, 16);
    assert_ne!(
        styles_of(&after, label, "Monitoring".len()),
        label_before,
        "the Monitoring label advances its shimmer"
    );
    assert_eq!(
        styles_of(&after, description, "cargo test".len()),
        description_before,
        "the Watch's description stays still and readable"
    );
    assert_eq!(
        styles_of(&after, metadata, "Esc to stop".len()),
        metadata_before,
        "elapsed time and stop guidance stay still"
    );
    assert!(
        description_before
            .iter()
            .all(|style| *style == metadata_before[0]),
        "the description is drawn subdued, like the metadata"
    );
}

#[test]
fn escape_arms_and_then_requests_stopping_the_watches() {
    let workspace = workspace_dir();
    let snapshot = monitoring_snapshot(workspace.path(), seconds_ago(3), &["cargo test"]);
    let (session_id, revision) = (snapshot.session.id, snapshot.revision);
    let mut application = attached(workspace.path(), snapshot);

    assert_eq!(
        press_escape(&mut application),
        ApplicationTransition::Continue,
        "the first Escape only arms the stop"
    );
    let armed = drawn(&application);
    assert!(
        armed.contains("Monitoring cargo test (3s • Esc again to stop)")
            || armed.contains("Monitoring cargo test (4s • Esc again to stop)"),
        "an armed stop says what the second press does: {armed}"
    );

    let ApplicationTransition::InterruptSession { session } = press_escape(&mut application) else {
        panic!("the second Escape asks the Server to interrupt the Session");
    };
    assert_eq!(session.session_id, session_id);
    let requested = drawn(&application);
    assert!(
        requested.contains("Monitoring cargo test (") && requested.contains("s • stopping…)"),
        "a confirmed stop says it is under way until the Watches settle: {requested}"
    );
    assert!(!requested.contains("Esc"), "{requested}");

    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Updated(
            SessionUpdate {
                session_id,
                revision: SessionRevision(revision.0 + 1),
                changes: vec![
                    SessionChange::SessionMonitoringChanged {
                        monitoring_since: None,
                    },
                    SessionChange::SessionWatchesChanged {
                        watches: Vec::new(),
                    },
                ],
            },
        )))
        .expect("the Watches settle over the Session stream");
    let idle = drawn(&application);
    assert!(
        !idle.contains("Monitoring") && !idle.contains("stopping"),
        "the Session goes idle once its Watches settle: {idle}"
    );
    assert_eq!(
        press_escape(&mut application),
        ApplicationTransition::Continue,
        "an idle Session has nothing left to stop"
    );
    assert!(!drawn(&application).contains("again to stop"));
}

#[test]
fn a_watch_stop_that_fails_offers_the_gesture_again() {
    let workspace = workspace_dir();
    let mut application = attached(
        workspace.path(),
        monitoring_snapshot(workspace.path(), SessionTimestamp::now(), &["cargo test"]),
    );
    press_escape(&mut application);
    press_escape(&mut application);
    assert!(drawn(&application).contains("stopping…"));

    application
        .handle_event(ApplicationEvent::SessionOperationFailed(
            "Provider Watch stop failed".to_owned(),
        ))
        .expect("report the refused stop");
    let screen = drawn(&application);
    assert!(
        screen.contains("s • Esc to stop)") && !screen.contains("stopping…"),
        "a stop that stopped nothing is offered again: {screen}"
    );
}
