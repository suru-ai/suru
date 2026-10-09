//! The Directory Browser: opening it by `/browse` or its chord at the
//! Landing's Execution Directory, walking and opening its tree, the listings
//! the Outlook's Server answers it with, and closing it.
//!
//! Every directory here is one the Client's own disk does not hold, so a row
//! the tree draws can only have come from the Server's answer.

use std::path::{Path, PathBuf};

use crate::support::{
    application_looking_at_studio, connected_application, key, rendered_application_rows_at,
    rendered_row, studio_stops_answering, type_terminal_text,
};
use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use std::time::Duration;
use suru::{
    protocol::{ChildDirectory, DirectoryListing, ListDirectoryRequest, Outlook},
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, DirectoryListingId,
        SemanticCommandId,
    },
};

const WIDTH: u16 = 100;
const HEIGHT: u16 = 30;

#[test]
fn the_browse_command_opens_the_browser_rooted_at_the_execution_directory() {
    let here = directory(&["nowhere", "here"]);
    let mut application = connected_application(&here);
    type_terminal_text(&mut application, "/browse");

    let (outlook, _, request) = expect_listing(key(&mut application, KeyCode::Enter));

    assert_eq!(
        outlook,
        Outlook::Local,
        "the Outlook's Server is asked for its directories"
    );
    assert_eq!(
        request,
        ListDirectoryRequest {
            path: here.clone(),
            base: Some(here.clone()),
        },
        "the root is the Landing's Execution Directory, and a path is read from it"
    );
    assert_eq!(
        path_field(&application),
        here.to_string_lossy(),
        "the path field names the root"
    );
    assert_eq!(
        tree(&application),
        ["› ▾ here", "    Loading…"],
        "the root is the first row, focused, and says it is still being read"
    );
}

#[test]
fn the_leader_chord_opens_the_browser() {
    let here = directory(&["nowhere", "here"]);
    let mut application = connected_application(&here);

    let (_, _, request) = browse_by_chord(&mut application);

    assert_eq!(request.path, here);
    assert_eq!(tree(&application), ["› ▾ here", "    Loading…"]);
}

#[test]
fn the_root_s_children_are_listed_beneath_it_as_the_server_answers() {
    let here = directory(&["nowhere", "here"]);
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);

    answer(&mut application, listing_id, &here, &["alpha", "beta"]);

    assert_eq!(
        tree(&application),
        ["› ▾ here", "    ▸ alpha", "    ▸ beta"],
        "only what the Server listed stands beneath the root, indented"
    );
}

#[test]
fn a_directory_s_children_are_asked_for_only_when_it_is_opened() {
    let here = directory(&["nowhere", "here"]);
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha", "beta"]);

    assert_eq!(
        key(&mut application, KeyCode::Down),
        ApplicationTransition::Continue,
        "walking onto a row asks nothing of the Server"
    );
    let (outlook, listing_id, request) = expect_listing(key(&mut application, KeyCode::Char(' ')));

    assert_eq!(outlook, Outlook::Local);
    assert_eq!(
        request,
        ListDirectoryRequest {
            path: here.join("alpha"),
            base: Some(here.clone()),
        },
        "opening a row asks for that directory, read from the Execution Directory"
    );
    assert_eq!(
        tree(&application),
        ["  ▾ here", "›   ▾ alpha", "      Loading…", "    ▸ beta"],
        "the row opened says it is being read until the Server answers"
    );

    answer(
        &mut application,
        listing_id,
        &here.join("alpha"),
        &["inner"],
    );
    assert_eq!(
        tree(&application),
        ["  ▾ here", "›   ▾ alpha", "      ▸ inner", "    ▸ beta"]
    );
}

#[test]
fn right_opens_the_focused_row_and_left_closes_it() {
    let here = directory(&["nowhere", "here"]);
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha", "beta"]);
    key(&mut application, KeyCode::Down);

    let (_, listing_id, request) = expect_listing(key(&mut application, KeyCode::Right));
    assert_eq!(request.path, here.join("alpha"));
    answer(
        &mut application,
        listing_id,
        &here.join("alpha"),
        &["inner"],
    );
    assert_eq!(
        key(&mut application, KeyCode::Right),
        ApplicationTransition::Continue,
        "Right on a row already open leaves it open"
    );
    assert_eq!(
        tree(&application),
        ["  ▾ here", "›   ▾ alpha", "      ▸ inner", "    ▸ beta"]
    );

    assert_eq!(
        key(&mut application, KeyCode::Left),
        ApplicationTransition::Continue
    );
    assert_eq!(
        tree(&application),
        ["  ▾ here", "›   ▸ alpha", "    ▸ beta"],
        "Left closes the row it is on and keeps focus there"
    );

    assert_eq!(
        key(&mut application, KeyCode::Right),
        ApplicationTransition::Continue,
        "a directory already listed is not asked for again"
    );
    assert_eq!(
        tree(&application),
        ["  ▾ here", "›   ▾ alpha", "      ▸ inner", "    ▸ beta"]
    );
}

