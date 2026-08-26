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
use crossterm::event::{
    Event as InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::{buffer::Cell, style::Color};
use suru::{
    managed_client::ManagedEvent,
    protocol::{
        AutoSettle, EffectiveSettings, ModelAvailability, PromptId, Session, SessionDeleted,
        SessionId, SessionListItem, SessionSettlementChanged, SessionStatus, SessionSummary,
        SessionTimestamp, SessionTitleChanged, SettingsSnapshot, SidebarScope, SidebarSettings,
        SidebarVisibility, UnreadableSessionSummary, Workspace,
    },
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, SemanticCommandId,
        SessionListScope, SessionListSurface,
    },
};

/// Wide enough for the Sidebar and a main view both, which is what every test
/// about the Sidebar's own content needs.
const WIDE: u16 = 100;

/// Short enough that six active rows do not fit: the search box, the selector,
/// and three three-line rows fill the column, so the rest is read through the
/// window.
const WINDOWED: u16 = 13;

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
    // What a row reads is the question here, so nothing settles itself and
    // every Session keeps the active row the reading is drawn on.
    let application = sidebar_settling(
        workspace.path(),
        AutoSettle::Off,
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
    // The order of the active list is the question, so nothing settles itself
    // and the whole listing stays on it however long ago each Session moved.
    let application = sidebar_settling(
        workspace.path(),
        AutoSettle::Off,
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

/// A connected client whose Sidebar is open on the Sessions given, settling
/// idle work the way the built-in defaults do.
fn sidebar_showing(workspace: &Path, sessions: Vec<SessionListItem>) -> Application {
    sidebar_settling(workspace, AutoSettle::default(), sessions)
}

/// The same, under the auto-settle Setting the reader has chosen.
fn sidebar_settling(
    workspace: &Path,
    auto_settle: AutoSettle,
    sessions: Vec<SessionListItem>,
) -> Application {
    let mut application = connected_application(workspace);
    let request = expect_sidebar_listing(deliver_sidebar_settings(
        &mut application,
        SidebarSettings {
            launch_visibility: SidebarVisibility::Shown,
            auto_settle,
            ..SidebarSettings::default()
        },
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
    deliver_sidebar_settings(
        application,
        SidebarSettings {
            launch_visibility,
            ..SidebarSettings::default()
        },
    )
}

fn deliver_sidebar_settings(
    application: &mut Application,
    sidebar: SidebarSettings,
) -> ApplicationTransition {
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::SettingsSnapshot(
            SettingsSnapshot {
                settings: EffectiveSettings {
                    sidebar,
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
        vec![
            listed(
                "A Title long enough to run past the Sidebar's own columns",
                Some("🧪"),
                &workspace.path().join("suru"),
                2,
                days_ago(400),
            ),
            settled(
                "A settled Title long enough to run past them too",
                Some("🧪"),
                &workspace.path().join("suru"),
                1,
                days_ago(400),
                days_ago(399),
            ),
        ],
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

    type_terminal_text(&mut application, "listed");

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(
        rows.iter()
            .any(|row| row.contains("Type a Prompt and press Enter")),
        "the Sidebar the reader opened has the keys, so nothing reaches the composer: {rows:?}"
    );
    assert!(
        drawn_in_sidebar(&rows, "Search: listed"),
        "what they type goes to the Sidebar's own search box: {rows:?}"
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
        selected_sidebar_text(&application).contains(ALL_WORKSPACES),
        "moving off the top of the list lands on the selector standing above it"
    );

    press_sidebar_key(&mut application, KeyCode::Up);
    assert!(
        selected_sidebar_text(&application).contains("Oldest"),
        "and moving off the selector wraps to the end of the list"
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

    let opening = rendered_application_rows_at(&application, WIDE, WINDOWED);
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

    let scrolled = rendered_application_rows_at(&application, WIDE, WINDOWED);
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

/// A Title carried in from somewhere else is as good a way to find a Session
/// as one the reader types out, so a paste goes to the search box whole — and
/// not, on any account, to the composer that does not have the keys.
#[test]
fn a_paste_goes_to_the_search_box_rather_than_the_composer() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_focused(
        workspace.path(),
        vec![
            listed("Listed work", None, workspace.path(), 2, now()),
            listed("Other work", None, workspace.path(), 1, now()),
        ],
    );

    application
        .handle_terminal_event(InputEvent::Paste("Listed".to_owned()))
        .expect("paste while the Sidebar has the keys");

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(
        drawn_in_sidebar(&rows, "Search: Listed"),
        "the paste lands in the search box whole: {rows:?}"
    );
    assert!(
        !drawn_in_sidebar(&rows, "Other work"),
        "and narrows the list as typing it would have: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("Type a Prompt and press Enter")),
        "nothing of it reaches a composer that does not have the keys: {rows:?}"
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
    rendered_application_rows_at(&application, WIDE, WINDOWED);
    for _ in 0..5 {
        press_sidebar_key(&mut application, KeyCode::Down);
        rendered_application_rows_at(&application, WIDE, WINDOWED);
    }

    press_sidebar_key(&mut application, KeyCode::Up);

    let rows = rendered_application_rows_at(&application, WIDE, WINDOWED);
    assert!(
        rows.iter().any(|row| row.contains("Row 5")),
        "the row the reader moved onto is in view: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("Row 4")),
        "a selection moving to a row already in view leaves the window where it was: {rows:?}"
    );
    assert!(
        !rows.iter().any(|row| row.contains("Row 3")),
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

#[test]
fn a_settled_session_stands_below_the_divider_as_one_slim_line() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let application = sidebar_showing(
        workspace.path(),
        vec![
            listed("Active work", None, workspace.path(), 2, now()),
            settled(
                "Wrapped up",
                Some("🧪"),
                workspace.path(),
                1,
                hours_ago(3),
                minutes_ago(5),
            ),
            settled(
                "Wrapped up earlier",
                None,
                workspace.path(),
                3,
                hours_ago(9),
                hours_ago(8),
            ),
        ],
    );

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    let divider = sidebar_divider(&rows);
    assert!(
        rendered_row(&rows, "Active work") < divider,
        "the Sessions still in flight stand above the divider: {rows:?}"
    );
    let shelf = rendered_row(&rows, "Wrapped up");
    assert_eq!(
        shelf,
        divider + 1,
        "the settled shelf opens on the line below the divider: {rows:?}"
    );
    let row = sidebar_column(&rows[shelf]);
    assert!(
        row.starts_with('🧪') && row.contains("Wrapped up"),
        "a settled row leads with the Emoji and the Title: {row:?}"
    );
    assert!(
        row.ends_with("5m"),
        "and closes with how long ago the work ended: {row:?}"
    );
    assert_eq!(
        rendered_row(&rows, "Wrapped up earlier"),
        shelf + 1,
        "a settled Session takes one slim line, so the next one is the line below it: {rows:?}"
    );
}

#[test]
fn the_settled_shelf_orders_by_when_the_work_ended() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    // Creation order and activity order both disagree with the order the work
    // ended in, so a shelf reading either would be caught out.
    let application = sidebar_showing(
        workspace.path(),
        vec![
            settled(
                "Ended first",
                None,
                workspace.path(),
                3,
                minutes_ago(1),
                hours_ago(9),
            ),
            settled(
                "Ended last",
                None,
                workspace.path(),
                1,
                hours_ago(20),
                minutes_ago(2),
            ),
            settled(
                "Ended in between",
                None,
                workspace.path(),
                2,
                hours_ago(10),
                hours_ago(4),
            ),
        ],
    );

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(
        rendered_row(&rows, "Ended last") < rendered_row(&rows, "Ended in between"),
        "the work that wrapped up most recently is nearest the divider: {rows:?}"
    );
    assert!(
        rendered_row(&rows, "Ended in between") < rendered_row(&rows, "Ended first"),
        "and the work that wrapped up longest ago is furthest from it: {rows:?}"
    );
}

#[test]
fn a_session_settled_elsewhere_moves_shelves_and_comes_back_when_it_is_unsettled() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = connected_application(workspace.path());
    let request = expect_sidebar_listing(deliver_launch_visibility(
        &mut application,
        SidebarVisibility::Shown,
    ));
    let set_aside = SessionId::new();
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request,
            sessions: vec![
                listed_as(set_aside, "Work set aside", workspace.path(), 2),
                listed_as(SessionId::new(), "Work still going", workspace.path(), 1),
            ],
        })
        .expect("hydrate the Sidebar");
    assert!(
        sidebar_divider_row(&rendered_application_rows_at(&application, WIDE, 20)).is_none(),
        "a reader with nothing set aside is shown no shelf to set it on"
    );

    settle_elsewhere(&mut application, set_aside, Some(SessionTimestamp(now())));

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    let divider = sidebar_divider(&rows);
    assert!(
        rendered_row(&rows, "Work still going") < divider,
        "the Session still in flight keeps its place above the divider: {rows:?}"
    );
    assert!(
        divider < rendered_row(&rows, "Work set aside"),
        "the settled Session moves onto the shelf below it: {rows:?}"
    );

    settle_elsewhere(&mut application, set_aside, None);

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(
        sidebar_divider_row(&rows).is_none(),
        "unsettling empties the shelf, and an empty shelf takes its divider with it: {rows:?}"
    );
    assert!(
        rendered_row(&rows, "Work set aside") < rendered_row(&rows, "Work still going"),
        "and the Session comes back to the place its creation order gives it: {rows:?}"
    );
}

#[test]
fn a_listing_refreshed_with_a_settled_marker_moves_the_session_onto_the_shelf() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let set_apart = SessionId::new();
    let steady = SessionId::new();
    let listing = |settled: bool| {
        let questioned = listed_as(set_apart, "Work in question", workspace.path(), 2);
        vec![
            if settled {
                set_aside(questioned, now())
            } else {
                questioned
            },
            listed_as(steady, "Steady work", workspace.path(), 1),
        ]
    };
    let mut application = sidebar_showing(workspace.path(), listing(false));
    assert!(
        sidebar_divider_row(&rendered_application_rows_at(&application, WIDE, 20)).is_none(),
        "the Session opens on the active list, where an unmarked Session belongs"
    );

    // Hiding and showing the Sidebar is what asks the server afresh, so this
    // is a whole listing landing rather than a change announced in place.
    press_toggle(&mut application);
    let request = expect_sidebar_listing(press_toggle(&mut application));
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request,
            sessions: listing(true),
        })
        .expect("adopt the refreshed listing");

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    let divider = sidebar_divider(&rows);
    assert!(
        divider < rendered_row(&rows, "Work in question"),
        "a listing carrying the marker puts the Session on the settled shelf: {rows:?}"
    );
    assert!(
        rendered_row(&rows, "Steady work") < divider,
        "and leaves the Session it does not mark on the active list: {rows:?}"
    );
}

#[test]
fn a_never_prompted_session_lists_as_active() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    // A Session made and then left alone: no Prompt, so nothing has moved it
    // since — which is why the idle it has been sitting in settles nothing.
    let application = sidebar_showing(
        workspace.path(),
        vec![
            never_prompted("Never prompted", workspace.path(), days_ago(30)),
            settled(
                "Set aside",
                None,
                workspace.path(),
                2,
                hours_ago(2),
                hours_ago(1),
            ),
        ],
    );

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(
        rendered_row(&rows, "Never prompted") < sidebar_divider(&rows),
        "the Session a reader just made is never hidden on the settled shelf: {rows:?}"
    );
}

/// Nobody settled this Session and nothing is stored saying they did: the
/// Sidebar reads its last activity against the threshold each time it lists.
#[test]
fn a_session_left_alone_past_the_threshold_settles_itself() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let application = sidebar_showing(
        workspace.path(),
        vec![
            listed("Still warm", None, workspace.path(), 2, hours_ago(1)),
            listed("Left alone", None, workspace.path(), 1, days_ago(5)),
        ],
    );

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    let divider = sidebar_divider(&rows);
    assert!(
        divider < rendered_row(&rows, "Left alone"),
        "a Session past the three-day default settles on its own: {rows:?}"
    );
    assert!(
        rendered_row(&rows, "Still warm") < divider,
        "and one the reader touched today stays active: {rows:?}"
    );
}

#[test]
fn the_idle_setting_says_how_long_being_left_alone_has_to_be() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let application = sidebar_settling(
        workspace.path(),
        AutoSettle::Idle(7),
        vec![
            listed("Left alone", None, workspace.path(), 2, days_ago(3)),
            settled(
                "Set aside",
                None,
                workspace.path(),
                1,
                hours_ago(2),
                hours_ago(1),
            ),
        ],
    );

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(
        rendered_row(&rows, "Left alone") < sidebar_divider(&rows),
        "three days is not long enough for a reader who asked for seven: {rows:?}"
    );
}

