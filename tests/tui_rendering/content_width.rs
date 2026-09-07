//! The configurable Session Content Column and the surfaces that share it.

use crate::support::{
    connected_application, enter_session, navigable_session_snapshot, rendered_application_rows_at,
    rendered_row, workspace_dir,
};
use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use suru::{
    managed_client::{ManagedEvent, SessionEvent},
    protocol::{
        Activity, ActivityStatus, EffectiveSettings, MessageRole, SessionContentWidth,
        SessionSettings, SessionStatus, SettingsSnapshot, SidebarSettings, SidebarVisibility,
        TurnStatus,
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
            content_width_snapshot(content_width, Vec::new()),
        )))
        .expect("receive Session content width");
    let (_, snapshot) = enter_session(&mut application, workspace);
    (application, snapshot)
}

fn composer_columns(rows: &[String]) -> (usize, usize) {
    let border = rows
        .iter()
        .find(|row| row.contains('┌'))
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
        .position(|row| row.contains('┌'))
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
    super::support::click_mouse(
        application,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        },
    )
    .expect("handle Transcript click");
}

fn change_content_width(application: &mut Application, content_width: SessionContentWidth) {
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::SettingsSnapshot(
            content_width_snapshot(content_width, vec!["session.contentWidth".to_owned()]),
        )))
        .expect("receive a changed Session content width");
}

fn content_width_snapshot(
    content_width: SessionContentWidth,
    pinned: Vec<String>,
) -> SettingsSnapshot {
    SettingsSnapshot {
        settings: EffectiveSettings {
            session: SessionSettings {
                content_width,
                ..SessionSettings::default()
            },
            // The Session Content Column is what every measurement here is
            // about, so the Sidebar stays off the frame rather than standing
            // in the middle of it.
            sidebar: SidebarSettings {
                initial_visibility: SidebarVisibility::Hidden,
                ..SidebarSettings::default()
            },
            ..EffectiveSettings::default()
        },
        pinned,
        diagnostics: Vec::new(),
    }
}

fn session_with_foldable_command(
    workspace: &std::path::Path,
    content_width: SessionContentWidth,
) -> Application {
    let (mut application, mut snapshot) = session_with_width(workspace, content_width);
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
        .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(snapshot)))
        .expect("attach a foldable command");
    application
}

fn session_with_reflowing_transcript(
    workspace: &std::path::Path,
    content_width: SessionContentWidth,
) -> Application {
    let (mut application, initial) = session_with_width(workspace, content_width);
    let mut snapshot = navigable_session_snapshot(initial.session.id, workspace, 12);
    for (index, message) in snapshot
        .messages
        .iter_mut()
        .filter(|message| message.role == MessageRole::Agent)
        .enumerate()
        .take(9)
    {
        message.content = format!("## Agent section {}\n\n{}", index + 1, "reflow ".repeat(60));
    }
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(snapshot)))
        .expect("attach a reflowing Transcript");
    application
}

fn command_output_is_visible(application: &Application, width: u16, height: u16) -> bool {
    rendered_application_rows_at(application, width, height)
        .join("\n")
        .contains("output line 12")
}

fn reflow_row_count(application: &Application) -> usize {
    rendered_application_rows_at(application, 120, 240)
        .iter()
        .filter(|row| row.contains("reflow"))
        .count()
}

#[test]
fn a_custom_maximum_centers_the_session_composer() {
    let workspace = workspace_dir();
    let (application, _) = session_with_width(workspace.path(), SessionContentWidth::Maximum(60));

    let rows = rendered_application_rows_at(&application, 120, 20);
    assert_eq!(composer_columns(&rows), (30, 89));
}

#[test]
fn default_and_fill_use_the_expected_session_content_column() {
    let workspace = workspace_dir();
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
    let workspace = workspace_dir();
    let (application, _) = session_with_width(workspace.path(), SessionContentWidth::Maximum(80));

    assert_eq!(
        composer_columns(&rendered_application_rows_at(&application, 40, 20)),
        (1, 38),
        "the 38 normally padded columns remain usable without clipping"
    );
}

#[test]
fn session_surfaces_share_the_column_while_the_header_keeps_terminal_width() {
    let workspace = workspace_dir();
    let (mut application, initial) =
        session_with_width(workspace.path(), SessionContentWidth::Maximum(60));
    let session_id = initial.session.id;
    let snapshot = navigable_session_snapshot(session_id, workspace.path(), 10);
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(snapshot)))
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
        "┌",
        "Agent unavailable",
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
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::SettingsSnapshot(
            content_width_snapshot(
                SessionContentWidth::Maximum(50),
                vec!["session.contentWidth".to_owned()],
            ),
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
    let workspace = workspace_dir();
    let mut application =
        session_with_foldable_command(workspace.path(), SessionContentWidth::Maximum(60));

    let folded = rendered_application_rows_at(&application, 120, 24);
    let command_row = row_index(&folded, "cargo nextest run");
    click(&mut application, 5, command_row);
    assert!(
        !command_output_is_visible(&application, 120, 24),
        "a click in the left gutter changes no Transcript disclosure"
    );

    click(&mut application, 110, command_row);
    assert!(
        !command_output_is_visible(&application, 120, 24),
        "a click in the right gutter changes no Transcript disclosure"
    );

    click(&mut application, 35, command_row);
    assert!(
        command_output_is_visible(&application, 120, 24),
        "the same row remains interactive inside the Session Content Column"
    );
}

