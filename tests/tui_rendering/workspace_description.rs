//! A Workspace's Description in the Workspace Picker: drawn for the row the
//! reader is on, edited from that row by a key or its context menu, and sent
//! to the Workspace's own Origin when saved.

use std::path::{Path, PathBuf};

use crate::support::{
    click_mouse, connected_application, deliver_settings, rendered_application_buffer,
    rendered_application_rows_at, text_position,
};
use crossterm::event::{
    Event as InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use suru::{
    managed_client::ManagedEvent,
    protocol::{
        AppearanceSettings, EffectiveSettings, Outlook, SessionId, SessionListItem,
        WorkspaceDescription, WorkspaceDescriptionChanged, WorkspaceId,
    },
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, SemanticCommandId,
        SessionListRequest, SessionListSurface,
    },
};

const WIDE: u16 = 100;
const TALL: u16 = 30;

/// A Workspace path rooted per platform, so a fixture reads as an absolute
/// path on Windows as readily as on Unix.
fn workspace(components: &[&str]) -> PathBuf {
    components.iter().fold(
        Path::new(if cfg!(windows) { r"C:\" } else { "/" }).to_owned(),
        |path, component| path.join(component),
    )
}

/// A listed Session rooted in `workspace`, whose Workspace carries
/// `description` as the server sent it.
fn rooted(
    title: &str,
    workspace: &Path,
    updated_at: u64,
    description: Option<&str>,
) -> SessionListItem {
    let mut item =
        crate::support::listed_session(SessionId::new(), title, workspace, 1, updated_at);
    let SessionListItem::Readable(summary) = &mut item else {
        unreachable!("a listed Session is readable")
    };
    summary.session.workspace.description = description.map(|text| WorkspaceDescription {
        text: text.to_owned(),
        set: false,
    });
    item
}

fn icons_shown(show_icons: bool) -> EffectiveSettings {
    EffectiveSettings {
        appearance: AppearanceSettings {
            show_icons,
            ..AppearanceSettings::default()
        },
        ..EffectiveSettings::default()
    }
}

/// The Workspace Picker opened from `here` over `sessions`, with Icons shown
/// or hidden as asked.
fn picker_over(here: &Path, sessions: Vec<SessionListItem>, show_icons: bool) -> Application {
    let mut application = connected_application(here);
    deliver_settings(&mut application, icons_shown(show_icons));
    let ApplicationTransition::ListSessions(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::WorkspaceList,
        )))
        .expect("open the Workspace Picker")
    else {
        panic!("opening the Workspace Picker asks for its Sessions");
    };
    assert_listing(&request);
    application
        .handle_event(ApplicationEvent::SessionsListed { request, sessions })
        .expect("hydrate the Workspace Picker");
    application
}

fn assert_listing(request: &SessionListRequest) {
    assert_eq!(request.surface(), SessionListSurface::WorkspacePicker);
}

/// Two Workspaces besides the one the reader is in: "ledger", described,
/// and "atlas", not. The picker opens on "here", which stands first.
fn described_and_plain(show_icons: bool) -> Application {
    picker_over(
        &workspace(&["work", "here"]),
        vec![
            rooted(
                "Ledger work",
                &workspace(&["work", "ledger"]),
                30,
                Some("Where the quarterly ledger is reconciled."),
            ),
            rooted("Atlas work", &workspace(&["work", "atlas"]), 20, None),
        ],
        show_icons,
    )
}

fn frame(application: &Application) -> String {
    rendered_application_rows_at(application, WIDE, TALL).join("\n")
}

fn press_key(application: &mut Application, code: KeyCode) -> ApplicationTransition {
    press_chord(application, code, KeyModifiers::NONE)
}

fn press_chord(
    application: &mut Application,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(code, modifiers)))
        .expect("press the key")
}

fn type_text(application: &mut Application, text: &str) {
    for character in text.chars() {
        press_key(application, KeyCode::Char(character));
    }
}

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

/// What the context menu anchored at `anchor` says, item by item.
fn menu_item_labels(application: &Application, anchor: u16) -> Vec<String> {
    let rows = rendered_application_rows_at(application, WIDE, TALL);
    let mut labels = Vec::new();
    for offset in 1..8 {
        let Some(row) = rows.get(usize::from(anchor + offset)) else {
            break;
        };
        let trimmed = row.trim();
        if trimmed.starts_with('\u{2514}') || trimmed.is_empty() {
            break;
        }
        labels.push(row.clone());
    }
    labels
}

