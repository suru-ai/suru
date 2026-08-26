//! The Sidebar: its column beside both views, the toggle, the launch Setting,
//! and the shape of an active row.

use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::support::{
    connected_application, enter_session, failed_session_snapshot, rendered_application_buffer,
    rendered_application_rows_at, rendered_row, type_terminal_text,
};
use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{buffer::Cell, style::Color};
use suru::{
    managed_client::ManagedEvent,
    protocol::{
        EffectiveSettings, ModelAvailability, PromptId, Session, SessionDeleted, SessionId,
        SessionListItem, SessionStatus, SessionSummary, SessionTimestamp, SettingsSnapshot,
        SidebarSettings, SidebarVisibility, UnreadableSessionSummary, Workspace,
    },
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, SemanticCommandId,
        SessionListScope, SessionListSurface,
    },
};

/// Wide enough for the Sidebar and a main view both, which is what every test
/// about the Sidebar's own content needs.
const WIDE: u16 = 100;

#[test]
fn the_sidebar_stands_beside_the_landing_and_the_main_view_takes_what_is_left() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let application = sidebar_showing(workspace.path(), Vec::new());

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(
        sidebar_is_drawn(&rows),
        "the Sidebar takes a fixed 32-column left column, divided down the frame: {rows:?}"
    );
    let landing = rendered_row(&rows, "What would you like to work on?");
    assert!(
        rows[landing].find("What").expect("the question is drawn") > 31,
        "the Landing lays itself out in the columns the Sidebar left: {:?}",
        rows[landing]
    );
}

#[test]
fn the_sidebar_stands_beside_an_open_session_too() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_showing(workspace.path(), Vec::new());
    enter_session(&mut application, workspace.path());

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    let composer = rendered_row(&rows, "┌");
    assert!(
        rows[composer].find('┌').expect("the composer is drawn") > 31,
        "an open Session lays itself out in the columns the Sidebar left: {:?}",
        rows[composer]
    );
    assert!(
        sidebar_is_drawn(&rows),
        "the Sidebar stands beside a Session as it does beside the Landing: {rows:?}"
    );
}

#[test]
fn an_active_row_is_three_lines_of_workspace_time_emoji_and_title() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let application = sidebar_showing(
        workspace.path(),
        vec![listed(
            "Sidebar shell",
            Some("🧪"),
            &workspace.path().join("suru"),
            1,
            minutes_ago(5),
        )],
    );

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    let first = rendered_row(&rows, "suru");
    let workspace_line = sidebar_column(&rows[first]);
    assert!(
        workspace_line.starts_with("suru"),
        "the row leads with the Workspace the work is in: {workspace_line:?}"
    );
    assert!(
        workspace_line.ends_with("5m"),
        "the right slot carries the compact time since the Session moved: {workspace_line:?}"
    );
    let title_line = sidebar_column(&rows[first + 1]);
    assert!(
        title_line.starts_with('🧪') && title_line.ends_with("Sidebar shell"),
        "the second line carries the Emoji and then the Title: {title_line:?}"
    );
    assert_eq!(
        sidebar_column(&rows[first + 2]),
        "",
        "the third line is held blank for git awareness"
    );
}

#[test]
fn the_compact_time_reads_now_minutes_hours_and_days() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let application = sidebar_showing(
        workspace.path(),
        vec![
            listed("Just now", None, &workspace.path().join("a"), 4, now()),
            listed(
                "Minutes",
                None,
                &workspace.path().join("b"),
                3,
                minutes_ago(5),
            ),
            listed("Hours", None, &workspace.path().join("c"), 2, hours_ago(3)),
            listed("Days", None, &workspace.path().join("d"), 1, days_ago(2)),
        ],
    );

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    for (title, elapsed) in [
        ("Just now", "now"),
        ("Minutes", "5m"),
        ("Hours", "3h"),
        ("Days", "2d"),
    ] {
        let row = rendered_row(&rows, title);
        assert!(
            sidebar_column(&rows[row - 1]).ends_with(elapsed),
            "{title:?} reads as {elapsed:?}: {:?}",
            sidebar_column(&rows[row - 1])
        );
    }
}

