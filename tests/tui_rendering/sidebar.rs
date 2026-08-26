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
        AutoSettle, EffectiveSettings, ModelAvailability, PromptId, Session, SessionDeleted,
        SessionId, SessionListItem, SessionSettlementChanged, SessionStatus, SessionSummary,
        SessionTimestamp, SettingsSnapshot, SidebarSettings, SidebarVisibility,
        UnreadableSessionSummary, Workspace,
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
        selected_sidebar_text(&application).contains("Still going"),
        "and past the end of the shelf they wrap back to the top of the active list"
    );
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
