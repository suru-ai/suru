//! The Workspace Picker: opening it, the Workspaces it puts on offer, the
//! order it stands them in, and walking away from it unchanged.

use std::path::{Path, PathBuf};

use crate::support::{
    connected_application, noncanonical_spelling, rendered_application_rows,
    rendered_application_rows_at, rendered_row, type_terminal_text, workspace_dir,
};
use crossterm::event::{
    Event as InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use suru::{
    protocol::{
        ModelAvailability, Session, SessionId, SessionListItem, SessionStatus, SessionSummary,
        SessionTimestamp, Workspace,
    },
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, SemanticCommandId,
        SessionListRequest, SessionListScope, SessionListSurface,
    },
};

#[test]
fn the_workspace_command_opens_a_picker_loading_every_workspace_s_sessions() {
    let mut application = connected_application(&workspace(&["work", "here"]));
    type_terminal_text(&mut application, "/workspace");

    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("/workspace")
    );
    expect_workspace_listing(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("select /workspace"),
    );

    let picker = rendered_application_rows(&application).join("\n");
    assert!(picker.contains("Workspaces"));
    assert!(
        picker.contains("Loading Workspaces"),
        "the listing the picker derives its rows from is still on its way, and \
         an empty box would read as no Workspaces at all"
    );
}

/// "Project" is vocabulary Suru avoids, so it finds the command without ever
/// being offered as one: the completion row names only the canonical spelling.
#[test]
fn the_project_synonym_finds_the_command_while_the_completion_names_only_workspace() {
    let mut application = connected_application(&workspace(&["work", "here"]));
    type_terminal_text(&mut application, "/project");

    let rows = rendered_application_rows(&application);
    let offered = &rows[rendered_row(&rows, "Switch Workspace")];
    assert!(
        offered.contains("/workspace"),
        "the alias matches, and the row it matches names the command Suru calls it: {rows:?}"
    );
    assert!(
        !offered.contains("/project"),
        "no completion row offers the alias itself: {rows:?}"
    );

    expect_workspace_listing(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("select the command /project found"),
    );
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Workspaces")
    );
}

#[test]
fn the_leader_chord_opens_the_picker() {
    let mut application = connected_application(&workspace(&["work", "here"]));

    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL,
        )))
        .expect("begin the leader chord");
    expect_workspace_listing(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('w'),
                KeyModifiers::NONE,
            )))
            .expect("finish Ctrl+X W"),
    );

    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("Workspaces")
    );
}

#[test]
fn the_current_workspace_stands_first_and_the_rest_by_which_held_work_most_recently() {
    let here = workspace(&["work", "here"]);
    let ledger = workspace(&["work", "ledger"]);
    let atlas = workspace(&["work", "atlas"]);
    let mut application = connected_application(&here);

    open_picker_with(
        &mut application,
        vec![
            rooted("Older work", &ledger, 10),
            rooted("Newest work", &atlas, 30),
            rooted("Work where I am", &here, 20),
        ],
    );

    let rows = picker_rows(&application);
    assert_eq!(
        rows.len(),
        3,
        "one row per Workspace the Sessions are rooted in: {rows:?}"
    );
    assert!(rows[0].contains("here"), "{rows:?}");
    assert!(
        rows[0].contains("[current]"),
        "the Workspace the client works in stands first and says so: {rows:?}"
    );
    assert!(
        rows[1].contains("atlas"),
        "then the Workspace whose newest Session is newest: {rows:?}"
    );
    assert!(rows[2].contains("ledger"), "{rows:?}");
}

/// A Workspace with no Sessions yet is still somewhere the reader can be: the
/// one this client is working in stands whether or not any work is rooted
/// there.
#[test]
fn the_workspace_the_client_works_in_stands_even_with_no_work_rooted_there() {
    let here = workspace(&["work", "here"]);
    let ledger = workspace(&["work", "ledger"]);
    let mut application = connected_application(&here);

    open_picker_with(
        &mut application,
        vec![rooted("Work elsewhere", &ledger, 10)],
    );

    let rows = picker_rows(&application);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(
        rows[0].contains("here") && rows[0].contains("[current]"),
        "{rows:?}"
    );
    assert!(rows[1].contains("ledger"), "{rows:?}");
}

#[test]
fn a_row_names_the_workspace_and_spells_its_path_beside_it() {
    let here = workspace(&["work", "here"]);
    let one = workspace(&["one", "api"]);
    let two = workspace(&["two", "api"]);
    let mut application = connected_application(&here);

    open_picker_with(
        &mut application,
        vec![rooted("Newer", &one, 30), rooted("Older", &two, 20)],
    );

    let rows = picker_rows(&application);
    let named = rows
        .iter()
        .filter(|row| row.contains("api"))
        .collect::<Vec<_>>();
    assert_eq!(named.len(), 2, "{rows:?}");
    assert!(
        named[0].contains(one.to_string_lossy().as_ref()),
        "two Workspaces named alike are told apart by the path beside the name: {rows:?}"
    );
    assert!(
        named[1].contains(two.to_string_lossy().as_ref()),
        "{rows:?}"
    );
}