#[test]
fn turning_auto_settle_off_leaves_only_what_the_reader_settled_on_the_shelf() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let application = sidebar_settling(
        workspace.path(),
        AutoSettle::Off,
        vec![
            listed("Left alone", None, workspace.path(), 2, days_ago(30)),
            settled("Set aside", None, workspace.path(), 1, hours_ago(2), now()),
        ],
    );

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    let divider = sidebar_divider(&rows);
    assert!(
        rendered_row(&rows, "Left alone") < divider,
        "no idle is long enough once the reader has turned settling off: {rows:?}"
    );
    assert!(
        divider < rendered_row(&rows, "Set aside"),
        "and the marker the reader set stands whatever the Setting says: {rows:?}"
    );
}

#[test]
fn a_session_the_reader_settled_stays_settled_however_recently_it_moved() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let application = sidebar_showing(
        workspace.path(),
        vec![settled(
            "Set aside",
            None,
            workspace.path(),
            1,
            now(),
            now(),
        )],
    );

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(
        sidebar_divider(&rows) < rendered_row(&rows, "Set aside"),
        "the reader's own say-so is not something an idle threshold overrules: {rows:?}"
    );
}

/// A Session that settled itself has no settled marker to read a moment off,
/// so both its place on the shelf and the time its row shows come from its last
/// activity — the moment the idle it settled for began.
#[test]
fn a_session_that_settled_itself_stands_and_reads_by_its_last_activity() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let application = sidebar_showing(
        workspace.path(),
        vec![
            listed("Left alone", None, workspace.path(), 2, days_ago(5)),
            settled(
                "Set aside",
                None,
                workspace.path(),
                1,
                days_ago(9),
                hours_ago(1),
            ),
        ],
    );

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    let left_alone = rendered_row(&rows, "Left alone");
    assert!(
        rendered_row(&rows, "Set aside") < left_alone,
        "the marker's own stamp is more recent, so it stands nearer the divider: {rows:?}"
    );
    assert!(
        sidebar_column(&rows[left_alone]).ends_with("5d"),
        "and the row that settled itself reads the activity it settled for: {:?}",
        sidebar_column(&rows[left_alone])
    );
}

/// The reader edits the Setting while the Sidebar is open: the derivation is
/// read afresh on the next frame, so the shelves move without a new listing.
#[test]
fn moving_the_auto_settle_settings_reclassifies_the_sidebar_in_place() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_showing(
        workspace.path(),
        vec![listed("Left alone", None, workspace.path(), 1, days_ago(5))],
    );
    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(
        sidebar_divider(&rows) < rendered_row(&rows, "Left alone"),
        "the default threshold settles it: {rows:?}"
    );

    deliver_sidebar_settings(
        &mut application,
        SidebarSettings {
            launch_visibility: SidebarVisibility::Shown,
            auto_settle: AutoSettle::Off,
            ..SidebarSettings::default()
        },
    );

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(
        sidebar_divider_row(&rows).is_none(),
        "turning settling off empties the shelf and takes the divider with it: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("Left alone")),
        "and leaves the Session standing on the active list: {rows:?}"
    );
}

