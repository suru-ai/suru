//! The rename dialog: opened over a Session from its Sidebar row's context
//! menu, a press on the open Session's header Title, or `/rename`, written
//! in by hand or filled with a proposal its Origin derives, and saved with
//! Enter.

use crate::support::{
    click_mouse, connected_application, deliver_settings, enter_session,
    rendered_application_buffer, rendered_application_rows_at, text_position, type_terminal_text,
    workspace_dir,
};
use crossterm::event::{
    Event as InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use std::path::Path;
use suru::{
    managed_client::{ManagedEvent, SessionEvent},
    protocol::{
        EffectiveSettings, Outlook, SessionId, SessionReference, SessionTitleChanged,
        SidebarSettings, SidebarVisibility,
    },
    tui::{Application, ApplicationEvent, ApplicationTransition},
};

const WIDE: u16 = 100;
const TALL: u16 = 24;
/// A column well inside the Sidebar's own, which is where a reader points at
/// a row.
const SIDEBAR_CELL: u16 = 4;

fn sidebar_shown() -> EffectiveSettings {
    EffectiveSettings {
        sidebar: SidebarSettings {
            initial_visibility: SidebarVisibility::Shown,
            ..SidebarSettings::default()
        },
        ..EffectiveSettings::default()
    }
}

/// A Sidebar showing one Session titled `title`.
fn sidebar_with_one_session(workspace: &Path, session_id: SessionId, title: &str) -> Application {
    let mut application = connected_application(workspace);
    let ApplicationTransition::ListSessions(request) =
        deliver_settings(&mut application, sidebar_shown())
    else {
        panic!("a Sidebar coming into view asks for its Sessions");
    };
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request,
            sessions: vec![crate::support::listed_session(
                session_id, title, workspace, 1, 1,
            )],
        })
        .expect("hydrate the Sidebar");
    application
}

fn rows(application: &Application) -> Vec<String> {
    rendered_application_rows_at(application, WIDE, TALL)
}

fn drawn(application: &Application) -> String {
    rows(application).join("\n")
}

fn dialog_is_drawn(application: &Application) -> bool {
    drawn(application).contains(" Rename ")
}

fn drawn_at(application: &Application, needle: &str) -> u16 {
    let rows = rows(application);
    u16::try_from(
        rows.iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("rendered frame did not contain {needle:?}: {rows:#?}")),
    )
    .expect("the row fits a screen row")
}

fn press(
    application: &mut Application,
    button: MouseButton,
    column: u16,
    row: u16,
) -> ApplicationTransition {
    click_mouse(
        application,
        MouseEvent {
            kind: MouseEventKind::Down(button),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        },
    )
    .expect("handle a press")
}

fn key(
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
        key(application, KeyCode::Char(character), KeyModifiers::NONE);
    }
}

/// What the context menu anchored at `anchor` offers, top to bottom.
fn menu_items(application: &Application, anchor: u16) -> Vec<String> {
    let rows = rows(application);
    (1..8)
        .map_while(|offset| rows.get(usize::from(anchor + offset)))
        .take_while(|row| !row.trim().starts_with('\u{2514}') && !row.trim().is_empty())
        .cloned()
        .collect()
}

/// Opens the dialog over the one listed Session through its row's context
/// menu, answering the opening the Origin is asked about.
fn open_from_sidebar(application: &mut Application) -> (SessionReference, uuid::Uuid) {
    let anchor = drawn_at(application, "Wanted work");
    press(application, MouseButton::Right, SIDEBAR_CELL, anchor);
    let index = menu_items(application, anchor)
        .iter()
        .position(|label| label.contains("Rename"))
        .expect("the menu offers Rename");
    let transition = press(
        application,
        MouseButton::Left,
        SIDEBAR_CELL + 1,
        anchor + 1 + u16::try_from(index).expect("small menu index"),
    );
    let ApplicationTransition::AskTitleProposalOffer { session, opening } = transition else {
        panic!("opening the dialog asks the Origin whether it proposes Titles: {transition:?}");
    };
    (session, opening)
}

fn offer(application: &mut Application, opening: uuid::Uuid, offered: bool) {
    application
        .handle_event(ApplicationEvent::TitleProposalOffered { opening, offered })
        .expect("take the Origin's offer");
}

