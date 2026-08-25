//! The configurable Session Content Column and the surfaces that share it.

use crate::support::{
    connected_application, enter_session, navigable_session_snapshot, rendered_application_rows_at,
};
use crossterm::event::{
    Event as InputEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use suru::{
    managed_client::{ManagedEvent, SessionEvent},
    protocol::{
        Activity, ActivityStatus, EffectiveSettings, SessionContentWidth, SessionSettings,
        SessionStatus, SettingsSnapshot, TurnStatus,
    },
    tui::{Application, ApplicationEvent, CommandId},
};

fn session_with_width(
    workspace: &std::path::Path,
    content_width: SessionContentWidth,
) -> (Application, suru::protocol::SessionSnapshot) {
    let mut application = connected_application(workspace);
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::SettingsSnapshot(
            SettingsSnapshot {
                settings: EffectiveSettings {
                    session: SessionSettings {
                        content_width,
                        ..SessionSettings::default()
                    },
                    ..EffectiveSettings::default()
                },
                pinned: Vec::new(),
                diagnostics: Vec::new(),
            },
        )))
        .expect("receive Session content width");
    let (_, snapshot) = enter_session(&mut application, workspace);
    (application, snapshot)
}

fn composer_columns(rows: &[String]) -> (usize, usize) {
    let border = rows
        .iter()
        .find(|row| row.contains('┌') && row.contains(" Prompt "))
        .expect("Session composer border is visible");
    (
        border
            .chars()
            .position(|character| character == '┌')
            .expect("composer has a left border"),
        border
            .chars()
            .enumerate()
            .filter_map(|(column, character)| (character == '┐').then_some(column))
            .last()
            .expect("composer has a right border"),
    )
}

fn composer_height(rows: &[String]) -> usize {
    let top = rows
        .iter()
        .position(|row| row.contains('┌') && row.contains(" Prompt "))
        .expect("composer has a top border");
    let bottom = rows
        .iter()
        .enumerate()
        .skip(top + 1)
        .find_map(|(row, content)| content.contains('└').then_some(row))
        .expect("composer has a bottom border");
    bottom - top + 1
}

fn occupied_columns(row: &str) -> (usize, usize) {
    let occupied = row
        .chars()
        .enumerate()
        .filter_map(|(column, character)| (!character.is_whitespace()).then_some(column))
        .collect::<Vec<_>>();
    (
        *occupied.first().expect("row has visible content"),
        *occupied.last().expect("row has visible content"),
    )
}

fn row_containing<'a>(rows: &'a [String], needle: &str) -> &'a str {
    rows.iter()
        .find(|row| row.contains(needle))
        .map(String::as_str)
        .unwrap_or_else(|| {
            panic!(
                "rendered frame did not contain {needle:?}:\n{}",
                rows.join("\n")
            )
        })
}

fn row_index(rows: &[String], needle: &str) -> u16 {
    rows.iter()
        .position(|row| row.contains(needle))
        .expect("rendered frame contains row") as u16
}

fn click(application: &mut Application, column: u16, row: u16) {
    application
        .handle_terminal_event(InputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }))
        .expect("handle Transcript click");
}

#[test]
fn a_custom_maximum_centers_the_session_composer() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let (application, _) = session_with_width(workspace.path(), SessionContentWidth::Maximum(60));

    let rows = rendered_application_rows_at(&application, 120, 20);
    assert_eq!(composer_columns(&rows), (30, 89));
}

#[test]
fn default_and_fill_use_the_expected_session_content_column() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let (defaulted, _) = session_with_width(workspace.path(), SessionContentWidth::Maximum(80));
    let (fill, _) = session_with_width(workspace.path(), SessionContentWidth::Fill);

    assert_eq!(
        composer_columns(&rendered_application_rows_at(&defaulted, 120, 20)),
        (20, 99),
        "Maximum 80 centers within the normally padded 116 columns"
    );
    assert_eq!(
        composer_columns(&rendered_application_rows_at(&fill, 120, 20)),
        (2, 117),
        "Fill uses the normally padded width"
    );
}

#[test]
fn a_maximum_shrinks_below_fifty_when_the_terminal_is_narrow() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let (application, _) = session_with_width(workspace.path(), SessionContentWidth::Maximum(80));

    assert_eq!(
        composer_columns(&rendered_application_rows_at(&application, 40, 20)),
        (1, 38),
        "the 38 normally padded columns remain usable without clipping"
    );
}