/// Unsettling is the reader saying this work is live again, and the server
/// answers by moving the Session's last activity — so the idle the Sidebar
/// derives cannot put back what the reader just took off the shelf.
#[test]
fn a_session_unsettled_after_a_long_idle_comes_back_to_the_active_list() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let set_apart = SessionId::new();
    let mut application = sidebar_showing(
        workspace.path(),
        vec![set_aside(
            listed_at(set_apart, "Long set aside", workspace.path(), days_ago(30)),
            days_ago(30),
        )],
    );
    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(sidebar_divider(&rows) < rendered_row(&rows, "Long set aside"));

    // The server clears the marker and stamps the Session's last activity with
    // the moment the reader reached for it, which the refreshed listing brings.
    settle_elsewhere(&mut application, set_apart, None);
    press_toggle(&mut application);
    let request = expect_sidebar_listing(press_toggle(&mut application));
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request,
            sessions: vec![listed_at(
                set_apart,
                "Long set aside",
                workspace.path(),
                now(),
            )],
        })
        .expect("adopt the refreshed listing");

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(
        sidebar_divider_row(&rows).is_none(),
        "nothing is left on the shelf the reader emptied: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("Long set aside")),
        "the work the reader picked back up is on the active list: {rows:?}"
    );
}

/// A settled Session is one a reader can prompt back to life, and a Session
/// Suru could not read is not one: the marker never lands on it, and neither
/// does the idle the Sidebar derives.
#[test]
fn a_session_suru_cannot_read_never_settles_however_long_it_has_sat() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let application = sidebar_showing(
        workspace.path(),
        vec![
            SessionListItem::Unreadable(UnreadableSessionSummary {
                id: SessionId::new(),
                title: "Unreadable work".to_owned(),
                created_at: SessionTimestamp(days_ago(90)),
                updated_at: SessionTimestamp(days_ago(60)),
                workspace: Some(Workspace {
                    path: workspace.path().to_owned(),
                }),
            }),
            listed("Left alone", None, workspace.path(), 1, days_ago(5)),
        ],
    );

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(
        rendered_row(&rows, "Unreadable work") < sidebar_divider(&rows),
        "the shelf is for work a reader set down, not for a record Suru cannot open: {rows:?}"
    );
}

#[test]
fn the_arrows_walk_across_the_divider_onto_the_settled_shelf() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_focused(
        workspace.path(),
        vec![
            listed("Still going", None, workspace.path(), 1, now()),
            settled(
                "Set aside",
                None,
                workspace.path(),
                2,
                hours_ago(2),
                hours_ago(1),
            ),
        ],
    );

    assert!(
        selected_sidebar_text(&application).contains("Still going"),
        "an opened Sidebar starts on the row nearest the reader"
    );

    press_sidebar_key(&mut application, KeyCode::Down);
    assert!(
        selected_sidebar_text(&application).contains("Set aside"),
        "the divider is a rule rather than a row, so the arrows step over it onto the shelf"
    );

    press_sidebar_key(&mut application, KeyCode::Down);
    assert!(
        selected_sidebar_text(&application).contains(ALL_WORKSPACES),
        "and past the end of the shelf they wrap back to the selector above the list"
    );

    press_sidebar_key(&mut application, KeyCode::Down);
    assert!(
        selected_sidebar_text(&application).contains("Still going"),
        "and on to the top of the active list"
    );
}

/// Recent history is what a reader looks back for; the whole of it would
/// drown the work they are choosing between, so the shelf is bounded and the
/// tail stands behind an affordance.
#[test]
fn the_settled_shelf_shows_ten_rows_and_offers_the_rest() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let application = sidebar_showing(workspace.path(), set_aside_shelf(workspace.path(), 12));

    let rows = rendered_application_rows_at(&application, WIDE, 20);

    let divider = sidebar_divider(&rows);
    assert_eq!(
        rendered_row(&rows, "Ended 00"),
        divider + 1,
        "the shelf opens on the work that ended most recently: {rows:?}"
    );
    assert_eq!(
        rendered_row(&rows, "Ended 09"),
        divider + 10,
        "and closes ten rows later: {rows:?}"
    );
    assert!(
        !drawn_in_sidebar(&rows, "Ended 10"),
        "the eleventh stands under the affordance rather than on the shelf: {rows:?}"
    );
    assert_eq!(
        sidebar_column(&rows[divider + 11]),
        "Show 2 more",
        "which offers what is left rather than a batch that is not there: {rows:?}"
    );
}

#[test]
fn the_affordance_shows_twenty_five_more_and_repeats_to_the_end_of_the_shelf() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_focused(workspace.path(), set_aside_shelf(workspace.path(), 40));

    // Past the top of the list is the selector standing above it, and past that
    // the affordance closing the shelf.
    press_sidebar_key(&mut application, KeyCode::Up);
    press_sidebar_key(&mut application, KeyCode::Up);
    assert_eq!(
        selected_sidebar_text(&application).trim(),
        "Show 25 more",
        "past the selector is the affordance closing the shelf"
    );

    assert_eq!(
        press_sidebar_key(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue,
        "asking for more of the shelf attaches nothing"
    );

    let rows = rendered_application_rows_at(&application, WIDE, TALL);
    assert!(
        drawn_in_sidebar(&rows, "Ended 34"),
        "twenty-five more rows stand on the shelf: {rows:?}"
    );
    assert!(
        !drawn_in_sidebar(&rows, "Ended 35"),
        "and the rest still stand under it: {rows:?}"
    );
    assert!(
        drawn_in_sidebar(&rows, "Show 5 more"),
        "the affordance goes on offering what is left: {rows:?}"
    );

    press_sidebar_key(&mut application, KeyCode::Enter);

    let rows = rendered_application_rows_at(&application, WIDE, TALL);
    assert!(
        drawn_in_sidebar(&rows, "Ended 39"),
        "the whole shelf is on show: {rows:?}"
    );
    assert!(
        !drawn_in_sidebar(&rows, "Show "),
        "with nothing left to ask for: {rows:?}"
    );
    assert!(
        selected_sidebar_text(&application).contains("Ended 39"),
        "the affordance the reader was on is gone, so they land on the last row it uncovered"
    );
}

/// The revealed tail belongs to the listing it was revealed on. A Sidebar
/// asking for its Sessions afresh opens the shelf on its first rows again
/// rather than inheriting however deep the reader had walked into some other
/// body of work.
#[test]
fn a_sidebar_asking_for_its_sessions_afresh_opens_the_shelf_on_its_first_rows() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_focused(workspace.path(), set_aside_shelf(workspace.path(), 12));
    // Up onto the selector standing above the list, and up again onto the
    // affordance at the shelf's foot.
    press_sidebar_key(&mut application, KeyCode::Up);
    press_sidebar_key(&mut application, KeyCode::Up);
    press_sidebar_key(&mut application, KeyCode::Enter);
    assert!(
        drawn_in_sidebar(
            &rendered_application_rows_at(&application, WIDE, 20),
            "Ended 11"
        ),
        "the reader revealed the whole of the shelf"
    );

    press_toggle(&mut application);
    let request = expect_sidebar_listing(press_toggle(&mut application));
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request,
            sessions: set_aside_shelf(workspace.path(), 12),
        })
        .expect("hydrate the Sidebar it asked for afresh");

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(
        !drawn_in_sidebar(&rows, "Ended 11"),
        "the tail they had revealed went with the listing it was revealed on: {rows:?}"
    );
    assert!(
        drawn_in_sidebar(&rows, "Show 2 more"),
        "and the shelf offers it again: {rows:?}"
    );
}

/// Tall enough to draw a whole settled shelf, which is what a test about
/// paging through one asks for.
const TALL: u16 = 48;

/// A shelf of Sessions the reader set aside, one minute apart, so the order
/// the shelf draws them in is the order they are numbered.
fn set_aside_shelf(workspace: &Path, count: u64) -> Vec<SessionListItem> {
    (0..count)
        .map(|ordinal| {
            settled(
                &format!("Ended {ordinal:02}"),
                None,
                workspace,
                ordinal + 1,
                minutes_ago(ordinal + 1),
                minutes_ago(ordinal + 1),
            )
        })
        .collect()
}

