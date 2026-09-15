//! The Icon Picker: a grid overlay a reader opens from a Sidebar row's
//! context menu, a Sidebar selector entry's context menu, a Workspace Picker
//! row's context menu, or a press on the open Session's header Icon, narrows
//! by typing, and chooses from with the arrows and Enter or a pointer press.

use crate::support::{
    click_mouse, connected_application, deliver_settings, enter_session,
    rendered_application_buffer, rendered_application_rows_at, text_position, workspace_dir,
};
use crossterm::event::{
    Event as InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use std::path::Path;
use suru::{
    managed_client::SessionEvent,
    protocol::{
        AppearanceSettings, EffectiveSettings, Outlook, Session, SessionId, SessionListItem,
        SessionStandingInputs, SessionStatus, SessionSummary, SessionTimestamp, SidebarSettings,
        SidebarVisibility, Workspace, WorkspaceId,
    },
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, SemanticCommandId,
        SessionListRequest, SessionListScope, SessionListSurface,
    },
};

fn shown_with_icons(show_icons: bool) -> EffectiveSettings {
    EffectiveSettings {
        sidebar: SidebarSettings {
            initial_visibility: SidebarVisibility::Shown,
            ..SidebarSettings::default()
        },
        appearance: AppearanceSettings {
            show_icons,
            ..AppearanceSettings::default()
        },
        ..EffectiveSettings::default()
    }
}

fn listed_as(session_id: SessionId, title: &str, workspace: &Path) -> SessionListItem {
    SessionListItem::Readable(Box::new(SessionSummary {
        checkout_state: None,
        session: Session {
            checkout: None,
            context_fill: None,
            id: session_id,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.to_owned(),
            },
            workspace: Workspace::directory(workspace.to_owned()),
            agent_selection: None,
            agent_selection_availability: suru::protocol::ModelAvailability::Available,
            approval_posture: None,
            status: SessionStatus::Idle,
            working_since: None,
            parent: None,
        },
        title: title.to_owned(),
        icon: None,
        settled_at: None,
        standing_inputs: SessionStandingInputs::default(),
        total_usage: None,
        created_at: SessionTimestamp(1),
        updated_at: SessionTimestamp(1),
    }))
}

/// A Sidebar showing one Session, with `show_icons` as given.
fn sidebar_with_one_session(
    workspace: &Path,
    session_id: SessionId,
    title: &str,
    show_icons: bool,
) -> Application {
    let mut application = connected_application(workspace);
    let ApplicationTransition::ListSessions(request) =
        deliver_settings(&mut application, shown_with_icons(show_icons))
    else {
        panic!("a Sidebar coming into view asks for its Sessions");
    };
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request,
            sessions: vec![listed_as(session_id, title, workspace)],
        })
        .expect("hydrate the Sidebar");
    application
}

/// A wide, tall frame: enough room for the Sidebar, a whole context menu, and
/// the Icon Picker's own grid.
const WIDE: u16 = 100;
const TALL: u16 = 24;

fn press(application: &mut Application, button: MouseButton, column: u16, row: u16) {
    click_mouse(
        application,
        MouseEvent {
            kind: MouseEventKind::Down(button),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        },
    )
    .expect("handle a press");
}

/// The screen row `needle` is drawn on, with the frame drawn to find out.
fn drawn_at(application: &Application, needle: &str) -> u16 {
    let rows = rendered_application_rows_at(application, WIDE, TALL);
    u16::try_from(
        rows.iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("rendered frame did not contain {needle:?}: {rows:#?}")),
    )
    .expect("the row fits a screen row")
}

/// A column well inside the Sidebar's own, which is where a reader points at
/// a row.
const SIDEBAR_CELL: u16 = 4;

fn press_key(application: &mut Application, code: KeyCode) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
        .expect("press the key")
}

fn type_text(application: &mut Application, text: &str) {
    for character in text.chars() {
        press_key(application, KeyCode::Char(character));
    }
}

fn picker_is_drawn(application: &Application) -> bool {
    rendered_application_rows_at(application, WIDE, TALL)
        .iter()
        .any(|row| row.contains("Choose Icon"))
}