/// Opens the row menu on the "ledger" row with a right press, and answers
/// where it stands and what it offers.
fn open_ledger_menu(application: &mut Application) -> (u16, u16, Vec<String>) {
    let buffer = rendered_application_buffer(application, WIDE, TALL);
    let (column, row) = text_position(&buffer, "ledger");
    press(application, MouseButton::Right, column, row);
    let items = menu_item_labels(application, row);
    (column, row, items)
}

fn editor_is_drawn(application: &Application) -> bool {
    frame(application).contains("Describe ")
}

#[test]
fn the_focused_row_s_description_is_drawn_beneath_the_rows() {
    let mut application = described_and_plain(true);

    assert!(
        !frame(&application).contains("quarterly ledger"),
        "only the row the reader is on has its Description drawn"
    );

    press_key(&mut application, KeyCode::Down);
    let drawn = frame(&application);
    assert!(
        drawn.contains("Where the quarterly ledger is reconciled."),
        "the focused row's Description is drawn: {drawn}"
    );
    let rows = rendered_application_rows_at(&application, WIDE, TALL);
    let description = rows
        .iter()
        .position(|row| row.contains("quarterly ledger"))
        .expect("the Description is drawn");
    let last_row = rows
        .iter()
        .position(|row| row.contains("atlas"))
        .expect("the last row is drawn");
    let footer = rows
        .iter()
        .position(|row| row.contains("Esc close"))
        .expect("the footer is drawn");
    assert!(
        last_row < description && description < footer,
        "the Description stands beneath the rows and above the footer: {rows:#?}"
    );
}

#[test]
fn a_workspace_with_no_description_draws_as_readily_as_one_with() {
    let mut application = described_and_plain(true);
    press_key(&mut application, KeyCode::Down);
    let described = rendered_application_rows_at(&application, WIDE, TALL);
    press_key(&mut application, KeyCode::Down);
    let plain = rendered_application_rows_at(&application, WIDE, TALL);

    let footer_of = |rows: &[String]| {
        rows.iter()
            .position(|row| row.contains("Esc close"))
            .expect("the footer is drawn")
    };
    assert_eq!(
        footer_of(&described),
        footer_of(&plain),
        "moving onto a Workspace with no Description moves nothing else in the picker"
    );
    let plain = plain.join("\n");
    assert!(
        !plain.contains("quarterly ledger"),
        "the Description of a row the reader left is not drawn"
    );
    for name in ["here", "ledger", "atlas"] {
        assert!(plain.contains(name), "every row is still drawn: {plain}");
    }
}

#[test]
fn a_long_description_gives_up_its_tail_rather_than_the_rows() {
    let long = format!("Starts plainly. {}Ends unseen.", "middle words ".repeat(40));
    let mut application = picker_over(
        &workspace(&["work", "here"]),
        vec![rooted(
            "Ledger work",
            &workspace(&["work", "ledger"]),
            30,
            Some(&long),
        )],
        true,
    );
    press_key(&mut application, KeyCode::Down);

    let drawn = frame(&application);
    assert!(drawn.contains("Starts plainly."), "{drawn}");
    assert!(
        !drawn.contains("Ends unseen."),
        "a Description too long for its lines is cut: {drawn}"
    );
    assert!(drawn.contains('\u{2026}'), "and says so: {drawn}");
    assert!(drawn.contains("here") && drawn.contains("Esc close"));
}

#[test]
fn a_description_the_catalog_announces_is_drawn_without_reopening_the_picker() {
    let mut application = described_and_plain(true);
    press_key(&mut application, KeyCode::Down);
    press_key(&mut application, KeyCode::Down);
    assert!(!frame(&application).contains("Where the atlas is drawn."));

    application
        .handle_event(ApplicationEvent::Managed(
            ManagedEvent::WorkspaceDescriptionChanged(WorkspaceDescriptionChanged {
                workspace_id: WorkspaceId::directory(&workspace(&["work", "atlas"])),
                description: Some(WorkspaceDescription {
                    text: "Where the atlas is drawn.".to_owned(),
                    set: true,
                }),
            }),
        ))
        .expect("take the catalog change");

    assert!(
        frame(&application).contains("Where the atlas is drawn."),
        "the focused row's new Description is drawn at once"
    );
}