#[test]
fn the_sidebar_menu_offers_rename_between_the_shelf_and_deletion() {
    let workspace = workspace_dir();
    let mut application =
        sidebar_with_one_session(workspace.path(), SessionId::new(), "Wanted work");
    let anchor = drawn_at(&application, "Wanted work");
    press(&mut application, MouseButton::Right, SIDEBAR_CELL, anchor);
    let items = menu_items(&application, anchor);
    let position = |label: &str| {
        items
            .iter()
            .position(|item| item.contains(label))
            .unwrap_or_else(|| panic!("the menu offers {label}: {items:?}"))
    };
    assert!(position("Settle") < position("Rename"));
    assert!(position("Rename") < position("Delete"));
}

#[test]
fn renaming_from_the_sidebar_saves_what_was_written() {
    let workspace = workspace_dir();
    let session_id = SessionId::new();
    let mut application = sidebar_with_one_session(workspace.path(), session_id, "Wanted work");
    let (session, opening) = open_from_sidebar(&mut application);
    assert_eq!(session, SessionReference::new(Outlook::Local, session_id));
    offer(&mut application, opening, false);
    assert!(dialog_is_drawn(&application));
    assert!(
        drawn(&application).contains("› Wanted work"),
        "the field holds the Title it renames: {}",
        drawn(&application)
    );

    key(&mut application, KeyCode::Char('u'), KeyModifiers::CONTROL);
    type_text(&mut application, "Map the seam");
    let transition = key(&mut application, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        transition,
        ApplicationTransition::SetSessionTitle {
            session: session.clone(),
            title: "Map the seam".to_owned(),
        }
    );
    assert!(drawn(&application).contains("Saving…"));

    application
        .handle_event(ApplicationEvent::SessionTitleSaved { session })
        .expect("hear the rename landed");
    assert!(
        !dialog_is_drawn(&application),
        "a landed rename closes the dialog"
    );
}

#[test]
fn a_refused_rename_says_why_beside_the_text() {
    let workspace = workspace_dir();
    let mut application =
        sidebar_with_one_session(workspace.path(), SessionId::new(), "Wanted work");
    let (session, _) = open_from_sidebar(&mut application);
    type_text(&mut application, "!");
    key(&mut application, KeyCode::Enter, KeyModifiers::NONE);
    application
        .handle_event(ApplicationEvent::SessionTitleSetFailed {
            session,
            error: "Session does not exist on this server instance".to_owned(),
        })
        .expect("hear the rename was refused");
    let text = drawn(&application);
    assert!(text.contains("Wanted work!"), "{text}");
    assert!(text.contains("Session does not exist"), "{text}");
}

#[test]
fn a_blank_title_is_not_sent() {
    let workspace = workspace_dir();
    let mut application =
        sidebar_with_one_session(workspace.path(), SessionId::new(), "Wanted work");
    open_from_sidebar(&mut application);
    key(&mut application, KeyCode::Char('u'), KeyModifiers::CONTROL);
    type_text(&mut application, "   ");
    assert_eq!(
        key(&mut application, KeyCode::Enter, KeyModifiers::NONE),
        ApplicationTransition::Continue
    );
    assert!(drawn(&application).contains("A Title cannot be blank"));
}

#[test]
fn deriving_is_offered_only_where_the_origin_offers_it() {
    let workspace = workspace_dir();
    let mut application =
        sidebar_with_one_session(workspace.path(), SessionId::new(), "Wanted work");
    let (_, opening) = open_from_sidebar(&mut application);
    assert!(
        !drawn(&application).contains("Derive"),
        "nothing is offered until the Origin says"
    );
    assert_eq!(
        key(&mut application, KeyCode::Char('g'), KeyModifiers::CONTROL),
        ApplicationTransition::Continue
    );
    offer(&mut application, opening, false);
    assert!(
        !drawn(&application).contains("Derive"),
        "an Origin whose Setting derives no Title shows no way to ask for one"
    );
}