/// Whether the Sidebar's own columns carry `needle` anywhere down the frame.
fn drawn_in_sidebar(rows: &[String], needle: &str) -> bool {
    rows.iter().any(|row| sidebar_column(row).contains(needle))
}

/// A settlement another client made, arriving on the session-catalog stream.
fn settle_elsewhere(
    application: &mut Application,
    session_id: SessionId,
    settled_at: Option<SessionTimestamp>,
) {
    application
        .handle_event(ApplicationEvent::Managed(
            ManagedEvent::SessionSettlementChanged(SessionSettlementChanged {
                session_id,
                settled_at,
            }),
        ))
        .expect("take the settlement another client made");
}

/// The row the divider closing the active list is drawn on, where there is a
/// settled shelf for it to open.
fn sidebar_divider_row(rows: &[String]) -> Option<usize> {
    rows.iter()
        .position(|row| sidebar_column(row).starts_with("Settled ─"))
}

fn sidebar_divider(rows: &[String]) -> usize {
    sidebar_divider_row(rows)
        .unwrap_or_else(|| panic!("the divider opens the settled shelf: {rows:?}"))
}

/// A listed Session with an id the test can name and a last activity it
/// chooses, which is what a Session moving between shelves needs to keep its
/// identity across two listings.
fn listed_at(
    session_id: SessionId,
    title: &str,
    workspace: &Path,
    updated_at: u64,
) -> SessionListItem {
    let SessionListItem::Readable(mut summary) = listed(title, None, workspace, 1, updated_at)
    else {
        unreachable!("the fixture builds a readable Session");
    };
    summary.session.id = session_id;
    SessionListItem::Readable(summary)
}

/// A Session made and never prompted since, which is a Session whose last
/// activity is the moment it was made.
fn never_prompted(title: &str, workspace: &Path, made_at: u64) -> SessionListItem {
    listed(title, None, workspace, made_at, made_at)
}

/// A Session the reader has set aside as done for now.
fn settled(
    title: &str,
    emoji: Option<&str>,
    workspace: &Path,
    created_at: u64,
    updated_at: u64,
    settled_at: u64,
) -> SessionListItem {
    set_aside(
        listed(title, emoji, workspace, created_at, updated_at),
        settled_at,
    )
}

/// A listed Session as a listing that carries its settled marker reports it.
fn set_aside(session: SessionListItem, settled_at: u64) -> SessionListItem {
    let SessionListItem::Readable(mut summary) = session else {
        unreachable!("the fixture builds a readable Session");
    };
    summary.settled_at = Some(SessionTimestamp(settled_at));
    SessionListItem::Readable(summary)
}

/// The search box stands at the top of the column whether or not the reader is
/// searching, so the way to narrow a long list is always in view.
#[test]
fn the_search_box_stands_at_the_top_of_the_sidebar() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let application = sidebar_showing(
        workspace.path(),
        vec![listed("Listed work", None, workspace.path(), 1, now())],
    );

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert_eq!(
        sidebar_column(&rows[0]),
        "Search:",
        "the box opens the column, above everything it filters: {rows:?}"
    );
}

/// A reader searches by the words they remember, in whatever case they
/// remember them.
#[test]
fn typing_narrows_the_sidebar_by_title_whatever_case_either_is_in() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_focused(
        workspace.path(),
        vec![
            listed("Sidebar shell", None, workspace.path(), 2, now()),
            listed("Codex runtime", None, workspace.path(), 1, now()),
        ],
    );

    type_terminal_text(&mut application, "SHELL");

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(
        drawn_in_sidebar(&rows, "Search: SHELL"),
        "the box carries what the reader typed: {rows:?}"
    );
    assert!(
        drawn_in_sidebar(&rows, "Sidebar shell"),
        "a Title carrying the query stands however either is cased: {rows:?}"
    );
    assert!(
        !drawn_in_sidebar(&rows, "Codex runtime"),
        "and a Title that does not carry it steps aside: {rows:?}"
    );
}

/// The query is a substring of the Title rather than a pattern spelled through
/// it: the Sidebar is a list a reader reads, and a looser match would leave
/// rows standing they cannot see the reason for.
#[test]
fn the_query_has_to_run_whole_through_the_title() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_focused(
        workspace.path(),
        vec![listed("Sidebar shell", None, workspace.path(), 1, now())],
    );

    type_terminal_text(&mut application, "sdbr");

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(
        !drawn_in_sidebar(&rows, "Sidebar shell"),
        "letters scattered through the Title are not the query running through it: {rows:?}"
    );
}

/// Searching is a look across the whole body of work rather than down one
/// shelf, so a query puts both shelves away and answers with one list.
#[test]
fn a_query_replaces_both_shelves_with_one_flat_list_in_shelf_order() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_focused(
        workspace.path(),
        vec![
            listed("Live match", None, workspace.path(), 3, now()),
            listed("Other work", None, workspace.path(), 2, now()),
            settled(
                "Shelved match",
                None,
                workspace.path(),
                1,
                hours_ago(2),
                hours_ago(1),
            ),
        ],
    );

    type_terminal_text(&mut application, "match");

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(
        sidebar_divider_row(&rows).is_none(),
        "the results are one list rather than two shelves: {rows:?}"
    );
    assert!(
        !drawn_in_sidebar(&rows, "Other work"),
        "work the query passed over is not among the results: {rows:?}"
    );
    assert!(
        rendered_row(&rows, "Live match") < rendered_row(&rows, "Shelved match"),
        "the results keep the order the shelves would have drawn them in: {rows:?}"
    );
}

/// The shelf's cap is what keeps history from drowning a list nobody narrowed.
/// A query is the reader narrowing it themselves, so every result stands.
#[test]
fn a_query_shows_every_result_rather_than_a_capped_shelf() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_focused(workspace.path(), set_aside_shelf(workspace.path(), 12));

    type_terminal_text(&mut application, "ended");

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(
        drawn_in_sidebar(&rows, "Ended 11"),
        "a result the shelf would have held back stands with the rest: {rows:?}"
    );
    assert!(
        !drawn_in_sidebar(&rows, "Show "),
        "with nothing left behind an affordance to ask for: {rows:?}"
    );
}

#[test]
fn a_query_nothing_carries_says_so_rather_than_drawing_an_empty_column() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_focused(
        workspace.path(),
        vec![listed("Sidebar shell", None, workspace.path(), 1, now())],
    );

    type_terminal_text(&mut application, "nothing");

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(
        drawn_in_sidebar(&rows, "No Sessions match"),
        "an empty result says why the column is empty: {rows:?}"
    );
    assert!(
        !drawn_in_sidebar(&rows, "No Sessions yet"),
        "which is not the same as having no work at all: {rows:?}"
    );
}

#[test]
fn backspace_takes_the_query_back_a_letter_and_widens_the_results() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_focused(
        workspace.path(),
        vec![
            listed("Sidebar shell", None, workspace.path(), 2, now()),
            listed("Shelf paging", None, workspace.path(), 1, now()),
        ],
    );

    type_terminal_text(&mut application, "shell");
    assert!(
        !drawn_in_sidebar(
            &rendered_application_rows_at(&application, WIDE, 20),
            "Shelf paging"
        ),
        "the whole query narrows to one result"
    );

    press_sidebar_key(&mut application, KeyCode::Backspace);

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(
        drawn_in_sidebar(&rows, "Search: shel"),
        "the box gives up the last letter: {rows:?}"
    );
    assert!(
        drawn_in_sidebar(&rows, "Shelf paging") && drawn_in_sidebar(&rows, "Sidebar shell"),
        "and the results widen to what the shorter query carries: {rows:?}"
    );
}