#[test]
fn the_footer_names_the_key_that_edits_the_description() {
    let application = described_and_plain(true);
    assert!(
        frame(&application).contains("Ctrl+E describe"),
        "the picker says how to describe the focused Workspace"
    );
}

#[test]
fn ctrl_e_opens_an_editor_holding_the_focused_workspace_s_description() {
    let mut application = described_and_plain(true);
    press_key(&mut application, KeyCode::Down);

    press_chord(&mut application, KeyCode::Char('e'), KeyModifiers::CONTROL);

    let drawn = frame(&application);
    assert!(
        drawn.contains("Describe ledger"),
        "the editor names the Workspace it describes: {drawn}"
    );
    assert!(
        drawn.contains("Where the quarterly ledger is reconciled."),
        "the editor begins from the Description the Workspace carries: {drawn}"
    );
    assert!(
        drawn.contains("Enter save") && drawn.contains("Esc cancel"),
        "the editor says how to save and cancel: {drawn}"
    );
}

#[test]
fn saving_asks_the_workspace_s_origin_to_set_what_the_reader_typed() {
    let mut application = described_and_plain(true);
    press_key(&mut application, KeyCode::Down);
    press_key(&mut application, KeyCode::Down);
    press_chord(&mut application, KeyCode::Char('e'), KeyModifiers::CONTROL);
    assert!(editor_is_drawn(&application));

    type_text(&mut application, "Maps.");
    let transition = press_key(&mut application, KeyCode::Enter);

    assert_eq!(
        transition,
        ApplicationTransition::SetWorkspaceDescription {
            origin: Outlook::Local,
            workspace_id: WorkspaceId::directory(&workspace(&["work", "atlas"])),
            description: "Maps.".to_owned(),
        }
    );
    assert!(!editor_is_drawn(&application), "saving closes the editor");
    assert!(
        frame(&application).contains("Search:"),
        "and leaves the reader in the picker"
    );
}

#[test]
fn backspace_edits_the_description_and_ctrl_u_clears_it_for_derivation() {
    let mut application = described_and_plain(true);
    press_key(&mut application, KeyCode::Down);
    press_chord(&mut application, KeyCode::Char('e'), KeyModifiers::CONTROL);

    press_key(&mut application, KeyCode::Backspace);
    assert!(
        frame(&application).contains("Where the quarterly ledger is reconciled")
            && !frame(&application).contains("reconciled."),
        "Backspace takes the last character back"
    );

    press_chord(&mut application, KeyCode::Char('u'), KeyModifiers::CONTROL);
    assert!(!frame(&application).contains("quarterly ledger"));
    let transition = press_key(&mut application, KeyCode::Enter);

    assert_eq!(
        transition,
        ApplicationTransition::SetWorkspaceDescription {
            origin: Outlook::Local,
            workspace_id: WorkspaceId::directory(&workspace(&["work", "ledger"])),
            description: String::new(),
        },
        "saving a cleared Description asks for it cleared, so it may be derived again"
    );
}

#[test]
fn a_paste_goes_into_the_description_on_one_line() {
    let mut application = described_and_plain(true);
    press_key(&mut application, KeyCode::Down);
    press_key(&mut application, KeyCode::Down);
    press_chord(&mut application, KeyCode::Char('e'), KeyModifiers::CONTROL);

    application
        .handle_terminal_event(InputEvent::Paste("Draws\nthe atlas.".to_owned()))
        .expect("paste into the editor");
    let transition = press_key(&mut application, KeyCode::Enter);

    let ApplicationTransition::SetWorkspaceDescription { description, .. } = transition else {
        panic!("Enter saves, got {transition:?}");
    };
    assert_eq!(description, "Draws the atlas.");
}

#[test]
fn escape_closes_the_editor_without_saving() {
    let mut application = described_and_plain(true);
    press_key(&mut application, KeyCode::Down);
    press_chord(&mut application, KeyCode::Char('e'), KeyModifiers::CONTROL);
    type_text(&mut application, " More.");

    assert_eq!(
        press_key(&mut application, KeyCode::Esc),
        ApplicationTransition::Continue
    );

    assert!(!editor_is_drawn(&application));
    let drawn = frame(&application);
    assert!(drawn.contains("Search:"), "the picker stands: {drawn}");
    assert!(
        !drawn.contains("More."),
        "nothing typed into a cancelled editor is kept: {drawn}"
    );
}