#[test]
fn space_opens_and_closes_the_focused_row() {
    let here = directory(&["nowhere", "here"]);
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha"]);

    assert_eq!(
        key(&mut application, KeyCode::Char(' ')),
        ApplicationTransition::Continue
    );
    assert_eq!(
        tree(&application),
        ["› ▸ here"],
        "Space closes the open row it is on, the root included"
    );

    assert_eq!(
        key(&mut application, KeyCode::Char(' ')),
        ApplicationTransition::Continue,
        "and opens it again without asking for what is already listed"
    );
    assert_eq!(tree(&application), ["› ▾ here", "    ▸ alpha"]);
}

#[test]
fn left_on_the_root_moves_nothing() {
    let here = directory(&["nowhere", "here"]);
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha"]);

    assert_eq!(
        key(&mut application, KeyCode::Left),
        ApplicationTransition::Continue
    );
    assert_eq!(tree(&application), ["› ▾ here", "    ▸ alpha"]);
    assert_eq!(path_field(&application), here.to_string_lossy());
}

#[test]
fn up_and_down_walk_the_drawn_directories_and_wrap_past_the_ends() {
    let here = directory(&["nowhere", "here"]);
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha", "beta"]);
    key(&mut application, KeyCode::Down);
    expect_listing(key(&mut application, KeyCode::Char(' ')));

    assert_eq!(focused(&application), "alpha");
    key(&mut application, KeyCode::Down);
    assert_eq!(
        focused(&application),
        "beta",
        "the line saying alpha is still being read is not a row to stand on"
    );
    key(&mut application, KeyCode::Down);
    assert_eq!(
        focused(&application),
        "here",
        "walking past the last row comes back to the root"
    );
    key(&mut application, KeyCode::Up);
    assert_eq!(
        focused(&application),
        "beta",
        "and walking back past the root comes to the last row"
    );
    chord(&mut application, KeyCode::Char('p'), KeyModifiers::CONTROL);
    assert_eq!(focused(&application), "alpha", "Ctrl+P walks up as Up does");
    chord(&mut application, KeyCode::Char('n'), KeyModifiers::CONTROL);
    assert_eq!(
        focused(&application),
        "beta",
        "Ctrl+N walks down as Down does"
    );
}

#[test]
fn a_directory_the_server_refuses_keeps_its_row_and_says_why_beneath_it() {
    let here = directory(&["nowhere", "here"]);
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha", "beta"]);
    key(&mut application, KeyCode::Down);
    let (_, listing_id, _) = expect_listing(key(&mut application, KeyCode::Char(' ')));

    refuse(&mut application, listing_id, "Permission denied");

    assert_eq!(
        tree(&application),
        [
            "  ▾ here",
            "›   ▾ alpha",
            "      Error: Permission denied",
            "    ▸ beta"
        ]
    );
    key(&mut application, KeyCode::Down);
    assert_eq!(
        focused(&application),
        "beta",
        "the reason is not a row to stand on"
    );
}

#[test]
fn a_root_the_server_refuses_says_why_in_the_path_field_s_place() {
    let here = directory(&["nowhere", "here"]);
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);

    refuse(&mut application, listing_id, "No directory there");

    let screen = rendered_application_rows_at(&application, WIDTH, HEIGHT);
    let field = rendered_row(&screen, "Path:");
    assert!(
        screen[field + 1].contains("Error: No directory there"),
        "the refusal stands beneath the path field naming the root: {screen:#?}"
    );
    assert_eq!(
        tree(&application),
        ["› ▾ here"],
        "the root keeps its row, with nothing said beneath it a second time"
    );
}