/// The server canonicalizes the Workspace it roots a Session at, and the
/// client reads its own launch directory the same way, so a launch spelling
/// that differs from the canonical one never stands the same directory twice.
#[test]
fn two_spellings_of_one_directory_make_one_row() {
    let here = workspace_dir();
    let mut application = connected_application(&noncanonical_spelling(&here));

    open_picker_with(
        &mut application,
        vec![rooted("Work where I am", here.path(), 20)],
    );

    let rows = picker_rows(&application);
    assert_eq!(
        rows.len(),
        1,
        "the launch spelling and the Session's rooting are one Workspace: {rows:?}"
    );
    assert!(rows[0].contains("[current]"), "{rows:?}");
}

#[test]
fn the_arrows_walk_the_rows_and_wrap_past_the_ends() {
    let here = workspace(&["work", "here"]);
    let ledger = workspace(&["work", "ledger"]);
    let atlas = workspace(&["work", "atlas"]);
    let mut application = connected_application(&here);

    open_picker_with(
        &mut application,
        vec![rooted("Older", &ledger, 10), rooted("Newer", &atlas, 30)],
    );

    assert_eq!(
        selected_row(&application),
        "here",
        "the picker opens where the reader is"
    );

    press(&mut application, KeyCode::Down);
    assert_eq!(selected_row(&application), "atlas");
    press(&mut application, KeyCode::Down);
    assert_eq!(selected_row(&application), "ledger");
    press(&mut application, KeyCode::Down);
    assert_eq!(
        selected_row(&application),
        "here",
        "walking past the last row comes back to the first"
    );
    press(&mut application, KeyCode::Up);
    assert_eq!(
        selected_row(&application),
        "ledger",
        "and walking back past the first comes to the last"
    );
}

/// A page is ten rows, as it is in the session picker, so it takes a list
/// longer than a page to tell paging from stepping.
#[test]
fn the_paging_keys_move_the_selection_a_page_at_a_time() {
    let here = workspace(&["work", "here"]);
    let mut application = connected_application(&here);
    let elsewhere = (1..=11)
        .map(|index| workspace(&["work", &format!("ws{index:02}")]))
        .collect::<Vec<_>>();

    open_picker_with(
        &mut application,
        elsewhere
            .iter()
            .enumerate()
            .map(|(rank, path)| rooted("Work", path, 100 - rank as u64))
            .collect(),
    );

    assert_eq!(selected_row(&application), "here");
    press(&mut application, KeyCode::PageDown);
    assert_eq!(
        selected_row(&application),
        "ws10",
        "ten rows on from the first, and in view"
    );
    press(&mut application, KeyCode::PageUp);
    assert_eq!(selected_row(&application), "here");
}

#[test]
fn escape_closes_the_picker_leaving_everything_as_it_was() {
    let here = workspace(&["work", "here"]);
    let ledger = workspace(&["work", "ledger"]);
    let mut application = connected_application(&here);
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "an unsent draft".to_owned(),
        )))
        .expect("type a Landing draft");

    open_picker_with(&mut application, vec![rooted("Older", &ledger, 10)]);
    press(&mut application, KeyCode::Down);

    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE,
            )))
            .expect("close the Workspace Picker"),
        ApplicationTransition::Continue,
        "backing out of the picker asks the server for nothing"
    );

    // Wide enough for the footer to spell the Workspace rather than truncate it.
    let landing = rendered_application_rows_at(&application, 120, 20).join("\n");
    assert!(!landing.contains("ledger"), "the picker is gone: {landing}");
    assert!(
        landing.contains(here.to_string_lossy().as_ref()),
        "the Workspace the client works in is where it was: {landing}"
    );
    assert!(landing.contains("an unsent draft"), "{landing}");
}

/// The path is what tells two Workspaces of the same name apart, so it holds
/// its place down to the narrowest terminal the frame will draw, giving up its
/// leading directories rather than the tail that names the work.
#[test]
fn a_narrow_row_gives_up_the_head_of_the_path_rather_than_the_path() {
    let here = workspace(&["work", "here"]);
    let ledger = workspace(&["work", "ledger"]);
    let mut application = connected_application(&here);

    open_picker_with(&mut application, vec![rooted("Older", &ledger, 10)]);

    let rows = picker_rows_at(&application, 28, 7);
    let row = rows
        .iter()
        .find(|row| row.contains("ledger"))
        .unwrap_or_else(|| panic!("no row names the Workspace: {rows:?}"));
    assert!(
        row.contains(Path::new("work").join("ledger").to_string_lossy().as_ref()),
        "the row still spells where the Workspace stands: {rows:?}"
    );
}