/// Opens the Icon Picker on the one listed Session, through its Sidebar row's
/// context menu — the way a reader without a mouse aimed at the header does
/// it. Answers with the Application, still with the menu's frame drawn, so a
/// caller can act on the picker at once.
fn open_picker_from_sidebar_menu(workspace: &Path, session_id: SessionId) -> Application {
    let mut application = sidebar_with_one_session(workspace, session_id, "Wanted work", true);
    let anchor = drawn_at(&application, "Wanted work");
    press(&mut application, MouseButton::Right, SIDEBAR_CELL, anchor);
    let items = menu_item_labels(&application, anchor);
    let index = items
        .iter()
        .position(|label| label.contains("Choose icon"))
        .expect("the menu offers Choose icon while Icons are shown");
    press(
        &mut application,
        MouseButton::Left,
        SIDEBAR_CELL + 1,
        anchor + 1 + u16::try_from(index).expect("small menu index"),
    );
    assert!(
        picker_is_drawn(&application),
        "choosing the menu item opens the Icon Picker"
    );
    application
}

/// What the context menu anchored at `anchor` says, item by item — as many
/// lines as fit between its borders on a frame this tall.
fn menu_item_labels(application: &Application, anchor: u16) -> Vec<String> {
    let rows = rendered_application_rows_at(application, WIDE, TALL);
    let mut labels = Vec::new();
    for offset in 1.. {
        let Some(row) = rows.get(usize::from(anchor + offset)) else {
            break;
        };
        let trimmed = row.trim();
        if trimmed.starts_with('\u{2514}') || trimmed.is_empty() {
            break;
        }
        labels.push(row.clone());
        if offset > 6 {
            break;
        }
    }
    labels
}

#[test]
fn the_sidebar_menu_offers_choose_icon_only_while_icons_are_shown() {
    let workspace = workspace_dir();
    let session_id = SessionId::new();

    let with_icons = sidebar_with_one_session(workspace.path(), session_id, "Wanted work", true);
    let anchor = drawn_at(&with_icons, "Wanted work");
    let mut with_icons = with_icons;
    press(&mut with_icons, MouseButton::Right, SIDEBAR_CELL, anchor);
    let items = menu_item_labels(&with_icons, anchor);
    assert!(
        items.iter().any(|label| label.contains("Choose icon")),
        "the menu offers Choose icon while Icons are shown: {items:?}"
    );

    let without_icons =
        sidebar_with_one_session(workspace.path(), session_id, "Wanted work", false);
    let anchor = drawn_at(&without_icons, "Wanted work");
    let mut without_icons = without_icons;
    press(&mut without_icons, MouseButton::Right, SIDEBAR_CELL, anchor);
    let items = menu_item_labels(&without_icons, anchor);
    assert!(
        !items.iter().any(|label| label.contains("Choose icon")),
        "the menu offers no Choose icon item while Icons are hidden: {items:?}"
    );
}

#[test]
fn choosing_the_sidebar_menu_item_opens_the_icon_picker() {
    let workspace = workspace_dir();
    let session_id = SessionId::new();
    let application = open_picker_from_sidebar_menu(workspace.path(), session_id);
    assert!(picker_is_drawn(&application));
}

/// A connected client with one active Session open, its Icon set and Icons
/// shown, which is the fixture every header-press test starts from.
fn application_with_open_session_icon(workspace: &Path, icon: &str) -> Application {
    let mut application = connected_application(workspace);
    deliver_settings(&mut application, shown_with_icons(true));
    let (_, mut snapshot) = enter_session(&mut application, workspace);
    snapshot.title = "Alpha work".into();
    snapshot.icon = Some(icon.to_owned());
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(snapshot)))
        .expect("show the titled, iconed Session");
    application
}

#[test]
fn pressing_the_header_icon_opens_the_icon_picker() {
    let workspace = workspace_dir();
    let mut application = application_with_open_session_icon(workspace.path(), "md-bug");
    let buffer = rendered_application_buffer(&application, WIDE, TALL);
    let (column, row) = text_position(&buffer, "\u{f00e4}");
    press(&mut application, MouseButton::Left, column, row);
    assert!(
        picker_is_drawn(&application),
        "a press on the header Icon opens the Icon Picker"
    );
}