#[test]
fn the_list_is_ordered_by_creation_and_activity_never_reorders_it() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let application = sidebar_showing(
        workspace.path(),
        vec![
            listed("Oldest", None, workspace.path(), 1, now()),
            listed("Newest", None, workspace.path(), 3, days_ago(2)),
            listed("Middle", None, workspace.path(), 2, hours_ago(3)),
        ],
    );

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(rendered_row(&rows, "Newest") < rendered_row(&rows, "Middle"));
    assert!(rendered_row(&rows, "Middle") < rendered_row(&rows, "Oldest"));
}

#[test]
fn ctrl_b_hides_the_sidebar_and_shows_it_again_without_touching_the_setting() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_showing(
        workspace.path(),
        vec![listed("Listed work", None, workspace.path(), 1, now())],
    );

    assert_eq!(
        press_toggle(&mut application),
        ApplicationTransition::Continue,
        "hiding the Sidebar asks the server for nothing, least of all a Setting edit"
    );
    let hidden = rendered_application_rows_at(&application, WIDE, 20);
    assert!(
        !hidden.iter().any(|row| row.contains("Listed work")),
        "the Sidebar is off the frame"
    );
    assert!(
        !sidebar_is_drawn(&hidden),
        "the main view has the whole frame back: {hidden:?}"
    );

    let request = expect_sidebar_listing(press_toggle(&mut application));
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request,
            sessions: vec![listed("Listed work", None, workspace.path(), 1, now())],
        })
        .expect("hydrate the reopened Sidebar");
    assert!(
        rendered_application_rows_at(&application, WIDE, 20)
            .iter()
            .any(|row| row.contains("Listed work")),
        "showing the Sidebar again lists Sessions afresh"
    );
}

#[test]
fn the_slash_command_toggles_the_same_sidebar_the_keybinding_does() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_showing(
        workspace.path(),
        vec![listed("Listed work", None, workspace.path(), 1, now())],
    );
    type_terminal_text(&mut application, "/sidebar");

    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("select /sidebar");

    assert!(
        !rendered_application_rows_at(&application, WIDE, 20)
            .iter()
            .any(|row| row.contains("Listed work")),
        "the slash command hides the Sidebar the keybinding hides"
    );
}

#[test]
fn a_sidebar_the_setting_hides_is_absent_until_the_reader_asks_for_it() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = connected_application(workspace.path());

    assert_eq!(
        deliver_launch_visibility(&mut application, SidebarVisibility::Hidden),
        ApplicationTransition::Continue,
        "a Sidebar nobody can see asks for no Sessions"
    );
    assert!(
        !sidebar_is_drawn(&rendered_application_rows_at(&application, WIDE, 20)),
        "the first frame honors the launch Setting"
    );

    expect_sidebar_listing(press_toggle(&mut application));
    assert!(
        sidebar_is_drawn(&rendered_application_rows_at(&application, WIDE, 20)),
        "the toggle overrides the launch Setting for this run"
    );
}

#[test]
fn a_later_settings_snapshot_leaves_the_readers_own_choice_alone() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_showing(workspace.path(), Vec::new());
    press_toggle(&mut application);

    deliver_launch_visibility(&mut application, SidebarVisibility::Shown);

    assert!(
        !sidebar_is_drawn(&rendered_application_rows_at(&application, WIDE, 20)),
        "some other Setting's edit does not reopen a Sidebar the reader closed"
    );
}