/// Esc backs out one step at a time: the query first, and only then the keys.
#[test]
fn esc_clears_the_query_and_keeps_the_keys_in_the_sidebar() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_focused(
        workspace.path(),
        vec![
            listed("Sidebar shell", None, workspace.path(), 2, now()),
            listed("Codex runtime", None, workspace.path(), 1, now()),
        ],
    );
    type_terminal_text(&mut application, "shell");

    press_sidebar_key(&mut application, KeyCode::Esc);

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert_eq!(
        sidebar_column(&rows[0]),
        "Search:",
        "the box is empty again: {rows:?}"
    );
    assert!(
        drawn_in_sidebar(&rows, "Codex runtime"),
        "and the whole list is back with it: {rows:?}"
    );
    assert!(
        !selected_sidebar_text(&application).is_empty(),
        "the reader is still driving the Sidebar: they cleared a query, not the surface"
    );

    press_sidebar_key(&mut application, KeyCode::Esc);
    type_terminal_text(&mut application, "hello");

    assert!(
        rendered_application_rows_at(&application, WIDE, 20)
            .iter()
            .any(|row| row.contains("hello")),
        "and the Esc after that hands the keys back to the composer"
    );
}

#[test]
fn enter_attaches_the_result_the_reader_is_on() {
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

    type_terminal_text(&mut application, "wanted");

    assert!(
        selected_sidebar_text(&application).contains("The work wanted"),
        "a query that leaves the selection nowhere puts the reader on the first result"
    );
    assert_eq!(
        press_sidebar_key(&mut application, KeyCode::Enter),
        ApplicationTransition::AttachSession(wanted),
        "Enter attaches the result the reader is on"
    );
}

/// The arrows walk the results and nothing else: a row the query put away is
/// not one the reader can land on.
#[test]
fn the_arrows_walk_the_results_and_wrap_within_them() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_focused(
        workspace.path(),
        vec![
            listed("Match first", None, workspace.path(), 3, now()),
            listed("Passed over", None, workspace.path(), 2, now()),
            listed("Match second", None, workspace.path(), 1, now()),
        ],
    );

    type_terminal_text(&mut application, "match");

    press_sidebar_key(&mut application, KeyCode::Down);
    assert!(
        selected_sidebar_text(&application).contains("Match second"),
        "the arrows step over the work the query put away"
    );

    press_sidebar_key(&mut application, KeyCode::Down);
    assert!(
        selected_sidebar_text(&application).contains(ALL_WORKSPACES),
        "past the last result is the selector, as it is past the last row of any list"
    );

    press_sidebar_key(&mut application, KeyCode::Down);
    assert!(
        selected_sidebar_text(&application).contains("Match first"),
        "and they wrap within the results rather than past them into the rest"
    );
}

/// A query narrows a look at one listing. The Sidebar coming back into view
/// asks for its Sessions afresh, and that look is over.
#[test]
fn a_sidebar_coming_back_into_view_opens_on_the_whole_list_again() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let sessions = vec![
        listed("Sidebar shell", None, workspace.path(), 2, now()),
        listed("Codex runtime", None, workspace.path(), 1, now()),
    ];
    let mut application = sidebar_focused(workspace.path(), sessions.clone());
    type_terminal_text(&mut application, "shell");

    press_toggle(&mut application);
    let request = expect_sidebar_listing(press_toggle(&mut application));
    application
        .handle_event(ApplicationEvent::SessionsListed { request, sessions })
        .expect("hydrate the Sidebar it asked for afresh");

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert_eq!(
        sidebar_column(&rows[0]),
        "Search:",
        "the query went with the look that made it: {rows:?}"
    );
    assert!(
        drawn_in_sidebar(&rows, "Codex runtime"),
        "so the whole body of work is listed again: {rows:?}"
    );
}

/// A Title is what a query is read against, so another client's retitle can
/// carry the row the reader is on out of the results under them.
#[test]
fn a_result_retitled_elsewhere_out_of_the_query_takes_the_reader_with_it() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let moved = SessionId::new();
    let mut application = sidebar_focused(
        workspace.path(),
        vec![
            listed_as(SessionId::new(), "Match kept", workspace.path(), 2),
            listed_as(moved, "Match moving", workspace.path(), 1),
        ],
    );
    type_terminal_text(&mut application, "match");
    press_sidebar_key(&mut application, KeyCode::Down);
    assert!(
        selected_sidebar_text(&application).contains("Match moving"),
        "the reader is on the row about to be retitled"
    );

    application
        .handle_event(ApplicationEvent::Managed(
            ManagedEvent::SessionTitleChanged(SessionTitleChanged {
                session_id: moved,
                title: "Renamed away".to_owned(),
                emoji: None,
            }),
        ))
        .expect("take the retitle another client made");

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(
        !drawn_in_sidebar(&rows, "Renamed away"),
        "a Title the query no longer carries leaves the results: {rows:?}"
    );
    assert!(
        selected_sidebar_text(&application).contains("Match kept"),
        "and the reader lands back on a row that is drawn, rather than on one that is not"
    );
}

/// The box is narrow, so it shows the end of a long query rather than its
/// beginning: a reader watches the letters they are typing.
#[test]
fn a_long_query_keeps_its_end_in_the_box() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_focused(
        workspace.path(),
        vec![listed("Listed work", None, workspace.path(), 1, now())],
    );

    type_terminal_text(&mut application, "a query longer than the box is wide");

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(
        sidebar_column(&rows[0]).ends_with("box is wide"),
        "the letters the reader just typed are the ones in view: {:?}",
        sidebar_column(&rows[0])
    );
}

/// A listing that failed has told the reader nothing about whether their query
/// matches anything, so the refusal is the only account the column gives.
#[test]
fn a_refused_listing_says_so_rather_than_blaming_the_query() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = connected_application(workspace.path());
    deliver_launch_visibility(&mut application, SidebarVisibility::Hidden);
    let request = expect_sidebar_listing(press_toggle(&mut application));
    type_terminal_text(&mut application, "shell");
    application
        .handle_event(ApplicationEvent::SessionListingFailed {
            request,
            error: "the server refused".to_owned(),
        })
        .expect("take the refusal");

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(
        drawn_in_sidebar(&rows, "the server refused"),
        "the refusal stands under the search box: {rows:?}"
    );
    assert!(
        !drawn_in_sidebar(&rows, "No Sessions match"),
        "and nothing under it blames the reader's query for a list that never arrived: {rows:?}"
    );
}

/// A column well inside the Sidebar's own, which is where a reader points at a
/// row.
const SIDEBAR_CELL: u16 = 4;

/// The frame every press test draws: tall enough for a menu opened on any row
/// its fixtures list to stand whole.
const PRESS_HEIGHT: u16 = 20;

/// A press of one mouse button on one cell. The press rather than the release,
/// so a row answers the click the reader has just made rather than trailing a
/// drag that ends elsewhere.
fn press_at(
    application: &mut Application,
    button: MouseButton,
    column: u16,
    row: u16,
) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(button),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }))
        .expect("handle a press")
}

/// Presses the line the Sidebar drew `label` on. The frame is drawn first,
/// because a press resolves against the geometry the frame in force drew.
fn press_line(
    application: &mut Application,
    button: MouseButton,
    label: &str,
) -> ApplicationTransition {
    let row = drawn_at(application, label);
    press_at(application, button, SIDEBAR_CELL, row)
}

/// The screen row the Sidebar draws `label` on, with the frame drawn to find
/// out.
fn drawn_at(application: &Application, label: &str) -> u16 {
    let rows = rendered_application_rows_at(application, WIDE, PRESS_HEIGHT);
    u16::try_from(rendered_row(&rows, label)).expect("the row fits a screen row")
}

/// Opens one row's context menu the way a reader does, and reports the cell it
/// was anchored at, which is the corner the box is drawn from.
fn open_menu_on(application: &mut Application, title: &str) -> u16 {
    let anchor = drawn_at(application, title);
    assert!(
        anchor + MENU_LINES <= PRESS_HEIGHT,
        "the fixture anchors the menu where the frame has room for the whole box"
    );
    assert_eq!(
        press_at(application, MouseButton::Right, SIDEBAR_CELL, anchor),
        ApplicationTransition::Continue,
        "asking for a menu asks nothing of the server"
    );
    anchor
}