#[test]
fn the_footer_names_the_keys_the_browser_answers() {
    let here = directory(&["nowhere", "here"]);
    let mut application = connected_application(&here);
    browse(&mut application);

    let screen = rendered_application_rows_at(&application, WIDTH, HEIGHT);
    let footer = &screen[rendered_row(&screen, "Esc close")];
    for named in ["↑↓", "Space", "→", "←", "Esc"] {
        assert!(footer.contains(named), "{named} is named: {footer}");
    }
    assert!(
        !footer.contains("Enter"),
        "Enter chooses nothing yet, so the footer does not offer it: {footer}"
    );
}

#[test]
fn escape_closes_the_browser_leaving_the_landing_as_it_was() {
    let here = directory(&["nowhere", "here"]);
    let mut application = connected_application(&here);
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "an unsent draft".to_owned(),
        )))
        .expect("type a Landing draft");
    // Typed, `/browse` would only add to the draft, so the chord opens it.
    let (_, listing_id, _) = browse_by_chord(&mut application);
    answer(&mut application, listing_id, &here, &["alpha"]);

    assert_eq!(
        key(&mut application, KeyCode::Esc),
        ApplicationTransition::Continue,
        "backing out of the browser asks the Server for nothing"
    );

    let landing = rendered_application_rows_at(&application, WIDTH, HEIGHT).join("\n");
    assert!(!landing.contains("Path:"), "the browser is gone: {landing}");
    assert!(!landing.contains("alpha"), "{landing}");
    assert!(landing.contains("an unsent draft"), "{landing}");
}

#[test]
fn an_unreachable_remote_refuses_to_open_the_browser() {
    let mut application = application_looking_at_studio();
    studio_stops_answering(&mut application, 1, Duration::from_secs(5));

    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                SemanticCommandId::WorkspaceBrowse,
            )))
            .expect("refuse the browser bound for the Remote"),
        ApplicationTransition::Continue,
        "nothing is asked of a Remote that is not answering"
    );

    let screen = rendered_application_rows_at(&application, WIDTH, HEIGHT).join("\n");
    assert!(
        screen.contains("studio is unreachable"),
        "the refusal is said where any other start's is: {screen}"
    );
    assert!(!screen.contains("Path:"), "no browser opens: {screen}");
}

#[test]
fn the_browser_lists_the_directories_of_the_server_the_outlook_is_turned_toward() {
    let mut application = application_looking_at_studio();

    let (outlook, _, _) = expect_listing(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                SemanticCommandId::WorkspaceBrowse,
            )))
            .expect("open the browser toward the Remote"),
    );

    assert_eq!(
        outlook,
        Outlook::Remote("studio".to_owned()),
        "a Remote's directories are read on the Remote, never on this Client"
    );
}

#[test]
fn an_answer_the_browser_no_longer_awaits_moves_nothing() {
    let here = directory(&["nowhere", "here"]);
    let mut application = connected_application(&here);
    let (_, abandoned, _) = browse(&mut application);
    key(&mut application, KeyCode::Esc);

    answer(&mut application, abandoned, &here, &["stale"]);
    let closed = rendered_application_rows_at(&application, WIDTH, HEIGHT).join("\n");
    assert!(
        !closed.contains("Path:") && !closed.contains("stale"),
        "an answer for a browser since closed opens nothing: {closed}"
    );

    let (_, awaited, _) = browse(&mut application);
    assert_ne!(
        awaited, abandoned,
        "each listing request has its own identity"
    );
    answer(&mut application, abandoned, &here, &["stale"]);
    assert_eq!(
        tree(&application),
        ["› ▾ here", "    Loading…"],
        "an answer to an earlier opening's request is not this one's"
    );

    answer(&mut application, awaited, &here, &["fresh"]);
    assert_eq!(tree(&application), ["› ▾ here", "    ▸ fresh"]);
}

#[test]
fn every_opening_begins_afresh_at_the_root() {
    let here = directory(&["nowhere", "here"]);
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha", "beta"]);
    key(&mut application, KeyCode::Down);
    let (_, listing_id, _) = expect_listing(key(&mut application, KeyCode::Char(' ')));
    answer(
        &mut application,
        listing_id,
        &here.join("alpha"),
        &["inner"],
    );
    key(&mut application, KeyCode::Esc);

    let (_, listing_id, request) = browse(&mut application);

    assert_eq!(
        request.path, here,
        "the root's children are asked for again rather than remembered"
    );
    assert_eq!(
        tree(&application),
        ["› ▾ here", "    Loading…"],
        "focus is back on the root and nothing opened before stands open"
    );
    answer(&mut application, listing_id, &here, &["alpha", "beta"]);
    assert_eq!(
        tree(&application),
        ["› ▾ here", "    ▸ alpha", "    ▸ beta"]
    );
}