#[test]
fn a_terminal_too_narrow_for_both_keeps_the_main_view_and_forgets_nothing() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let application = sidebar_showing(
        workspace.path(),
        vec![listed("Listed work", None, workspace.path(), 1, now())],
    );

    let squeezed = rendered_application_rows_at(&application, 85, 20);
    assert!(
        !squeezed.iter().any(|row| row.contains("Listed work")),
        "a terminal too narrow for the Sidebar plus a usable main view keeps the main view"
    );
    assert!(
        squeezed
            .iter()
            .any(|row| row.contains("What would you like to work on?")),
        "the main view is drawn in full: {squeezed:?}"
    );

    let widened = rendered_application_rows_at(&application, 86, 20);
    assert!(
        widened.iter().any(|row| row.contains("Listed work")),
        "the Sidebar comes back the moment there is room, the reader's choice intact"
    );
}

#[test]
fn the_landing_footer_is_spread_across_the_columns_the_sidebar_left() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let application = sidebar_showing(workspace.path(), Vec::new());

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    let footer = rendered_row(&rows, "Connected");
    assert!(
        rows[footer]
            .find("Connected")
            .expect("the connection status is drawn")
            > 31,
        "the footer is laid out in the main view, not the whole frame: {:?}",
        rows[footer]
    );
    assert!(
        rows[footer].contains("| server "),
        "the footer spreads across the main view's own width, so its right-hand end lands inside \
         the frame rather than being cut off it: {:?}",
        rows[footer]
    );
}

#[test]
fn a_session_deleted_elsewhere_leaves_the_sidebar_it_was_listed_in() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = connected_application(workspace.path());
    let request = expect_sidebar_listing(deliver_launch_visibility(
        &mut application,
        SidebarVisibility::Shown,
    ));
    let deleted = SessionId::new();
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request,
            sessions: vec![
                listed_as(deleted, "Deleted elsewhere", workspace.path(), 2),
                listed_as(SessionId::new(), "Still here", workspace.path(), 1),
            ],
        })
        .expect("hydrate the Sidebar");

    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::SessionDeleted(
            SessionDeleted {
                session_id: deleted,
            },
        )))
        .expect("take the deletion another client made");

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(
        !rows.iter().any(|row| row.contains("Deleted elsewhere")),
        "a Session another client deleted leaves the Sidebar listing it"
    );
    assert!(
        rows.iter().any(|row| row.contains("Still here")),
        "the Sessions that survive stay listed"
    );
}

#[test]
fn an_overlay_opens_over_the_main_view_and_never_over_the_sidebar() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_showing(
        workspace.path(),
        vec![listed("Listed work", None, workspace.path(), 1, now())],
    );

    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::SessionList,
        )))
        .expect("open the Session picker");

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(
        rows.iter().any(|row| row.contains("Sessions")),
        "the picker is on screen: {rows:?}"
    );
    assert!(
        sidebar_is_drawn(&rows),
        "an overlay is centered on the main view and leaves the Sidebar standing: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("Listed work")),
        "the Sidebar's own rows are still readable beside the overlay"
    );
}

/// A connected client whose Sidebar is open on the Sessions given.
fn sidebar_showing(workspace: &Path, sessions: Vec<SessionListItem>) -> Application {
    let mut application = connected_application(workspace);
    let request = expect_sidebar_listing(deliver_launch_visibility(
        &mut application,
        SidebarVisibility::Shown,
    ));
    application
        .handle_event(ApplicationEvent::SessionsListed { request, sessions })
        .expect("hydrate the Sidebar");
    application
}

fn deliver_launch_visibility(
    application: &mut Application,
    launch_visibility: SidebarVisibility,
) -> ApplicationTransition {
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::SettingsSnapshot(
            SettingsSnapshot {
                settings: EffectiveSettings {
                    sidebar: SidebarSettings { launch_visibility },
                    ..EffectiveSettings::default()
                },
                pinned: Vec::new(),
                diagnostics: Vec::new(),
            },
        )))
        .expect("receive the effective-settings snapshot")
}