/// The lines the menu's box takes: an item apiece, and its two borders.
const MENU_LINES: u16 = 4;

/// What the menu anchored at `anchor` says, item by item, read off the frame.
fn menu_items(application: &Application, anchor: u16) -> Vec<String> {
    let rows = rendered_application_rows_at(application, WIDE, PRESS_HEIGHT);
    (1..MENU_LINES - 1)
        .map(|offset| rows[usize::from(anchor + offset)].clone())
        .collect()
}

/// Presses one item of the menu anchored at `anchor`. The box is drawn from
/// that cell, so its items stand one row down and one column in.
fn press_menu_item(
    application: &mut Application,
    anchor: u16,
    index: u16,
) -> ApplicationTransition {
    let _ = rendered_application_rows_at(application, WIDE, PRESS_HEIGHT);
    press_at(
        application,
        MouseButton::Left,
        SIDEBAR_CELL + 1,
        anchor + 1 + index,
    )
}

/// Two Sessions, the second of which every press test points at.
fn two_listed(workspace: &Path, wanted: SessionId) -> Vec<SessionListItem> {
    vec![
        listed_as(SessionId::new(), "Other work", workspace, 2),
        listed_as(wanted, "Wanted work", workspace, 1),
    ]
}

/// A press is how a reader opens a Session without reaching for the keyboard,
/// and it opens the row it landed on rather than the row they were on.
#[test]
fn a_left_press_on_a_row_attaches_the_session_it_stands_on() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let wanted = SessionId::new();
    let mut application = sidebar_showing(workspace.path(), two_listed(workspace.path(), wanted));

    assert_eq!(
        press_line(&mut application, MouseButton::Left, "Wanted work"),
        ApplicationTransition::AttachSession(wanted),
        "the press opens the Session under it, keys or no keys"
    );
}

/// The affordance is a row like any other to the pointer, and acting on it
/// brings up more of the shelf rather than opening anything.
#[test]
fn a_left_press_on_the_shelf_affordance_brings_up_more_of_it() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_showing(workspace.path(), set_aside_shelf(workspace.path(), 12));

    assert_eq!(
        press_line(&mut application, MouseButton::Left, "Show 2 more"),
        ApplicationTransition::Continue,
        "asking for more of the shelf attaches nothing"
    );
    assert!(
        drawn_in_sidebar(
            &rendered_application_rows_at(&application, WIDE, 20),
            "Ended 11"
        ),
        "and the rest of the shelf stands up"
    );
}

/// Only the rows answer a press. The search box, the rule between the shelves,
/// and the main view beside the column all stand for no Session.
#[test]
fn a_left_press_off_the_rows_attaches_nothing() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let wanted = SessionId::new();
    let mut application = sidebar_showing(
        workspace.path(),
        vec![
            listed_as(wanted, "Active work", workspace.path(), 2),
            settled(
                "Wrapped up",
                None,
                workspace.path(),
                1,
                hours_ago(3),
                minutes_ago(5),
            ),
        ],
    );

    assert_eq!(
        press_at(&mut application, MouseButton::Left, SIDEBAR_CELL, 0),
        ApplicationTransition::Continue,
        "the search box stands for no Session"
    );
    assert_eq!(
        press_line(&mut application, MouseButton::Left, "Settled ─"),
        ApplicationTransition::Continue,
        "nor does the rule closing the active list"
    );
    let row = drawn_at(&application, "Active work");
    assert_eq!(
        press_at(&mut application, MouseButton::Left, 60, row),
        ApplicationTransition::Continue,
        "and a press out in the main view is none of the Sidebar's business"
    );
}

/// A press resolves against the frame in force. A Sidebar the terminal has
/// since squeezed out has drawn nothing, so it answers nothing.
#[test]
fn a_press_resolves_against_the_frame_in_force_rather_than_the_one_before_it() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let wanted = SessionId::new();
    let mut application = sidebar_showing(workspace.path(), two_listed(workspace.path(), wanted));
    let row = drawn_at(&application, "Wanted work");

    let squeezed = rendered_application_rows_at(&application, 85, 20);
    assert!(
        !squeezed.iter().any(|drawn| drawn.contains("Wanted work")),
        "the frame in force is one too narrow for the Sidebar: {squeezed:?}"
    );
    assert_eq!(
        press_at(&mut application, MouseButton::Left, SIDEBAR_CELL, row),
        ApplicationTransition::Continue,
        "a press cannot land on a row this frame never drew"
    );
}

/// The menu offers what the row's own shelf asks for: active work is set
/// aside, and either way the Session can be taken away.
#[test]
fn a_right_press_on_an_active_row_offers_settle_and_delete() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let wanted = SessionId::new();
    let mut application = sidebar_showing(workspace.path(), two_listed(workspace.path(), wanted));

    let anchor = open_menu_on(&mut application, "Wanted work");

    let items = menu_items(&application, anchor);
    assert!(
        items[0].contains("Settle") && !items[0].contains("Unsettle"),
        "an active row is offered the shelf: {items:?}"
    );
    assert!(items[1].contains("Delete"), "and the way off it: {items:?}");
}

#[test]
fn a_right_press_on_a_settled_row_offers_unsettle_and_delete() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_showing(
        workspace.path(),
        vec![
            listed("Active work", None, workspace.path(), 2, now()),
            settled(
                "Wrapped up",
                None,
                workspace.path(),
                1,
                hours_ago(3),
                minutes_ago(5),
            ),
        ],
    );

    let anchor = open_menu_on(&mut application, "Wrapped up");

    let items = menu_items(&application, anchor);
    assert!(
        items[0].contains("Unsettle"),
        "a row already on the shelf is offered the way back: {items:?}"
    );
    assert!(items[1].contains("Delete"), "and the way off it: {items:?}");
}

#[test]
fn the_menu_sets_the_row_it_stands_on_aside() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let wanted = SessionId::new();
    let mut application = sidebar_showing(workspace.path(), two_listed(workspace.path(), wanted));

    let anchor = open_menu_on(&mut application, "Wanted work");

    assert_eq!(
        press_menu_item(&mut application, anchor, 0),
        ApplicationTransition::SettleSession {
            session_id: wanted,
            settled: true,
        },
        "the menu acts on the Session it was opened on rather than the one that is open"
    );
    assert!(
        !menu_is_drawn(&application),
        "and is done once it has acted"
    );
}

#[test]
fn the_menu_takes_a_settled_row_back_off_the_shelf() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let brought_back = SessionId::new();
    let mut application = sidebar_showing(
        workspace.path(),
        vec![
            listed("Active work", None, workspace.path(), 2, now()),
            set_aside(
                listed_as(brought_back, "Wrapped up", workspace.path(), 1),
                minutes_ago(5),
            ),
        ],
    );

    let anchor = open_menu_on(&mut application, "Wrapped up");

    assert_eq!(
        press_menu_item(&mut application, anchor, 0),
        ApplicationTransition::SettleSession {
            session_id: brought_back,
            settled: false,
        },
        "a settled row is brought back rather than set aside again"
    );
}

/// A Session and everything it owns is not something one stray press may take
/// away, so the item asks again before it acts.
#[test]
fn the_menu_asks_again_before_it_deletes() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let wanted = SessionId::new();
    let mut application = sidebar_showing(workspace.path(), two_listed(workspace.path(), wanted));

    let anchor = open_menu_on(&mut application, "Wanted work");

    assert_eq!(
        press_menu_item(&mut application, anchor, 1),
        ApplicationTransition::Continue,
        "the first press takes nothing away"
    );
    let items = menu_items(&application, anchor);
    assert!(
        items[1].contains("confirm"),
        "it asks the reader to say it again: {items:?}"
    );

    assert_eq!(
        press_menu_item(&mut application, anchor, 1),
        ApplicationTransition::DeleteSession(wanted),
        "and the second press acts on the Session the menu stands on"
    );
    assert!(
        !menu_is_drawn(&application),
        "leaving no menu standing over a row that is going away"
    );
}