#[test]
fn pressing_the_header_icon_does_nothing_while_icons_are_hidden() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    deliver_settings(&mut application, EffectiveSettings::default());
    let (_, mut snapshot) = enter_session(&mut application, workspace.path());
    snapshot.title = "Alpha work".into();
    snapshot.icon = Some("md-bug".to_owned());
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(snapshot)))
        .expect("show the titled Session with Icons hidden");
    let rows_before = rendered_application_rows_at(&application, WIDE, TALL);
    assert!(
        !rows_before.iter().any(|row| row.contains('\u{f00e4}')),
        "nothing is drawn for the Icon while Icons are hidden"
    );
    // The press lands where the Title begins — the leading Icon column when
    // Icons are shown — and reaches nothing: no Icon span was ever recorded.
    let (column, row) = text_position(
        &rendered_application_buffer(&application, WIDE, TALL),
        "Alpha work",
    );
    let transition = click_mouse(
        &mut application,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        },
    )
    .expect("press near the Title");
    assert_eq!(transition, ApplicationTransition::Continue);
    assert!(!picker_is_drawn(&application));
}

#[test]
fn the_grid_renders_glyphs_and_the_footer_names_the_focused_one() {
    let workspace = workspace_dir();
    let mut application = application_with_open_session_icon(workspace.path(), "md-bug");
    let buffer = rendered_application_buffer(&application, WIDE, TALL);
    let (column, row) = text_position(&buffer, "\u{f00e4}");
    press(&mut application, MouseButton::Left, column, row);

    let rows = rendered_application_rows_at(&application, WIDE, TALL);
    let text = rows.join("\n");
    assert!(
        text.contains("md-bug"),
        "the footer names the focused glyph's Catalog name: {text}"
    );
    // A second glyph the Catalog carries also stands somewhere in the grid.
    assert!(
        text.contains('\u{f0674}'),
        "the grid draws more than the focused glyph alone: {text}"
    );
}

#[test]
fn searching_narrows_the_grid_and_refocuses_the_first_result() {
    let workspace = workspace_dir();
    let mut application = application_with_open_session_icon(workspace.path(), "md-bug");
    let buffer = rendered_application_buffer(&application, WIDE, TALL);
    let (column, row) = text_position(&buffer, "\u{f00e4}");
    press(&mut application, MouseButton::Left, column, row);

    type_text(&mut application, "rust");
    let text = rendered_application_rows_at(&application, WIDE, TALL).join("\n");
    assert!(
        text.contains("dev-rust"),
        "the footer names the query's first result: {text}"
    );
}

#[test]
fn keyboard_choice_emits_the_set_icon_transition_and_closes() {
    let workspace = workspace_dir();
    let mut application = application_with_open_session_icon(workspace.path(), "md-bug");
    let buffer = rendered_application_buffer(&application, WIDE, TALL);
    let (column, row) = text_position(&buffer, "\u{f00e4}");
    press(&mut application, MouseButton::Left, column, row);

    type_text(&mut application, "rust");
    assert!(
        rendered_application_rows_at(&application, WIDE, TALL)
            .join("\n")
            .contains("dev-rust")
    );
    let transition = press_key(&mut application, KeyCode::Enter);
    let ApplicationTransition::SetSessionIcon { icon, .. } = transition else {
        panic!("Enter chooses the focused glyph, got {transition:?}");
    };
    assert_eq!(icon, "dev-rust");
    assert!(
        !picker_is_drawn(&application),
        "choosing closes the Icon Picker"
    );
}

#[test]
fn pointer_choice_on_a_cell_does_the_same() {
    let workspace = workspace_dir();
    let mut application = application_with_open_session_icon(workspace.path(), "md-bug");
    let buffer = rendered_application_buffer(&application, WIDE, TALL);
    let (column, row) = text_position(&buffer, "\u{f00e4}");
    press(&mut application, MouseButton::Left, column, row);

    type_text(&mut application, "rust");
    let buffer = rendered_application_buffer(&application, WIDE, TALL);
    // `dev-rust`'s own glyph, the query's only remaining cell.
    let (column, row) = text_position(&buffer, "\u{e7a8}");
    let transition = click_mouse(
        &mut application,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        },
    )
    .expect("press the cell");
    let ApplicationTransition::SetSessionIcon { icon, .. } = transition else {
        panic!("a press on a cell chooses it, got {transition:?}");
    };
    assert_eq!(icon, "dev-rust");
    assert!(!picker_is_drawn(&application));
}