fn press_toggle(application: &mut Application) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('b'),
            KeyModifiers::CONTROL,
        )))
        .expect("press Ctrl+B")
}

fn expect_sidebar_listing(transition: ApplicationTransition) -> suru::tui::SessionListRequest {
    let ApplicationTransition::ListSessions(request) = transition else {
        panic!("a Sidebar coming into view asks for its Sessions, not {transition:?}");
    };
    assert_eq!(request.surface(), SessionListSurface::Sidebar);
    assert_eq!(
        request.scope(),
        &SessionListScope::AllWorkspaces,
        "the Sidebar opens on the reader's whole body of work"
    );
    request
}

/// Whether the Sidebar's divider runs the whole height of the frame, which is
/// how a Sidebar on screen is told from the composer's own box borders.
fn sidebar_is_drawn(rows: &[String]) -> bool {
    rows.iter()
        .all(|row| row.chars().nth(31) == Some('\u{2502}'))
}

/// The Sidebar's own columns of one rendered row, trimmed of the padding that
/// insets them from the divider.
fn sidebar_column(row: &str) -> String {
    row.chars()
        .take_while(|character| *character != '│')
        .collect::<String>()
        .trim()
        .to_owned()
}

fn listed_as(
    session_id: SessionId,
    title: &str,
    workspace: &Path,
    created_at: u64,
) -> SessionListItem {
    let SessionListItem::Readable(mut summary) = listed(title, None, workspace, created_at, now())
    else {
        unreachable!("the fixture builds a readable Session");
    };
    summary.session.id = session_id;
    SessionListItem::Readable(summary)
}

fn listed(
    title: &str,
    emoji: Option<&str>,
    workspace: &Path,
    created_at: u64,
    updated_at: u64,
) -> SessionListItem {
    SessionListItem::Readable(SessionSummary {
        session: Session {
            id: SessionId::new(),
            workspace: Workspace {
                path: workspace.to_owned(),
            },
            agent_selection: None,
            agent_selection_availability: ModelAvailability::Available,
            status: SessionStatus::Idle,
        },
        title: title.to_owned(),
        emoji: emoji.map(str::to_owned),
        settled_at: None,
        created_at: SessionTimestamp(created_at),
        updated_at: SessionTimestamp(updated_at),
    })
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("read the clock")
        .as_millis()
        .try_into()
        .expect("the clock fits a Session timestamp")
}

fn minutes_ago(minutes: u64) -> u64 {
    now().saturating_sub(minutes * 60 * 1_000)
}

fn hours_ago(hours: u64) -> u64 {
    minutes_ago(hours * 60)
}

fn days_ago(days: u64) -> u64 {
    hours_ago(days * 24)
}

#[test]
fn the_sidebar_survives_every_terminal_the_frame_will_draw() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let application = sidebar_showing(
        workspace.path(),
        vec![listed(
            "A Title long enough to run past the Sidebar's own columns",
            Some("🧪"),
            &workspace.path().join("suru"),
            1,
            days_ago(400),
        )],
    );

    for width in [1_u16, 28, 85, 86, 87, 200] {
        for height in [1_u16, 5, 6, 40] {
            rendered_application_rows_at(&application, width, height);
        }
    }
}

#[test]
fn the_launch_setting_shows_the_sidebar_without_taking_the_keys() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_showing(
        workspace.path(),
        vec![listed("Listed work", None, workspace.path(), 1, now())],
    );

    type_terminal_text(&mut application, "hello");

    assert!(
        rendered_application_rows_at(&application, WIDE, 20)
            .iter()
            .any(|row| row.contains("hello")),
        "a Sidebar the reader never opened leaves them typing where they were"
    );
}