/// A press outside an open menu puts it away and is spent there: dismissing a
/// menu is not also acting on whatever it was drawn over.
#[test]
fn a_press_outside_the_menu_puts_it_away_and_nothing_more() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let wanted = SessionId::new();
    let mut application = sidebar_showing(workspace.path(), two_listed(workspace.path(), wanted));

    let _ = open_menu_on(&mut application, "Wanted work");

    let row = drawn_at(&application, "Other work");
    assert_eq!(
        press_at(&mut application, MouseButton::Left, SIDEBAR_CELL, row),
        ApplicationTransition::Continue,
        "the press that dismisses a menu opens no Session"
    );
    assert!(!menu_is_drawn(&application), "the menu is put away");
}

/// The menu is the newest thing on screen while it is up, so it has the keys
/// whether or not the Sidebar itself does.
#[test]
fn the_menu_answers_the_arrows_and_backs_out_on_esc() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let wanted = SessionId::new();
    let mut application = sidebar_showing(workspace.path(), two_listed(workspace.path(), wanted));

    let anchor = open_menu_on(&mut application, "Wanted work");
    press_sidebar_key(&mut application, KeyCode::Down);

    assert_eq!(
        press_sidebar_key(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue,
        "the arrows walk the items, and Delete asks again rather than acting"
    );
    let items = menu_items(&application, anchor);
    assert!(
        items[1].contains("confirm"),
        "which is the item the arrows landed on: {items:?}"
    );

    press_sidebar_key(&mut application, KeyCode::Esc);
    assert!(
        !menu_is_drawn(&application),
        "and Esc puts the menu away, leaving the row it stood on alone"
    );
    assert!(
        drawn_in_sidebar(
            &rendered_application_rows_at(&application, WIDE, 20),
            "Wanted work"
        ),
        "the row itself is still listed"
    );
}

/// Whether a context menu is standing anywhere on the frame.
fn menu_is_drawn(application: &Application) -> bool {
    rendered_application_rows_at(application, WIDE, PRESS_HEIGHT)
        .iter()
        .any(|row| row.contains("Delete"))
}

/// The menu belongs to the column it was opened in. A terminal too narrow for
/// the Sidebar draws neither, and holds none of the keys — the reader keeps
/// the menu, as they keep the focus, and both come back when it widens.
#[test]
fn a_terminal_too_narrow_for_the_sidebar_draws_no_menu_and_holds_no_keys() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let wanted = SessionId::new();
    let mut application = sidebar_showing(workspace.path(), two_listed(workspace.path(), wanted));
    let _ = open_menu_on(&mut application, "Wanted work");

    let squeezed = rendered_application_rows_at(&application, 85, PRESS_HEIGHT);
    assert!(
        !squeezed.iter().any(|row| row.contains("Delete")),
        "no menu stands over a main view the Sidebar was squeezed off: {squeezed:?}"
    );

    type_terminal_text(&mut application, "not-a-menu-key");
    assert!(
        rendered_application_rows_at(&application, 85, PRESS_HEIGHT)
            .join("\n")
            .contains("not-a-menu-key"),
        "and the composer has the keys a menu nobody can see is not holding"
    );
}

/// The box stands over rows it was not opened on, so asking it for a menu
/// asks for nothing rather than carrying the reader onto whatever it covers.
#[test]
fn a_right_press_inside_the_menu_leaves_it_where_it_is() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let wanted = SessionId::new();
    let mut application = sidebar_showing(workspace.path(), two_listed(workspace.path(), wanted));

    let anchor = open_menu_on(&mut application, "Wanted work");
    let _ = rendered_application_rows_at(&application, WIDE, PRESS_HEIGHT);
    press_at(
        &mut application,
        MouseButton::Right,
        SIDEBAR_CELL + 1,
        anchor + 1,
    );

    assert_eq!(
        press_menu_item(&mut application, anchor, 0),
        ApplicationTransition::SettleSession {
            session_id: wanted,
            settled: true,
        },
        "the menu stands where it was, on the row it was opened on"
    );
}

/// The words the selector's first entry stands for, which is the whole body of
/// work rather than any one Workspace.
const ALL_WORKSPACES: &str = "All Workspaces";

/// The Workspace selector stands between the search box and the list it
/// governs, saying what the Sidebar is narrowed to before it says anything
/// about the work itself.
#[test]
fn the_selector_stands_under_the_search_box_and_says_what_is_in_scope() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let application = sidebar_showing(
        workspace.path(),
        vec![listed("Listed work", None, workspace.path(), 1, now())],
    );

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert_eq!(
        sidebar_column(&rows[1]),
        format!("▸ {ALL_WORKSPACES}"),
        "the selector opens on the reader's whole body of work: {rows:?}"
    );
}

/// The entries are the ways into the reader's work: all of it, then each
/// Workspace the Sidebar has listed a Session in, and the Workspace this
/// client itself runs in — which stands whether or not there is work in it
/// yet, because it is where the next Session will be.
#[test]
fn the_selector_lists_all_workspaces_first_then_the_workspaces_it_has_work_in() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_focused(
        workspace.path(),
        vec![
            listed("Notes", None, &workspace.path().join("notes"), 2, now()),
            listed("Suru", None, &workspace.path().join("suru"), 1, now()),
        ],
    );

    open_selector(&mut application);

    assert_eq!(
        selector_entries(&application),
        vec![
            ALL_WORKSPACES.to_owned(),
            workspace_name(workspace.path()),
            "notes".to_owned(),
            "suru".to_owned(),
        ],
        "every Workspace the reader has work in, and the one they are standing in"
    );
}

/// Choosing an entry narrows the whole Sidebar to that Workspace: the active
/// list, the settled shelf, and the results a query answers with.
#[test]
fn choosing_a_workspace_narrows_both_shelves() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_focused(workspace.path(), two_workspaces(workspace.path()));

    choose_workspace(&mut application, "notes");

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(
        drawn_in_sidebar(&rows, "Notes work") && drawn_in_sidebar(&rows, "Notes history"),
        "both shelves go on listing the Workspace the reader narrowed to: {rows:?}"
    );
    assert!(
        !drawn_in_sidebar(&rows, "Suru work") && !drawn_in_sidebar(&rows, "Suru history"),
        "and neither lists the Workspace they narrowed away from: {rows:?}"
    );
    assert_eq!(
        sidebar_column(&rows[1]),
        "▸ notes",
        "the selector says which Workspace the list is answering for: {rows:?}"
    );
}

#[test]
fn choosing_all_workspaces_widens_the_sidebar_again() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_focused(workspace.path(), two_workspaces(workspace.path()));
    choose_workspace(&mut application, "notes");

    choose_workspace(&mut application, ALL_WORKSPACES);

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    for title in ["Notes work", "Suru work", "Notes history", "Suru history"] {
        assert!(
            drawn_in_sidebar(&rows, title),
            "the whole body of work is listed again: {title} is missing from {rows:?}"
        );
    }
}

/// A query is read against the Sessions in scope, so narrowing to a Workspace
/// narrows what searching can turn up.
#[test]
fn a_query_answers_within_the_workspace_the_selector_is_narrowed_to() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_focused(workspace.path(), two_workspaces(workspace.path()));
    choose_workspace(&mut application, "notes");

    type_terminal_text(&mut application, "work");

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(
        drawn_in_sidebar(&rows, "Notes work"),
        "the query answers with the work in scope: {rows:?}"
    );
    assert!(
        !drawn_in_sidebar(&rows, "Suru work"),
        "and not with work the reader has narrowed away from: {rows:?}"
    );
}