#[test]
fn escape_cancels_without_choosing() {
    let workspace = workspace_dir();
    let mut application = application_with_open_session_icon(workspace.path(), "md-bug");
    let buffer = rendered_application_buffer(&application, WIDE, TALL);
    let (column, row) = text_position(&buffer, "\u{f00e4}");
    press(&mut application, MouseButton::Left, column, row);

    type_text(&mut application, "rust");
    let transition = press_key(&mut application, KeyCode::Esc);
    assert_eq!(transition, ApplicationTransition::Continue);
    assert!(!picker_is_drawn(&application), "Escape closes the picker");

    // The Session's own Icon is untouched: no clear action stands here, and
    // choosing never ran.
    let text = rendered_application_rows_at(&application, WIDE, TALL).join("\n");
    assert!(
        text.contains('\u{f00e4}'),
        "the original Icon still draws: {text}"
    );
}

/// A pointer press outside every cell puts the picker away without choosing —
/// the same as Escape.
#[test]
fn a_press_outside_the_grid_closes_without_choosing() {
    let workspace = workspace_dir();
    let mut application = application_with_open_session_icon(workspace.path(), "md-bug");
    let buffer = rendered_application_buffer(&application, WIDE, TALL);
    let (column, row) = text_position(&buffer, "\u{f00e4}");
    press(&mut application, MouseButton::Left, column, row);
    assert!(picker_is_drawn(&application));

    // The far corner of a wide, tall frame stands well outside the centered
    // overlay's own box.
    press(&mut application, MouseButton::Left, WIDE - 1, TALL - 1);
    assert!(!picker_is_drawn(&application));
}

// A Workspace target: opened from a Sidebar selector entry's own context
// menu, or from a Workspace Picker row's own context menu, both carrying the
// Workspace by Origin and identity the way a Session's own row carries it.

/// Settings showing Icons without forcing the Sidebar open — the Workspace
/// Picker fixtures below draw their own overlay and have no use for it.
fn icons_shown(show_icons: bool) -> EffectiveSettings {
    EffectiveSettings {
        appearance: AppearanceSettings {
            show_icons,
            ..AppearanceSettings::default()
        },
        ..EffectiveSettings::default()
    }
}

/// A Sidebar showing two Sessions rooted in two different Workspaces, with
/// the selector opened on the entries beneath it — what a reader right-clicks
/// to act on a Workspace rather than a Session. Answers with the Application
/// and the "acorn" Workspace's own identity, computed the same way the
/// client does.
fn sidebar_with_selector_open(root: &Path, show_icons: bool) -> (Application, WorkspaceId) {
    let acorn = root.join("acorn");
    let birch = root.join("birch");
    let mut application = connected_application(root);
    let ApplicationTransition::ListSessions(request) =
        deliver_settings(&mut application, shown_with_icons(show_icons))
    else {
        panic!("a Sidebar coming into view asks for its Sessions");
    };
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request,
            sessions: vec![
                listed_as(SessionId::new(), "Acorn work", &acorn),
                listed_as(SessionId::new(), "Birch work", &birch),
            ],
        })
        .expect("hydrate the Sidebar");
    // The selector's own line stands closed, marked "▸", above the entries a
    // press on it opens.
    let selector_row = drawn_at(&application, "\u{25b8} ");
    press(
        &mut application,
        MouseButton::Left,
        SIDEBAR_CELL,
        selector_row,
    );
    (application, WorkspaceId::directory(&acorn))
}

/// Opens the Icon Picker on the "acorn" Workspace's selector entry, through
/// its own context menu. Answers with the Application, still with the
/// picker's frame drawn, and the Workspace's own identity.
fn open_picker_from_selector_menu(root: &Path) -> (Application, WorkspaceId) {
    let (mut application, workspace_id) = sidebar_with_selector_open(root, true);
    let anchor = drawn_at(&application, "acorn");
    press(&mut application, MouseButton::Right, SIDEBAR_CELL, anchor);
    let items = menu_item_labels(&application, anchor);
    let index = items
        .iter()
        .position(|label| label.contains("Choose icon"))
        .expect("the selector entry's menu offers Choose icon while Icons are shown");
    press(
        &mut application,
        MouseButton::Left,
        SIDEBAR_CELL + 1,
        anchor + 1 + u16::try_from(index).expect("small menu index"),
    );
    assert!(
        picker_is_drawn(&application),
        "choosing the menu item opens the Icon Picker"
    );
    (application, workspace_id)
}