#[test]
fn opening_the_sidebar_takes_the_keys_from_the_composer() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_focused(
        workspace.path(),
        vec![listed("Listed work", None, workspace.path(), 1, now())],
    );

    type_terminal_text(&mut application, "hello");

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(
        !rows.iter().any(|row| row.contains("hello")),
        "the Sidebar the reader opened has the keys, so nothing reaches the composer: {rows:?}"
    );
    assert!(
        selected_sidebar_text(&application).contains("Listed work"),
        "the row the reader would act on stands out from the rest of the column"
    );
}

#[test]
fn the_arrows_move_the_selection_and_wrap_past_the_ends() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_focused(
        workspace.path(),
        vec![
            listed("Newest", None, workspace.path(), 3, now()),
            listed("Middle", None, workspace.path(), 2, now()),
            listed("Oldest", None, workspace.path(), 1, now()),
        ],
    );

    assert!(
        selected_sidebar_text(&application).contains("Newest"),
        "an opened Sidebar starts on the row nearest the reader"
    );

    press_sidebar_key(&mut application, KeyCode::Down);
    assert!(selected_sidebar_text(&application).contains("Middle"));

    press_sidebar_key(&mut application, KeyCode::Up);
    assert!(selected_sidebar_text(&application).contains("Newest"));

    press_sidebar_key(&mut application, KeyCode::Up);
    assert!(
        selected_sidebar_text(&application).contains("Oldest"),
        "moving off the top wraps to the end of the list"
    );
}

#[test]
fn the_column_windows_onto_the_selection_for_a_list_longer_than_it() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let sessions = (1..=6)
        .map(|index| {
            listed(
                &format!("Row {}", 7 - index),
                None,
                workspace.path(),
                index,
                now(),
            )
        })
        .collect();
    let mut application = sidebar_focused(workspace.path(), sessions);

    let opening = rendered_application_rows_at(&application, WIDE, 12);
    assert!(
        opening.iter().any(|row| row.contains("Row 1")),
        "the list opens at the top: {opening:?}"
    );
    assert!(
        !opening.iter().any(|row| row.contains("Row 6")),
        "a list longer than the column is drawn through a window, not crammed in: {opening:?}"
    );

    for _ in 0..5 {
        press_sidebar_key(&mut application, KeyCode::Down);
    }

    let scrolled = rendered_application_rows_at(&application, WIDE, 12);
    assert!(
        scrolled.iter().any(|row| row.contains("Row 6")),
        "moving past the window's end brings the selected row into view: {scrolled:?}"
    );
    assert!(
        !scrolled.iter().any(|row| row.contains("Row 1")),
        "the window moved rather than growing: {scrolled:?}"
    );
}

#[test]
fn enter_attaches_the_selected_session_in_place() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let wanted = SessionId::new();
    let mut application = sidebar_focused(
        workspace.path(),
        vec![
            listed_as(SessionId::new(), "Nearest work", workspace.path(), 3),
            listed_as(wanted, "The work wanted", workspace.path(), 2),
            listed_as(SessionId::new(), "Older work", workspace.path(), 1),
        ],
    );

    press_sidebar_key(&mut application, KeyCode::Down);

    assert_eq!(
        press_sidebar_key(&mut application, KeyCode::Enter),
        ApplicationTransition::AttachSession(wanted),
        "Enter attaches the Session the reader has selected"
    );
}

#[test]
fn the_attached_session_hands_the_keys_back_to_the_composer() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let wanted = SessionId::new();
    let mut application = sidebar_focused(
        workspace.path(),
        vec![listed_as(wanted, "The work wanted", workspace.path(), 1)],
    );
    press_sidebar_key(&mut application, KeyCode::Enter);

    application
        .handle_event(ApplicationEvent::SessionAttached(failed_session_snapshot(
            wanted,
            PromptId::new(),
            "Initial Prompt",
            workspace.path(),
        )))
        .expect("attach the selected Session");

    type_terminal_text(&mut application, "hello");
    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(
        rows.iter().any(|row| row.contains("hello")),
        "the reader is done choosing, so the composer takes the keys back: {rows:?}"
    );
    assert!(
        sidebar_is_drawn(&rows),
        "the Sidebar stays beside the Session it just opened"
    );
}