/// A directory rooted per platform, which no Client running these tests holds
/// on its own disk.
fn directory(components: &[&str]) -> PathBuf {
    components.iter().fold(
        Path::new(if cfg!(windows) { r"C:\" } else { "/" }).to_owned(),
        |path, component| path.join(component),
    )
}

fn chord(
    application: &mut Application,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(code, modifiers)))
        .expect("deliver a key press")
}

/// Opens the browser the way a reader does, by `/browse`, answering the
/// listing it asked for.
fn browse(application: &mut Application) -> (Outlook, DirectoryListingId, ListDirectoryRequest) {
    type_terminal_text(application, "/browse");
    expect_listing(key(application, KeyCode::Enter))
}

/// Opens the browser by Ctrl+X B, answering the listing it asked for.
fn browse_by_chord(
    application: &mut Application,
) -> (Outlook, DirectoryListingId, ListDirectoryRequest) {
    chord(application, KeyCode::Char('x'), KeyModifiers::CONTROL);
    expect_listing(chord(application, KeyCode::Char('b'), KeyModifiers::NONE))
}

fn expect_listing(
    transition: ApplicationTransition,
) -> (Outlook, DirectoryListingId, ListDirectoryRequest) {
    let ApplicationTransition::ListDirectory {
        outlook,
        listing_id,
        request,
    } = transition
    else {
        panic!("the browser asks the Server for a directory's children, not {transition:?}");
    };
    (outlook, listing_id, request)
}

/// The Server's listing of `root` with child directories named `children`,
/// each spelled beneath it the way the Server spells it.
fn answer(
    application: &mut Application,
    listing_id: DirectoryListingId,
    root: &Path,
    children: &[&str],
) {
    application
        .handle_event(ApplicationEvent::DirectoryListed {
            listing_id,
            result: Ok(DirectoryListing {
                root: root.to_owned(),
                parent: root.parent().map(Path::to_owned),
                children: children
                    .iter()
                    .map(|name| ChildDirectory {
                        name: (*name).to_owned(),
                        path: root.join(name),
                    })
                    .collect(),
            }),
        })
        .expect("deliver the Server's listing");
}

fn refuse(application: &mut Application, listing_id: DirectoryListingId, reason: &str) {
    application
        .handle_event(ApplicationEvent::DirectoryListed {
            listing_id,
            result: Err(reason.to_owned()),
        })
        .expect("deliver the Server's refusal");
}

/// What the path field says, read off its line.
fn path_field(application: &Application) -> String {
    let screen = rendered_application_rows_at(application, WIDTH, HEIGHT);
    inside_box(&screen[rendered_row(&screen, "Path:")])
        .trim_start()
        .trim_start_matches("Path:")
        .trim()
        .to_owned()
}

/// The tree's rows as drawn, between the path field — and any refusal of the
/// root beneath it — and the footer, kept indented as the frame indents them.
fn tree(application: &Application) -> Vec<String> {
    let screen = rendered_application_rows_at(application, WIDTH, HEIGHT);
    let field = rendered_row(&screen, "Path:");
    let footer = rendered_row(&screen, "Esc close");
    screen[field + 1..footer]
        .iter()
        .map(|row| inside_box(row).trim_end().to_owned())
        // A refusal of the root stands flush beneath the path field, where a
        // refusal of any other directory is indented beneath its row.
        .skip_while(|row| row.starts_with("Error:"))
        .filter(|row| !row.is_empty())
        .collect()
}

/// The name on the row focus stands on.
fn focused(application: &Application) -> String {
    let rows = tree(application);
    let row = rows
        .iter()
        .find(|row| row.starts_with('›'))
        .unwrap_or_else(|| panic!("no row is focused: {rows:?}"));
    row.trim_start_matches(['›', ' ', '▸', '▾']).to_owned()
}

/// A frame row's content between the overlay box's borders.
fn inside_box(row: &str) -> &str {
    let start = row.find('│').map_or(0, |index| index + '│'.len_utf8());
    let end = row
        .rfind('│')
        .filter(|end| *end >= start)
        .unwrap_or(row.len());
    &row[start..end]
}