/// The Setting seeds the scope a TUI launches with, and nothing more: the
/// selector is the reader's to move afterwards.
#[test]
fn the_initial_scope_setting_narrows_the_sidebar_a_tui_launches_with() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let application = sidebar_scoped(
        workspace.path(),
        SidebarScope::CurrentWorkspace,
        two_workspaces(workspace.path()),
    );

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert_eq!(
        sidebar_column(&rows[1]),
        format!("▸ {}", workspace_name(workspace.path())),
        "the Sidebar launches narrowed to the Workspace the client runs in: {rows:?}"
    );
    assert!(
        !drawn_in_sidebar(&rows, "Notes work") && !drawn_in_sidebar(&rows, "Suru work"),
        "and lists nothing rooted elsewhere: {rows:?}"
    );
}

/// The reader's own choice is view state: it is not written back, and a later
/// snapshot carrying some other Setting's edit does not undo it.
#[test]
fn a_scope_the_reader_chose_is_ephemeral_and_survives_a_later_snapshot() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_scoped(
        workspace.path(),
        SidebarScope::CurrentWorkspace,
        two_workspaces(workspace.path()),
    );

    assert_eq!(
        choose_workspace(&mut application, "notes"),
        ApplicationTransition::Continue,
        "moving the selector edits no Config Document"
    );
    deliver_sidebar_settings(
        &mut application,
        SidebarSettings {
            launch_visibility: SidebarVisibility::Shown,
            initial_scope: SidebarScope::CurrentWorkspace,
            ..SidebarSettings::default()
        },
    );

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert_eq!(
        sidebar_column(&rows[1]),
        "▸ notes",
        "a later snapshot leaves the scope the reader chose alone: {rows:?}"
    );
}

/// Esc backs the reader out one step at a time, and the entries they opened
/// are the innermost of them.
#[test]
fn esc_closes_the_selector_before_it_gives_up_the_query() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_focused(workspace.path(), two_workspaces(workspace.path()));
    type_terminal_text(&mut application, "work");
    open_selector(&mut application);

    press_sidebar_key(&mut application, KeyCode::Esc);

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert_eq!(
        sidebar_column(&rows[1]),
        format!("▸ {ALL_WORKSPACES}"),
        "the entries are put away, leaving the scope where it was: {rows:?}"
    );
    assert!(
        drawn_in_sidebar(&rows, "Search: work"),
        "and the query the reader was reading by is still theirs: {rows:?}"
    );
}

/// The selector answers a pointer as readily as the keys: one press opens the
/// entries, and a press on one of them chooses it.
#[test]
fn a_press_on_the_selector_opens_it_and_a_press_on_an_entry_chooses_it() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_showing(workspace.path(), two_workspaces(workspace.path()));

    assert_eq!(
        press_line(&mut application, MouseButton::Left, "▸ "),
        ApplicationTransition::Continue,
        "opening the entries asks nothing of the server"
    );
    assert_eq!(
        selector_entries(&application).first(),
        Some(&ALL_WORKSPACES.to_owned()),
        "the entries stand open under the selector"
    );

    press_line(&mut application, MouseButton::Left, "  notes");

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert_eq!(
        sidebar_column(&rows[1]),
        "▸ notes",
        "the entry the reader pressed is the scope in force: {rows:?}"
    );

    press_line(&mut application, MouseButton::Left, "▸ ");
    press_line(&mut application, MouseButton::Left, "▾ ");

    assert!(
        selector_entries(&application).is_empty(),
        "pointing at the same affordance twice puts the entries away again"
    );
}

/// The selector is a row like any other in that the arrows reach it: it stands
/// above the list, so moving off the top of the list lands on it.
#[test]
fn the_arrows_reach_the_selector_above_the_list() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut application = sidebar_focused(
        workspace.path(),
        vec![listed("Listed work", None, workspace.path(), 1, now())],
    );

    press_sidebar_key(&mut application, KeyCode::Up);

    assert!(
        selected_sidebar_text(&application).contains(ALL_WORKSPACES),
        "the row above the list is the selector"
    );
    press_sidebar_key(&mut application, KeyCode::Enter);
    assert_eq!(
        selector_entries(&application).first(),
        Some(&ALL_WORKSPACES.to_owned()),
        "which Enter opens rather than attaching anything"
    );
}

/// The server keeps a Session it could not place in every listing it narrows,
/// and the selector must give the same reading: narrowing is a claim about
/// where work is, and a Session nobody can place is not work it can hide.
#[test]
fn a_session_suru_cannot_place_stands_however_narrow_the_scope() {
    let workspace = tempfile::tempdir().expect("create Workspace");
    let mut sessions = two_workspaces(workspace.path());
    sessions.push(SessionListItem::Unreadable(UnreadableSessionSummary {
        id: SessionId::new(),
        title: "Unplaceable work".to_owned(),
        created_at: SessionTimestamp(minutes_ago(9)),
        updated_at: SessionTimestamp(minutes_ago(9)),
        workspace: None,
    }));
    let mut application = sidebar_focused(workspace.path(), sessions);

    choose_workspace(&mut application, "notes");

    let rows = rendered_application_rows_at(&application, WIDE, 20);
    assert!(
        drawn_in_sidebar(&rows, "Unplaceable work"),
        "a Session with no Workspace to read stands wherever the listing does: {rows:?}"
    );
    assert!(
        !drawn_in_sidebar(&rows, "Suru work"),
        "and the narrowing is otherwise as narrow as ever: {rows:?}"
    );
}

/// A connected client whose Sidebar launches under `initial_scope`.
fn sidebar_scoped(
    workspace: &Path,
    initial_scope: SidebarScope,
    sessions: Vec<SessionListItem>,
) -> Application {
    let mut application = connected_application(workspace);
    let request = expect_sidebar_listing(deliver_sidebar_settings(
        &mut application,
        SidebarSettings {
            launch_visibility: SidebarVisibility::Shown,
            initial_scope,
            ..SidebarSettings::default()
        },
    ));
    application
        .handle_event(ApplicationEvent::SessionsListed { request, sessions })
        .expect("hydrate the Sidebar");
    application
}

/// Work on both shelves in each of two Workspaces beside the one the client
/// runs in, which is what a narrowing is read against.
fn two_workspaces(root: &Path) -> Vec<SessionListItem> {
    vec![
        listed("Notes work", None, &root.join("notes"), 4, now()),
        listed("Suru work", None, &root.join("suru"), 3, now()),
        settled(
            "Notes history",
            None,
            &root.join("notes"),
            2,
            hours_ago(3),
            minutes_ago(5),
        ),
        settled(
            "Suru history",
            None,
            &root.join("suru"),
            1,
            hours_ago(4),
            minutes_ago(6),
        ),
    ]
}

/// Opens the selector's entries the way a reader driving the Sidebar from the
/// keyboard does: up onto the row above the list, then Enter.
fn open_selector(application: &mut Application) {
    press_sidebar_key(application, KeyCode::Up);
    press_sidebar_key(application, KeyCode::Enter);
}

/// Chooses one of the selector's entries by pressing it, opening the entries
/// first where they are not already open.
fn choose_workspace(application: &mut Application, label: &str) -> ApplicationTransition {
    if selector_entries(application).is_empty() {
        press_line(application, MouseButton::Left, "▸ ");
    }
    press_line(application, MouseButton::Left, &format!("  {label}"))
}

/// The entries standing open under the selector, and nothing where it is
/// closed. They are the lines stepped in past the affordance that opened them.
fn selector_entries(application: &Application) -> Vec<String> {
    let rows = rendered_application_rows_at(application, WIDE, 20);
    if !sidebar_column(&rows[1]).starts_with('▾') {
        return Vec::new();
    }
    rows.iter()
        .skip(2)
        .map(|row| row.chars().take_while(|character| *character != '│'))
        .map(|column| column.collect::<String>())
        .take_while(|column| column.starts_with("  ") && !column.trim().is_empty())
        .map(|column| column.trim().to_owned())
        .collect()
}

/// The Workspace as the Sidebar names it: the last component of its path.
fn workspace_name(workspace: &Path) -> String {
    workspace
        .file_name()
        .expect("the fixture's Workspace has a name")
        .to_string_lossy()
        .into_owned()
}