#[test]
fn a_derived_proposal_fills_the_field_and_saving_keeps_it() {
    let workspace = workspace_dir();
    let mut application =
        sidebar_with_one_session(workspace.path(), SessionId::new(), "Wanted work");
    let (session, opening) = open_from_sidebar(&mut application);
    offer(&mut application, opening, true);
    assert!(drawn(&application).contains("[ Derive a Title ]"));

    let buffer = rendered_application_buffer(&application, WIDE, TALL);
    let (column, row) = text_position(&buffer, "[ Derive a Title ]");
    let ApplicationTransition::ProposeSessionTitle {
        session: proposed_for,
        request,
    } = press(&mut application, MouseButton::Left, column + 2, row)
    else {
        panic!("pressing the button asks for a proposal");
    };
    assert_eq!(proposed_for, session);
    assert!(drawn(&application).contains("Deriving a Title"));
    type_text(&mut application, "lost");
    assert!(
        !drawn(&application).contains("lost"),
        "the field is held while a proposal is on its way"
    );

    application
        .handle_event(ApplicationEvent::TitleProposed {
            request,
            result: Ok("Map the Provider seam".to_owned()),
        })
        .expect("take the proposal");
    assert!(drawn(&application).contains("› Map the Provider seam"));
    assert_eq!(
        key(&mut application, KeyCode::Enter, KeyModifiers::NONE),
        ApplicationTransition::SetSessionTitle {
            session,
            title: "Map the Provider seam".to_owned(),
        }
    );
}

#[test]
fn escape_stops_a_proposal_and_gives_the_field_back_then_closes() {
    let workspace = workspace_dir();
    let mut application =
        sidebar_with_one_session(workspace.path(), SessionId::new(), "Wanted work");
    let (_, opening) = open_from_sidebar(&mut application);
    offer(&mut application, opening, true);
    let ApplicationTransition::ProposeSessionTitle { request, .. } =
        key(&mut application, KeyCode::Char('g'), KeyModifiers::CONTROL)
    else {
        panic!("Ctrl+G asks for a proposal");
    };
    assert_eq!(application.awaited_title_proposal(), Some(request));

    key(&mut application, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(
        application.awaited_title_proposal(),
        None,
        "the run loop lets go of a proposal no one waits on"
    );
    assert!(
        dialog_is_drawn(&application),
        "the first Escape only stops deriving"
    );
    assert!(drawn(&application).contains("› Wanted work"));

    assert_eq!(
        key(&mut application, KeyCode::Esc, KeyModifiers::NONE),
        ApplicationTransition::Continue
    );
    assert!(
        !dialog_is_drawn(&application),
        "the second closes the dialog"
    );
}

#[test]
fn a_failed_proposal_says_why_and_keeps_the_text() {
    let workspace = workspace_dir();
    let mut application =
        sidebar_with_one_session(workspace.path(), SessionId::new(), "Wanted work");
    let (_, opening) = open_from_sidebar(&mut application);
    offer(&mut application, opening, true);
    let ApplicationTransition::ProposeSessionTitle { request, .. } =
        key(&mut application, KeyCode::Char('g'), KeyModifiers::CONTROL)
    else {
        panic!("Ctrl+G asks for a proposal");
    };
    application
        .handle_event(ApplicationEvent::TitleProposed {
            request,
            result: Err("the Provider did not answer in time".to_owned()),
        })
        .expect("take the refusal");
    let text = drawn(&application);
    assert!(text.contains("› Wanted work"), "{text}");
    assert!(
        text.contains("Could not derive a Title: the Provider did not answer…"),
        "{text}"
    );
}

#[test]
fn the_field_follows_a_title_changed_elsewhere_until_it_is_written_in() {
    let workspace = workspace_dir();
    let session_id = SessionId::new();
    let mut application = sidebar_with_one_session(workspace.path(), session_id, "Wanted work");
    open_from_sidebar(&mut application);
    let retitle = |application: &mut Application, title: &str| {
        application
            .handle_event(ApplicationEvent::Managed(
                ManagedEvent::SessionTitleChanged(SessionTitleChanged {
                    session_id,
                    title: title.to_owned(),
                    icon: None,
                }),
            ))
            .expect("take a retitle");
    };
    retitle(&mut application, "Derived meanwhile");
    assert!(drawn(&application).contains("› Derived meanwhile"));
    type_text(&mut application, "!");
    retitle(&mut application, "Renamed elsewhere");
    assert!(drawn(&application).contains("› Derived meanwhile!"));
}

/// A connected client with one Session open, titled `Alpha work`.
fn open_session(workspace: &Path) -> (Application, SessionReference) {
    let mut application = connected_application(workspace);
    deliver_settings(&mut application, EffectiveSettings::default());
    let (session_id, mut snapshot) = enter_session(&mut application, workspace);
    snapshot.title = "Alpha work".into();
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(snapshot)))
        .expect("show the titled Session");
    (
        application,
        SessionReference::new(Outlook::Local, session_id),
    )
}