#[test]
fn the_selector_entry_menu_offers_choose_icon_only_while_icons_are_shown() {
    let workspace = workspace_dir();

    let (mut with_icons, _) = sidebar_with_selector_open(workspace.path(), true);
    let anchor = drawn_at(&with_icons, "acorn");
    press(&mut with_icons, MouseButton::Right, SIDEBAR_CELL, anchor);
    let items = menu_item_labels(&with_icons, anchor);
    assert!(
        items.iter().any(|label| label.contains("Choose icon")),
        "the entry's menu offers Choose icon while Icons are shown: {items:?}"
    );

    let (mut without_icons, _) = sidebar_with_selector_open(workspace.path(), false);
    let anchor = drawn_at(&without_icons, "acorn");
    let before = rendered_application_rows_at(&without_icons, WIDE, TALL);
    press(&mut without_icons, MouseButton::Right, SIDEBAR_CELL, anchor);
    let after = rendered_application_rows_at(&without_icons, WIDE, TALL);
    assert_eq!(
        before, after,
        "a right press draws no menu at all while Icons are hidden"
    );
}

#[test]
fn choosing_the_selector_menu_item_opens_the_icon_picker_over_the_workspace() {
    let workspace = workspace_dir();
    let (application, _) = open_picker_from_selector_menu(workspace.path());
    assert!(picker_is_drawn(&application));
}

#[test]
fn keyboard_choice_over_a_selector_entry_emits_set_workspace_icon_and_closes() {
    let workspace = workspace_dir();
    let (mut application, workspace_id) = open_picker_from_selector_menu(workspace.path());

    type_text(&mut application, "rust");
    let transition = press_key(&mut application, KeyCode::Enter);
    let ApplicationTransition::SetWorkspaceIcon {
        origin,
        workspace_id: chosen,
        icon,
    } = transition
    else {
        panic!("Enter chooses the focused glyph, got {transition:?}");
    };
    assert_eq!(origin, Outlook::Local);
    assert_eq!(chosen, workspace_id);
    assert_eq!(icon, "dev-rust");
    assert!(
        !picker_is_drawn(&application),
        "choosing closes the Icon Picker"
    );
}

#[test]
fn pointer_choice_over_a_selector_entry_does_the_same() {
    let workspace = workspace_dir();
    let (mut application, workspace_id) = open_picker_from_selector_menu(workspace.path());

    type_text(&mut application, "rust");
    let buffer = rendered_application_buffer(&application, WIDE, TALL);
    // `dev-rust`'s own glyph, the query's only remaining cell.
    let (column, row) = text_position(&buffer, "\u{e7a8}");
    let transition = click_mouse(
        &mut application,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        },
    )
    .expect("press the cell");
    let ApplicationTransition::SetWorkspaceIcon {
        origin,
        workspace_id: chosen,
        icon,
    } = transition
    else {
        panic!("a press on a cell chooses it, got {transition:?}");
    };
    assert_eq!(origin, Outlook::Local);
    assert_eq!(chosen, workspace_id);
    assert_eq!(icon, "dev-rust");
    assert!(!picker_is_drawn(&application));
}

fn expect_workspace_listing(transition: ApplicationTransition) -> SessionListRequest {
    let ApplicationTransition::ListSessions(request) = transition else {
        panic!("opening the Workspace Picker asks for its Sessions, not {transition:?}");
    };
    assert_eq!(request.surface(), SessionListSurface::WorkspacePicker);
    assert_eq!(request.scope(), &SessionListScope::AllWorkspaces);
    request
}

/// The Workspace Picker showing two Workspaces' rows, `show_icons` as given.
/// Answers with the Application and the "acorn" Workspace's own identity.
fn workspace_picker_with_two_workspaces(
    root: &Path,
    show_icons: bool,
) -> (Application, WorkspaceId) {
    let acorn = root.join("acorn");
    let birch = root.join("birch");
    let mut application = connected_application(root);
    deliver_settings(&mut application, icons_shown(show_icons));
    let request = expect_workspace_listing(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                SemanticCommandId::WorkspaceList,
            )))
            .expect("open the Workspace Picker"),
    );
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request,
            sessions: vec![
                listed_as(SessionId::new(), "Acorn work", &acorn),
                listed_as(SessionId::new(), "Birch work", &birch),
            ],
        })
        .expect("hydrate the Workspace Picker");
    (application, WorkspaceId::directory(&acorn))
}