#[test]
fn changing_width_reflows_an_open_sessions_composer() {
    let workspace = workspace_dir();
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

    change_content_width(&mut application, SessionContentWidth::Fill);
    let fill = rendered_application_rows_at(&application, 120, 20);
    assert_eq!(composer_columns(&fill), (2, 117));
    assert_eq!(composer_height(&fill), 3);
}

#[test]
fn changing_width_keeps_a_scrolled_reader_anchored_to_the_same_message_and_row() {
    let workspace = workspace_dir();
    let mut reflow_probe =
        session_with_reflowing_transcript(workspace.path(), SessionContentWidth::Maximum(50));
    let narrow_reflow_rows = reflow_row_count(&reflow_probe);
    assert!(narrow_reflow_rows > 0);
    change_content_width(&mut reflow_probe, SessionContentWidth::Maximum(70));
    let wider_maximum_reflow_rows = reflow_row_count(&reflow_probe);
    assert!(
        wider_maximum_reflow_rows < narrow_reflow_rows,
        "a larger Maximum must reproject the Transcript into fewer wrapped rows"
    );
    change_content_width(&mut reflow_probe, SessionContentWidth::Fill);
    let fill_reflow_rows = reflow_row_count(&reflow_probe);
    assert!(
        fill_reflow_rows < wider_maximum_reflow_rows,
        "Fill must reproject the Transcript into fewer wrapped rows than Maximum"
    );

    let mut application =
        session_with_reflowing_transcript(workspace.path(), SessionContentWidth::Maximum(50));

    rendered_application_rows_at(&application, 120, 20);
    application
        .handle_event(ApplicationEvent::Command(
            CommandId::ScrollTranscriptLinesUp,
        ))
        .expect("move away from the latest Transcript row");
    let maximum = rendered_application_rows_at(&application, 120, 20);
    let anchor_row = rendered_row(&maximum, "Prompt section 12");

    change_content_width(&mut application, SessionContentWidth::Maximum(70));

    let wider_maximum = rendered_application_rows_at(&application, 120, 20);
    assert_eq!(
        rendered_row(&wider_maximum, "Prompt section 12"),
        anchor_row,
        "the logical Message anchor stays on its screen row between maximum values"
    );

    change_content_width(&mut application, SessionContentWidth::Fill);

    let fill = rendered_application_rows_at(&application, 120, 20);
    assert_eq!(
        rendered_row(&fill, "Prompt section 12"),
        anchor_row,
        "the logical Message anchor stays on its screen row after reflow"
    );
    assert!(fill.join("\n").contains("Latest ↓"));
}

#[test]
fn changing_width_keeps_a_tail_following_reader_at_the_latest_message() {
    let workspace = workspace_dir();
    let mut application =
        session_with_reflowing_transcript(workspace.path(), SessionContentWidth::Maximum(50));
    let maximum = rendered_application_rows_at(&application, 120, 20).join("\n");
    assert!(maximum.contains("Agent section 12"));
    assert!(!maximum.contains("Latest ↓"));

    change_content_width(&mut application, SessionContentWidth::Fill);

    let fill = rendered_application_rows_at(&application, 120, 20).join("\n");
    assert!(fill.contains("Agent section 12"));
    assert!(!fill.contains("Latest ↓"));
}

#[test]
fn transcript_pointer_geometry_tracks_width_changes_and_terminal_resize() {
    let workspace = workspace_dir();
    let mut application =
        session_with_foldable_command(workspace.path(), SessionContentWidth::Maximum(60));

    change_content_width(&mut application, SessionContentWidth::Fill);
    let fill = rendered_application_rows_at(&application, 120, 24);
    let fill_command_row = row_index(&fill, "cargo nextest run");
    click(&mut application, 5, fill_command_row);
    assert!(
        command_output_is_visible(&application, 120, 24),
        "a former gutter column becomes interactive when Fill reaches it"
    );
    click(&mut application, 5, fill_command_row);

    change_content_width(&mut application, SessionContentWidth::Maximum(50));
    let maximum = rendered_application_rows_at(&application, 120, 24);
    let maximum_command_row = row_index(&maximum, "cargo nextest run");
    click(&mut application, 5, maximum_command_row);
    assert!(
        !command_output_is_visible(&application, 120, 24),
        "switching back to Maximum makes the centered gutter inert"
    );

    let resized = rendered_application_rows_at(&application, 90, 24);
    let resized_command_row = row_index(&resized, "cargo nextest run");
    click(&mut application, 75, resized_command_row);
    assert!(
        !command_output_is_visible(&application, 90, 24),
        "the resized right gutter is inert"
    );
    click(&mut application, 25, resized_command_row);
    assert!(
        command_output_is_visible(&application, 90, 24),
        "the resized Session Content Column remains interactive"
    );
}