#[test]
fn pressing_the_header_title_opens_the_dialog() {
    let workspace = workspace_dir();
    let (mut application, session) = open_session(workspace.path());
    let buffer = rendered_application_buffer(&application, WIDE, TALL);
    let (column, row) = text_position(&buffer, "Alpha work");
    let transition = press(&mut application, MouseButton::Left, column + 1, row);
    assert!(
        matches!(
            &transition,
            ApplicationTransition::AskTitleProposalOffer { session: asked, .. } if *asked == session),
        "{transition:?}"
    );
    assert!(drawn(&application).contains("› Alpha work"));
}

#[test]
fn slash_rename_with_a_title_renames_the_open_session_at_once() {
    let workspace = workspace_dir();
    let (mut application, session) = open_session(workspace.path());
    type_terminal_text(&mut application, "/rename Map the   seam");
    assert_eq!(
        key(&mut application, KeyCode::Enter, KeyModifiers::NONE),
        ApplicationTransition::SetSessionTitle {
            session,
            title: "Map the seam".to_owned(),
        }
    );
    assert!(!dialog_is_drawn(&application));
}

#[test]
fn bare_slash_rename_opens_the_dialog_over_the_open_session() {
    let workspace = workspace_dir();
    let (mut application, session) = open_session(workspace.path());
    type_terminal_text(&mut application, "/rename");
    let transition = key(&mut application, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        matches!(
            &transition,
            ApplicationTransition::AskTitleProposalOffer { session: asked, .. } if *asked == session),
        "{transition:?}"
    );
    assert!(drawn(&application).contains("› Alpha work"));
}

#[test]
fn slash_rename_with_an_overlong_title_opens_it_in_the_dialog_to_be_shortened() {
    let workspace = workspace_dir();
    let (mut application, _) = open_session(workspace.path());
    let long = "word ".repeat(20);
    type_terminal_text(&mut application, &format!("/rename {long}"));
    let transition = key(&mut application, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        matches!(
            transition,
            ApplicationTransition::AskTitleProposalOffer { .. }
        ),
        "{transition:?}"
    );
    let text = drawn(&application);
    assert!(text.contains("› word word"), "{text}");
    assert!(
        text.contains("A Title runs to at most 80 characters"),
        "{text}"
    );
}

#[test]
fn deleting_the_session_closes_the_dialog_and_lets_its_proposal_go() {
    let workspace = workspace_dir();
    let session_id = SessionId::new();
    let mut application = sidebar_with_one_session(workspace.path(), session_id, "Wanted work");
    let (_, opening) = open_from_sidebar(&mut application);
    offer(&mut application, opening, true);
    key(&mut application, KeyCode::Char('g'), KeyModifiers::CONTROL);
    assert!(application.awaited_title_proposal().is_some());

    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::SessionDeleted(
            suru::protocol::SessionDeleted { session_id },
        )))
        .expect("take the deletion");
    assert!(!dialog_is_drawn(&application));
    assert_eq!(application.awaited_title_proposal(), None);
}

#[test]
fn the_dialog_wears_the_sessions_icon_while_icons_are_shown() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    deliver_settings(
        &mut application,
        EffectiveSettings {
            appearance: suru::protocol::AppearanceSettings {
                show_icons: true,
                ..suru::protocol::AppearanceSettings::default()
            },
            ..EffectiveSettings::default()
        },
    );
    let (_, mut snapshot) = enter_session(&mut application, workspace.path());
    snapshot.title = "Alpha work".into();
    snapshot.icon = Some("md-bug".to_owned());
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(snapshot)))
        .expect("show the iconed Session");
    type_terminal_text(&mut application, "/rename");
    key(&mut application, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        drawn(&application).contains("\u{f00e4} Rename"),
        "{}",
        drawn(&application)
    );
}