/// Opens the Icon Picker on the "acorn" Workspace Picker row, through its own
/// context menu.
fn open_picker_from_workspace_picker_row_menu(root: &Path) -> (Application, WorkspaceId) {
    let (mut application, workspace_id) = workspace_picker_with_two_workspaces(root, true);
    let buffer = rendered_application_buffer(&application, WIDE, TALL);
    let (column, row) = text_position(&buffer, "acorn");
    press(&mut application, MouseButton::Right, column, row);
    let items = menu_item_labels(&application, row);
    let index = items
        .iter()
        .position(|label| label.contains("Choose icon"))
        .expect("the row's menu offers Choose icon while Icons are shown");
    press(
        &mut application,
        MouseButton::Left,
        column + 1,
        row + 1 + u16::try_from(index).expect("small menu index"),
    );
    assert!(
        picker_is_drawn(&application),
        "choosing the menu item opens the Icon Picker"
    );
    (application, workspace_id)
}

#[test]
fn the_workspace_picker_row_menu_offers_choose_icon_only_while_icons_are_shown() {
    let workspace = workspace_dir();

    let (mut with_icons, _) = workspace_picker_with_two_workspaces(workspace.path(), true);
    let buffer = rendered_application_buffer(&with_icons, WIDE, TALL);
    let (column, row) = text_position(&buffer, "acorn");
    press(&mut with_icons, MouseButton::Right, column, row);
    let items = menu_item_labels(&with_icons, row);
    assert!(
        items.iter().any(|label| label.contains("Choose icon")),
        "the row's menu offers Choose icon while Icons are shown: {items:?}"
    );

    let (mut without_icons, _) = workspace_picker_with_two_workspaces(workspace.path(), false);
    let buffer = rendered_application_buffer(&without_icons, WIDE, TALL);
    let (column, row) = text_position(&buffer, "acorn");
    let before = rendered_application_rows_at(&without_icons, WIDE, TALL);
    press(&mut without_icons, MouseButton::Right, column, row);
    let after = rendered_application_rows_at(&without_icons, WIDE, TALL);
    assert_eq!(
        before, after,
        "a right press draws no menu at all while Icons are hidden"
    );
}

#[test]
fn choosing_the_workspace_picker_row_menu_item_opens_the_icon_picker() {
    let workspace = workspace_dir();
    let (application, _) = open_picker_from_workspace_picker_row_menu(workspace.path());
    assert!(picker_is_drawn(&application));
}

#[test]
fn keyboard_choice_over_a_workspace_picker_row_emits_set_workspace_icon_and_closes() {
    let workspace = workspace_dir();
    let (mut application, workspace_id) =
        open_picker_from_workspace_picker_row_menu(workspace.path());

    type_text(&mut application, "rust");
    let transition = press_key(&mut application, KeyCode::Enter);
    let ApplicationTransition::SetWorkspaceIcon {
        origin,
        workspace_id: chosen,
        icon,
    } = transition
    else {
        panic!("Enter chooses the focused glyph, got {transition:?}");
    };
    assert_eq!(origin, Outlook::Local);
    assert_eq!(chosen, workspace_id);
    assert_eq!(icon, "dev-rust");
    assert!(
        !picker_is_drawn(&application),
        "choosing closes the Icon Picker"
    );
}

#[test]
fn pointer_choice_over_a_workspace_picker_row_does_the_same() {
    let workspace = workspace_dir();
    let (mut application, workspace_id) =
        open_picker_from_workspace_picker_row_menu(workspace.path());

    type_text(&mut application, "rust");
    let buffer = rendered_application_buffer(&application, WIDE, TALL);
    let (column, row) = text_position(&buffer, "\u{e7a8}");
    let transition = click_mouse(
        &mut application,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        },
    )
    .expect("press the cell");
    let ApplicationTransition::SetWorkspaceIcon {
        origin,
        workspace_id: chosen,
        icon,
    } = transition
    else {
        panic!("a press on a cell chooses it, got {transition:?}");
    };
    assert_eq!(origin, Outlook::Local);
    assert_eq!(chosen, workspace_id);
    assert_eq!(icon, "dev-rust");
    assert!(!picker_is_drawn(&application));
}