#[test]
fn esc_hands_the_keys_back_without_hiding_the_sidebar() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_focused(
        workspace.path(),
        vec![listed("Listed work", None, workspace.path(), 1, now())],
    );

    assert_eq!(
        press_sidebar_key(&mut application, KeyCode::Esc),
        ApplicationTransition::Continue
    );

    type_terminal_text(&mut application, "hello");
    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(
        rows.iter().any(|row| row.contains("hello")),
        "Esc hands the keys back to the composer: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("Listed work")) && sidebar_is_drawn(&rows),
        "the Sidebar stays standing, only without the keys: {rows:?}"
    );
    assert!(
        selected_sidebar_text(&application).is_empty(),
        "nothing in the column claims the keys any more"
    );
    assert!(
        sidebar_text_on(&application, Color::DarkGray).contains("Listed work"),
        "the row the reader left off on is still marked, dimly, because it is still the row \
         Enter would act on"
    );
}

#[test]
fn the_toggle_closes_the_sidebar_from_inside_it() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_focused(
        workspace.path(),
        vec![listed("Listed work", None, workspace.path(), 1, now())],
    );

    press_toggle(&mut application);

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(
        !sidebar_is_drawn(&rows),
        "the toggle closes the Sidebar from within it as readily as from the composer: {rows:?}"
    );
    type_terminal_text(&mut application, "hello");
    assert!(
        rendered_application_rows_at(&application, WIDE, 20)
            .iter()
            .any(|row| row.contains("hello")),
        "a closed Sidebar holds no keys"
    );
}

#[test]
fn an_open_overlay_keeps_the_keys_while_the_sidebar_holds_focus() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_focused(
        workspace.path(),
        vec![
            listed("Nearest work", None, workspace.path(), 2, now()),
            listed("Older work", None, workspace.path(), 1, now()),
        ],
    );
    let ApplicationTransition::ListSessions(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::SessionList,
        )))
        .expect("open the Session picker")
    else {
        panic!("opening the picker asks for its own listing");
    };
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request,
            sessions: vec![listed("Nearest work", None, workspace.path(), 2, now())],
        })
        .expect("hydrate the picker");

    press_sidebar_key(&mut application, KeyCode::Down);

    assert!(
        selected_sidebar_text(&application).contains("Nearest work"),
        "an overlay over the main view owns the keys, so the Sidebar's selection stays put"
    );
}

#[test]
fn a_session_suru_cannot_read_is_not_attached() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_focused(
        workspace.path(),
        vec![SessionListItem::Unreadable(UnreadableSessionSummary {
            id: SessionId::new(),
            title: "Unreadable work".to_owned(),
            created_at: SessionTimestamp(1),
            updated_at: SessionTimestamp(now()),
            workspace: Some(Workspace {
                path: workspace.path().to_owned(),
            }),
        })],
    );

    assert_eq!(
        press_sidebar_key(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue,
        "there is nothing to attach to in a Session Suru could not read"
    );
}

#[test]
fn a_terminal_too_narrow_to_draw_the_sidebar_leaves_the_keys_with_the_composer() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_focused(
        workspace.path(),
        vec![listed("Listed work", None, workspace.path(), 1, now())],
    );

    rendered_application_rows_at(&application, 85, 20);
    type_terminal_text(&mut application, "hello");

    assert!(
        rendered_application_rows_at(&application, 85, 20)
            .iter()
            .any(|row| row.contains("hello")),
        "a Sidebar the frame cannot spare the columns for cannot hold the keys either"
    );
}