#[test]
fn session_surfaces_share_the_column_while_the_header_keeps_terminal_width() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let (mut application, initial) =
        session_with_width(workspace.path(), SessionContentWidth::Maximum(60));
    let session_id = initial.session.id;
    let snapshot = navigable_session_snapshot(session_id, workspace.path(), 10);
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Snapshot(snapshot)))
        .expect("attach a long Transcript");
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Queued inside the column".to_owned(),
        )))
        .expect("type a queued Prompt");
    application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitQueue))
        .expect("dock the queued Prompt optimistically");

    // Establish the rendered viewport before moving away from the latest row.
    rendered_application_rows_at(&application, 120, 30);
    application
        .handle_event(ApplicationEvent::Command(CommandId::ScrollTranscriptPageUp))
        .expect("move into Transcript history");
    let rows = rendered_application_rows_at(&application, 120, 30);

    for surface in [
        "Prompt section",
        "Pending ·",
        "Latest ↓",
        "Queued inside the column",
        " Prompt ",
        "idle",
    ] {
        let (left, right) = occupied_columns(row_containing(&rows, surface));
        assert!(
            left >= 30 && right <= 89,
            "{surface:?} must stay within columns 30–89, occupied {left}–{right}"
        );
    }

    let header = row_containing(&rows, "Suru");
    assert_eq!(occupied_columns(header), (2, 117));

    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "/".to_owned(),
        )))
        .expect("show composer autocomplete");
    let autocomplete = rendered_application_rows_at(&application, 120, 30);
    for surface in [" Commands ", "/new"] {
        let (left, right) = occupied_columns(row_containing(&autocomplete, surface));
        assert!(
            left >= 30 && right <= 89,
            "{surface:?} must stay within columns 30–89, occupied {left}–{right}"
        );
    }
}

#[test]
fn session_content_width_does_not_change_the_landing() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::SettingsSnapshot(
            SettingsSnapshot {
                settings: EffectiveSettings {
                    session: SessionSettings {
                        content_width: SessionContentWidth::Maximum(50),
                        ..SessionSettings::default()
                    },
                    ..EffectiveSettings::default()
                },
                pinned: vec!["session.contentWidth".to_owned()],
                diagnostics: Vec::new(),
            },
        )))
        .expect("receive Session content width");

    let rows = rendered_application_rows_at(&application, 120, 20);
    assert_eq!(
        composer_columns(&rows),
        (24, 95),
        "the Landing keeps its independent 72-column composer"
    );
}

#[test]
fn centered_column_gutters_do_not_toggle_transcript_folds() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let (mut application, mut snapshot) =
        session_with_width(workspace.path(), SessionContentWidth::Maximum(60));
    let activity_id = snapshot.activities[0].id();
    let turn_id = snapshot.activities[0].turn_id();
    snapshot.session.status = SessionStatus::Active;
    snapshot.turns[0].status = TurnStatus::Active;
    snapshot.turns[0].settled_at = None;
    snapshot.activities[0] = Activity::Command {
        id: activity_id,
        turn_id,
        status: ActivityStatus::Completed,
        command: "cargo nextest run".to_owned(),
        cwd: None,
        output: (1..=12)
            .map(|line| format!("output line {line}"))
            .collect::<Vec<_>>()
            .join("\n"),
        output_truncated: false,
        exit_status: Some(0),
    };
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::Snapshot(snapshot)))
        .expect("attach a foldable command");

    let folded = rendered_application_rows_at(&application, 120, 24);
    let command_row = row_index(&folded, "cargo nextest run");
    click(&mut application, 5, command_row);
    assert!(
        !rendered_application_rows_at(&application, 120, 24)
            .join("\n")
            .contains("output line 12"),
        "a click in the left gutter changes no Transcript disclosure"
    );

    click(&mut application, 35, command_row);
    assert!(
        rendered_application_rows_at(&application, 120, 24)
            .join("\n")
            .contains("output line 12"),
        "the same row remains interactive inside the Session Content Column"
    );
}

#[test]
fn changing_width_reflows_an_open_sessions_composer() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let (mut application, _) =
        session_with_width(workspace.path(), SessionContentWidth::Maximum(50));
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "x".repeat(90),
        )))
        .expect("type a wrapping draft");
    let maximum = rendered_application_rows_at(&application, 120, 20);
    assert_eq!(composer_columns(&maximum), (35, 84));
    assert_eq!(composer_height(&maximum), 4);

    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::SettingsSnapshot(
            SettingsSnapshot {
                settings: EffectiveSettings {
                    session: SessionSettings {
                        content_width: SessionContentWidth::Fill,
                        ..SessionSettings::default()
                    },
                    ..EffectiveSettings::default()
                },
                pinned: vec!["session.contentWidth".to_owned()],
                diagnostics: Vec::new(),
            },
        )))
        .expect("receive a changed Session content width");
    let fill = rendered_application_rows_at(&application, 120, 20);
    assert_eq!(composer_columns(&fill), (2, 117));
    assert_eq!(composer_height(&fill), 3);
}