#[test]
fn the_row_menu_offers_edit_description_whether_or_not_icons_are_shown() {
    let mut with_icons = described_and_plain(true);
    let (_, _, items) = open_ledger_menu(&mut with_icons);
    assert!(
        items.iter().any(|label| label.contains("Edit description")),
        "{items:?}"
    );
    assert!(
        items.iter().any(|label| label.contains("Choose icon")),
        "{items:?}"
    );

    let mut without_icons = described_and_plain(false);
    let (_, _, items) = open_ledger_menu(&mut without_icons);
    assert!(
        items.iter().any(|label| label.contains("Edit description")),
        "describing a Workspace needs no glyph: {items:?}"
    );
    assert!(
        !items.iter().any(|label| label.contains("Choose icon")),
        "{items:?}"
    );
}

#[test]
fn choosing_edit_description_from_the_row_menu_edits_that_row() {
    let mut application = described_and_plain(true);
    let (column, row, items) = open_ledger_menu(&mut application);
    let index = items
        .iter()
        .position(|label| label.contains("Edit description"))
        .expect("the row's menu offers Edit description");

    press(
        &mut application,
        MouseButton::Left,
        column + 1,
        row + 1 + u16::try_from(index).expect("small menu index"),
    );

    let drawn = frame(&application);
    assert!(
        drawn.contains("Describe ledger"),
        "the menu edits the row it was opened on, not the one the reader is on: {drawn}"
    );
    assert!(drawn.contains("Where the quarterly ledger is reconciled."));
}

#[test]
fn the_arrows_walk_the_row_menu_and_enter_acts_on_the_item_the_reader_is_on() {
    let mut application = described_and_plain(true);
    let (_, _, items) = open_ledger_menu(&mut application);
    let index = items
        .iter()
        .position(|label| label.contains("Edit description"))
        .expect("the row's menu offers Edit description");

    for _ in 0..index {
        press_key(&mut application, KeyCode::Down);
    }
    press_key(&mut application, KeyCode::Enter);

    assert!(
        frame(&application).contains("Describe ledger"),
        "Enter on Edit description opens the editor"
    );
}

#[test]
fn a_remote_workspace_s_description_is_saved_to_that_remote() {
    let mut application = crate::support::application_looking_at_studio();
    let ApplicationTransition::ListSessions(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::WorkspaceList,
        )))
        .expect("open the Workspace Picker")
    else {
        panic!("opening the Workspace Picker asks its Outlook for Sessions");
    };
    assert_listing(&request);
    let studio_work = workspace(&["studio", "mural"]);
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request,
            sessions: vec![rooted("Mural work", &studio_work, 30, None)],
        })
        .expect("hydrate the Workspace Picker");
    let buffer = rendered_application_buffer(&application, WIDE, TALL);
    let (_, row) = text_position(&buffer, "mural");
    let rows = rendered_application_rows_at(&application, WIDE, TALL);
    if !rows[usize::from(row)].contains('›') {
        press_key(&mut application, KeyCode::Down);
    }

    press_chord(&mut application, KeyCode::Char('e'), KeyModifiers::CONTROL);
    type_text(&mut application, "Painted on the studio.");
    let transition = press_key(&mut application, KeyCode::Enter);

    assert_eq!(
        transition,
        ApplicationTransition::SetWorkspaceDescription {
            origin: Outlook::Remote("studio".to_owned()),
            workspace_id: WorkspaceId::directory(&studio_work),
            description: "Painted on the studio.".to_owned(),
        },
        "a Remote's Workspace is described at its own Origin"
    );
}

#[test]
fn describing_a_workspace_on_an_unreachable_remote_is_refused_in_view() {
    for command in [
        SemanticCommandId::WorkspaceDescriptionEdit,
        SemanticCommandId::WorkspaceDescriptionSave,
    ] {
        let mut application = crate::support::application_looking_at_studio();
        crate::support::studio_stops_answering(
            &mut application,
            1,
            std::time::Duration::from_secs(5),
        );
        assert_eq!(
            application
                .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                    command
                )))
                .expect("refuse work bound for the Remote"),
            ApplicationTransition::Continue,
            "{command:?} is applied on the Origin"
        );
        let screen = crate::support::rendered_application_rows(&application).join("\n");
        assert!(
            screen.contains("studio is unreachable"),
            "{command:?} is refused where the reader can see it: {screen}"
        );
    }
}