#[test]
fn a_paste_does_not_slip_past_the_focused_sidebar() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_focused(
        workspace.path(),
        vec![listed("Listed work", None, workspace.path(), 1, now())],
    );

    application
        .handle_terminal_event(InputEvent::Paste("pasted".to_owned()))
        .expect("paste while the Sidebar has the keys");

    assert!(
        !rendered_application_rows_at(&application, WIDE, 20)
            .iter()
            .any(|row| row.contains("pasted")),
        "nothing a reader types or pastes reaches a composer that does not have the keys"
    );
}

#[test]
fn the_keys_after_the_toggle_reach_the_sidebar_before_the_next_frame() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = connected_application(workspace.path());
    deliver_launch_visibility(&mut application, SidebarVisibility::Hidden);
    // A frame with no Sidebar on it, which is the state the toggle acts from.
    rendered_application_rows_at(&application, WIDE, 20);
    let request = expect_sidebar_listing(press_toggle(&mut application));
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request,
            sessions: vec![
                listed("Newest", None, workspace.path(), 2, now()),
                listed("Older", None, workspace.path(), 1, now()),
            ],
        })
        .expect("hydrate the Sidebar");

    press_sidebar_key(&mut application, KeyCode::Down);

    assert!(
        selected_sidebar_text(&application).contains("Older"),
        "a run of keys the terminal delivered together reaches the Sidebar the first of them \
         opened, without waiting for a frame to be drawn between them"
    );
}

#[test]
fn the_window_holds_still_while_the_selection_moves_inside_it() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let sessions = (1..=6)
        .map(|index| {
            listed(
                &format!("Row {}", 7 - index),
                None,
                workspace.path(),
                index,
                now(),
            )
        })
        .collect();
    let mut application = sidebar_focused(workspace.path(), sessions);
    // Only a frame knows how many rows the column holds, so a frame is what
    // settles the window — one is drawn after each run of keys, as the run
    // loop draws after each run of terminal events.
    rendered_application_rows_at(&application, WIDE, 12);
    for _ in 0..5 {
        press_sidebar_key(&mut application, KeyCode::Down);
        rendered_application_rows_at(&application, WIDE, 12);
    }

    press_sidebar_key(&mut application, KeyCode::Up);

    let rows = rendered_application_rows_at(&application, WIDE, 12);
    assert!(
        rows.iter().any(|row| row.contains("Row 5")),
        "the row the reader moved onto is in view: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("Row 3")),
        "a selection moving to a row already in view leaves the window where it was: {rows:?}"
    );
    assert!(
        !rows.iter().any(|row| row.contains("Row 2")),
        "the window did not follow the selection it never left: {rows:?}"
    );
}

/// A connected client whose Sidebar the reader opened themselves, which is the
/// Sidebar that has the keys.
fn sidebar_focused(workspace: &Path, sessions: Vec<SessionListItem>) -> Application {
    let mut application = connected_application(workspace);
    deliver_launch_visibility(&mut application, SidebarVisibility::Hidden);
    let request = expect_sidebar_listing(press_toggle(&mut application));
    application
        .handle_event(ApplicationEvent::SessionsListed { request, sessions })
        .expect("hydrate the Sidebar");
    application
}

fn press_sidebar_key(application: &mut Application, code: KeyCode) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
        .expect("press a Sidebar key")
}

/// The Sidebar text this frame draws on `background`. The row the reader is on
/// is drawn highlighted while they are driving the Sidebar, and dimly once the
/// keys have gone back to the composer, so the two backgrounds tell where the
/// selection is from where the keys are.
fn sidebar_text_on(application: &Application, background: Color) -> String {
    let buffer = rendered_application_buffer(application, WIDE, 20);
    (0..20)
        .map(|row| {
            (0..31)
                .filter_map(|column| buffer.cell((column, row)))
                .filter(|cell| cell.bg == background)
                .map(Cell::symbol)
                .collect::<String>()
        })
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The Sidebar row the reader is on while they are driving the Sidebar.
fn selected_sidebar_text(application: &Application) -> String {
    sidebar_text_on(application, Color::Blue)
}