/// A listing the server refuses is reported where the reader is looking, and
/// the picker stands rather than closing behind the refusal.
#[test]
fn a_refused_listing_is_reported_in_the_picker() {
    let here = workspace(&["work", "here"]);
    let mut application = connected_application(&here);
    let request = expect_workspace_listing(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                SemanticCommandId::WorkspaceList,
            )))
            .expect("open the Workspace Picker"),
    );

    application
        .handle_event(ApplicationEvent::SessionListingFailed {
            request,
            error: "the server is unreachable".to_owned(),
        })
        .expect("refuse the Workspace Picker's listing");

    let picker = rendered_application_rows(&application).join("\n");
    assert!(picker.contains("Workspaces"), "the picker stands: {picker}");
    assert!(picker.contains("the server is unreachable"), "{picker}");
    assert!(
        !picker.contains("Loading Workspaces"),
        "the wait is over, refused: {picker}"
    );
}

/// The picker's rows answer a pointer the way the session picker's rows do:
/// a press over the list is not a way to choose, so it moves nothing and
/// leaves the picker standing.
#[test]
fn a_press_over_the_rows_moves_nothing() {
    let here = workspace(&["work", "here"]);
    let ledger = workspace(&["work", "ledger"]);
    let mut application = connected_application(&here);

    open_picker_with(&mut application, vec![rooted("Older", &ledger, 10)]);
    let before = rendered_application_rows(&application);
    let row = rendered_row(&before, "ledger");

    application
        .handle_terminal_event(InputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 20,
            row: row.try_into().expect("the row fits terminal coordinates"),
            modifiers: KeyModifiers::NONE,
        }))
        .expect("press over a Workspace row");

    assert_eq!(rendered_application_rows(&application), before);
}

/// A Workspace path rooted per platform, so a fixture reads as an absolute
/// path on Windows as readily as on Unix — `Path::is_absolute` is
/// platform-defined, and a Workspace is always somewhere absolute.
fn workspace(components: &[&str]) -> PathBuf {
    components.iter().fold(
        Path::new(if cfg!(windows) { r"C:\" } else { "/" }).to_owned(),
        |path, component| path.join(component),
    )
}

/// A listed Session rooted at the Workspace named, last active when the test
/// says — which is what a catalog spanning several Workspaces is made of, and
/// what the picker's ordering is derived from.
fn rooted(title: &str, workspace: &Path, updated_at: u64) -> SessionListItem {
    SessionListItem::Readable(Box::new(SessionSummary {
        session: Session {
            id: SessionId::new(),
            workspace: Workspace {
                path: workspace.to_owned(),
            },
            agent_selection: None,
            agent_selection_availability: ModelAvailability::Available,
            status: SessionStatus::Idle,
            parent: None,
        },
        title: title.to_owned(),
        emoji: None,
        settled_at: None,
        working_since: None,
        total_usage: None,
        created_at: SessionTimestamp(1),
        updated_at: SessionTimestamp(updated_at),
    }))
}

fn open_picker_with(application: &mut Application, sessions: Vec<SessionListItem>) {
    let request = expect_workspace_listing(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                SemanticCommandId::WorkspaceList,
            )))
            .expect("open the Workspace Picker"),
    );
    application
        .handle_event(ApplicationEvent::SessionsListed { request, sessions })
        .expect("hydrate the Workspace Picker");
}

fn expect_workspace_listing(transition: ApplicationTransition) -> SessionListRequest {
    let ApplicationTransition::ListSessions(request) = transition else {
        panic!("opening the Workspace Picker asks for its Sessions, not {transition:?}");
    };
    assert_eq!(request.surface(), SessionListSurface::WorkspacePicker);
    assert_eq!(
        request.scope(),
        &SessionListScope::AllWorkspaces,
        "the Workspaces the reader is not in are the point of the picker"
    );
    request
}

fn press(application: &mut Application, code: KeyCode) {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
        .expect("drive the Workspace Picker");
}

/// The picker's own rows, taken from the frame between the box's title and its
/// footer and trimmed of the box that draws them.
fn picker_rows(application: &Application) -> Vec<String> {
    picker_rows_at(application, 80, 15)
}

fn picker_rows_at(application: &Application, width: u16, height: u16) -> Vec<String> {
    let rows = rendered_application_rows_at(application, width, height);
    let title = rendered_row(&rows, " Workspaces ");
    let footer = rendered_row(&rows, "Esc close");
    rows[title + 1..footer]
        .iter()
        .map(|row| row.trim_matches(['│', ' ']).to_owned())
        .filter(|row| !row.is_empty())
        .collect()
}

/// The name of the Workspace the reader is on, read off the marker the frame
/// draws in front of it.
fn selected_row(application: &Application) -> String {
    let rows = picker_rows(application);
    let selected = rows
        .iter()
        .find(|row| row.starts_with('›'))
        .unwrap_or_else(|| panic!("no row is marked as the reader's: {rows:?}"));
    selected
        .trim_start_matches(['›', ' '])
        .split(' ')
        .next()
        .expect("a marked row names a Workspace")
        .to_owned()
}
