//! The Directory Browser: opening it by `/browse` or its chord at the
//! Landing's Execution Directory, walking and opening its tree, the listings
//! the Outlook's Server answers it with, the path field that re-roots and
//! narrows the tree as it is typed, choosing a directory to land in, and
//! closing it.
//!
//! Every directory here is one the Client's own disk does not hold, so a row
//! the tree draws can only have come from the Server's answer. Each is rooted
//! per platform and spelled with the platform's separator, which is the local
//! Server's own.

use std::path::{MAIN_SEPARATOR as SEPARATOR, MAIN_SEPARATOR_STR, Path, PathBuf};

use crate::support::{
    SIDEBAR_WIDE, application_looking_at_studio, connected_application, deliver_settings,
    drawn_in_sidebar, enter_active_session, fixture_instance_id, invoke, key, listed_session,
    model_descriptor, ready_health, rendered_application_rows_at, rendered_row,
    selected_session_snapshot, selector_label, sidebar_column, studio_stops_answering,
    type_terminal_text, workspace_resolution,
};
use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use std::time::Duration;
use suru::{
    managed_client::ManagedEvent,
    protocol::{
        AgentSelection, CheckoutAssociation, CheckoutId, CheckoutKind, CheckoutRevision,
        CheckoutSummary, ChildDirectory, DirectoryListing, DirectorySourceControl,
        EffectiveSettings, ExecutionDirectory, ExecutionDirectoryStatus, ListDirectoryRequest,
        ModelAvailability, ModelCatalog, ModelId, ModelOptionChoice, ModelOptionChoiceId,
        ModelOptionDescriptor, ModelOptionId, ModelOptionKind, ModelOptionRole,
        ModelOptionSelection, ModelOptionValue, Outlook, ProviderCatalogStatus, ProviderId,
        ProviderModelCatalog, Repository, RepositoryId, RepositoryLocation,
        ResolveWorkspaceRequest, ResolvedWorkspace, SessionId, SessionListItem, SessionStatus,
        SessionTimestamp, SidebarScope, SidebarSettings, SidebarVisibility,
        SourceControlAvailability, SourceControlCapabilities, Workspace,
    },
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, DirectoryListingId,
        SemanticCommandId, TerminalFacts, WorkspaceResolutionSurface,
    },
};

const WIDTH: u16 = 100;
const HEIGHT: u16 = 30;

/// The Landing here works in a subdirectory of its Workspace, so a browser
/// rooted at the Workspace's root rather than where the Landing works would
/// be told apart.
#[test]
fn the_browse_command_opens_the_browser_rooted_at_the_execution_directory() {
    let workspace = directory(&["nowhere", "repo"]);
    let here = workspace.join("here");
    let mut application = landing_in_subdirectory(&workspace, &here);
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
        spelled(&here),
        "the path field names the root, ready for a name beneath it"
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
    assert_eq!(tree(&application), ["› ▾ here · [current]", "    Loading…"]);
}

#[test]
fn the_root_s_children_are_listed_beneath_it_as_the_server_answers() {
    let here = directory(&["nowhere", "here"]);
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);

    answer(&mut application, listing_id, &here, &["alpha", "beta"]);

    assert_eq!(
        tree(&application),
        ["› ▾ here · [current]", "    ▸ alpha", "    ▸ beta"],
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
        [
            "  ▾ here · [current]",
            "›   ▾ alpha",
            "      Loading…",
            "    ▸ beta"
        ],
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
        [
            "  ▾ here · [current]",
            "›   ▾ alpha",
            "      ▸ inner",
            "    ▸ beta"
        ]
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
        [
            "  ▾ here · [current]",
            "›   ▾ alpha",
            "      ▸ inner",
            "    ▸ beta"
        ]
    );

    assert_eq!(
        key(&mut application, KeyCode::Left),
        ApplicationTransition::Continue
    );
    assert_eq!(
        tree(&application),
        ["  ▾ here · [current]", "›   ▸ alpha", "    ▸ beta"],
        "Left closes the row it is on and keeps focus there"
    );

    assert_eq!(
        key(&mut application, KeyCode::Right),
        ApplicationTransition::Continue,
        "a directory already listed is not asked for again"
    );
    assert_eq!(
        tree(&application),
        [
            "  ▾ here · [current]",
            "›   ▾ alpha",
            "      ▸ inner",
            "    ▸ beta"
        ]
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
        ["› ▸ here · [current]"],
        "Space closes the open row it is on, the root included"
    );

    assert_eq!(
        key(&mut application, KeyCode::Char(' ')),
        ApplicationTransition::Continue,
        "and opens it again without asking for what is already listed"
    );
    assert_eq!(tree(&application), ["› ▾ here · [current]", "    ▸ alpha"]);
}

/// Left is "up" where Right is "into": on the root it stands the tree on the
/// root's parent, as the Server named it, with the former root left open and
/// focused so walking up never loses the reader's place.
#[test]
fn left_on_the_root_re_roots_the_tree_at_its_parent_with_the_former_root_open_and_focused() {
    let parent = directory(&["nowhere"]);
    let here = parent.join("here");
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha"]);

    let (_, listing_id, request) = expect_listing(key(&mut application, KeyCode::Left));

    assert_eq!(
        request,
        ListDirectoryRequest {
            path: parent.clone(),
            base: Some(here.clone()),
        },
        "the parent's children are asked for by the Server's own path for it"
    );
    assert_eq!(
        path_field(&application),
        spelled(&parent),
        "the path field names the new root"
    );
    assert_eq!(
        tree(&application),
        ["  ▾ nowhere", "    Loading…"],
        "the parent is the root, still being read"
    );

    answer(&mut application, listing_id, &parent, &["here", "there"]);
    assert_eq!(
        tree(&application),
        [
            "  ▾ nowhere",
            "›   ▾ here · [current]",
            "      ▸ alpha",
            "    ▸ there"
        ],
        "the former root stands open and focused beneath its parent"
    );
}

/// The filesystem's root, or a drive's, has no parent to walk up to.
#[test]
fn left_on_a_root_without_a_parent_moves_nothing() {
    let top = directory(&[]);
    let mut application = connected_application(&top);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &top, &["alpha"]);
    let field = path_field(&application);
    let rows = tree(&application);

    assert_eq!(
        key(&mut application, KeyCode::Left),
        ApplicationTransition::Continue,
        "nothing is asked of the Server"
    );
    assert_eq!(path_field(&application), field);
    assert_eq!(tree(&application), rows);
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

/// Two directories linking to one elsewhere both list its children, which
/// the Server spells beneath where the link leads, so two open branches draw
/// rows at one path. Each is a row of its own: focus stands on one at a time
/// and walks on past both, and opening one opens that one alone.
#[test]
fn a_directory_two_open_branches_both_list_is_a_row_beneath_each() {
    let here = directory(&["nowhere", "here"]);
    let shared = directory(&["nowhere", "shared"]);
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(
        &mut application,
        listing_id,
        &here,
        &["alpha", "beta", "gamma"],
    );
    key(&mut application, KeyCode::Down);
    let (_, listing_id, _) = expect_listing(key(&mut application, KeyCode::Char(' ')));
    answer(&mut application, listing_id, &shared, &["inner"]);
    key(&mut application, KeyCode::Down);
    key(&mut application, KeyCode::Down);
    let (_, listing_id, _) = expect_listing(key(&mut application, KeyCode::Char(' ')));
    answer(&mut application, listing_id, &shared, &["inner"]);
    assert_eq!(
        tree(&application),
        [
            "  ▾ here · [current]",
            "    ▾ alpha",
            "      ▸ inner",
            "›   ▾ beta",
            "      ▸ inner",
            "    ▸ gamma"
        ]
    );

    key(&mut application, KeyCode::Down);
    let rows = tree(&application);
    assert_eq!(
        rows.iter().filter(|row| row.starts_with('›')).count(),
        1,
        "focus stands on one row however many share its path: {rows:?}"
    );
    assert_eq!(
        rows,
        [
            "  ▾ here · [current]",
            "    ▾ alpha",
            "      ▸ inner",
            "    ▾ beta",
            "›     ▸ inner",
            "    ▸ gamma"
        ]
    );
    key(&mut application, KeyCode::Down);
    assert_eq!(
        focused(&application),
        "gamma",
        "Down walks on past the second row at that path"
    );

    key(&mut application, KeyCode::Up);
    let (_, listing_id, request) = expect_listing(key(&mut application, KeyCode::Char(' ')));
    assert_eq!(
        request.path,
        shared.join("inner"),
        "the Server is asked for the directory by its own path"
    );
    answer(
        &mut application,
        listing_id,
        &shared.join("inner"),
        &["deep"],
    );
    assert_eq!(
        tree(&application),
        [
            "  ▾ here · [current]",
            "    ▾ alpha",
            "      ▸ inner",
            "    ▾ beta",
            "›     ▾ inner",
            "        ▸ deep",
            "    ▸ gamma"
        ],
        "only the row opened is open"
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
            "  ▾ here · [current]",
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
        ["› ▾ here · [current]"],
        "the root keeps its row, with nothing said beneath it a second time"
    );
}

#[test]
fn the_footer_names_the_keys_the_browser_answers() {
    let here = directory(&["nowhere", "here"]);
    let mut application = connected_application(&here);
    browse(&mut application);

    let screen = rendered_application_rows_at(&application, WIDTH, HEIGHT);
    let footer = rendered_row(&screen, "Esc close");
    for named in ["↑↓", "Space", "→", "← close/up", "Enter choose", "Esc"] {
        assert!(
            screen[footer].contains(named),
            "{named} is named: {}",
            screen[footer]
        );
    }
    let path_keys = &screen[footer - 1];
    for named in ["Type a path", "Backspace", "Tab complete"] {
        assert!(
            path_keys.contains(named),
            "the path field's keys are named above the tree's: {path_keys}"
        );
    }
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
        ["› ▾ here · [current]", "    Loading…"],
        "an answer to an earlier opening's request is not this one's"
    );

    answer(&mut application, awaited, &here, &["fresh"]);
    assert_eq!(tree(&application), ["› ▾ here · [current]", "    ▸ fresh"]);
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
        ["› ▾ here · [current]", "    Loading…"],
        "focus is back on the root and nothing opened before stands open"
    );
    answer(&mut application, listing_id, &here, &["alpha", "beta"]);
    assert_eq!(
        tree(&application),
        ["› ▾ here · [current]", "    ▸ alpha", "    ▸ beta"]
    );
}

/// Focus begins on the root, so Enter straight away chooses the directory
/// the reader already stands in.
#[test]
fn enter_on_the_root_chooses_the_directory_the_tree_stands_on() {
    let here = directory(&["nowhere", "here"]);
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha"]);

    let (_, _, request) = expect_choice(&key(&mut application, KeyCode::Enter));

    assert_eq!(request.path, here);
}

/// Choosing acts as choosing in the Workspace Picker does: the browser is
/// done with, the Landing opens at once naming the directory chosen while its
/// Server works out where that is, and the answer lands the Landing in the
/// directory's Workspace with the directory as its Execution Directory.
#[test]
fn enter_chooses_the_focused_directory_and_lands_there_once_the_server_answers() {
    let here = directory(&["nowhere", "here"]);
    let alpha = here.join("alpha");
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha", "beta"]);
    key(&mut application, KeyCode::Down);

    let choice = key(&mut application, KeyCode::Enter);

    let (outlook, surface, request) = expect_choice(&choice);
    assert_eq!(
        outlook,
        Outlook::Local,
        "the Outlook's Server is asked where the directory chosen stands"
    );
    assert_eq!(
        surface,
        WorkspaceResolutionSurface::WorkspacePicker,
        "and its answer is taken as the Workspace Picker's choice is"
    );
    assert_eq!(
        request,
        ResolveWorkspaceRequest {
            checkout_id: None,
            remembered_execution_directory: None,
            workspace_id: None,
            base: Some(here.clone()),
            path: alpha.clone(),
        },
        "the directory chosen goes by the Server's own path, read from the Landing's \
         Execution Directory"
    );
    let landing = rendered_application_rows_at(&application, WIDTH, HEIGHT).join("\n");
    assert!(
        !landing.contains("Path:"),
        "the browser is done with: {landing}"
    );
    assert!(
        landing.contains("Type a prompt"),
        "the Landing stands in its place: {landing}"
    );
    assert!(
        landing_location(&application).ends_with(&format!("alpha · {SPINNER}")),
        "naming the directory chosen while its Server answers: {landing}"
    );

    resolve(
        &mut application,
        &choice,
        Ok(ResolvedWorkspace::directory(alpha.clone())),
    );

    let location = landing_location(&application);
    assert!(
        location.ends_with("alpha") && !location.contains(SPINNER),
        "the Landing stands in the directory's Workspace: {location}"
    );
    assert_eq!(
        next_session_directory(&mut application),
        alpha,
        "and the next Session begins in the directory chosen"
    );
}

/// A directory inside a Worktree is where the next Session works, in the
/// Workspace of the Repository the Worktree belongs to: the Landing names
/// that Workspace, the Worktree's Checkout State, and the directory's path
/// within the Worktree.
#[test]
fn choosing_a_subdirectory_inside_a_worktree_makes_it_the_execution_directory() {
    let main = directory(&["nowhere", "repo", "main"]);
    let linked = directory(&["nowhere", "repo", "linked"]);
    let source = linked.join("src");
    let mut application = connected_application(&linked);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &linked, &["src"]);
    key(&mut application, KeyCode::Down);

    let choice = key(&mut application, KeyCode::Enter);
    let (_, _, request) = expect_choice(&choice);
    assert_eq!(request.path, source);

    resolve(
        &mut application,
        &choice,
        Ok(inside_linked_worktree(&main, &linked, &source)),
    );

    let location = landing_location(&application);
    assert!(
        location.ends_with(&format!(
            "{} · feature/browse (worktree) · src",
            main.display()
        )),
        "the Landing works in the subdirectory of the Worktree: {location}"
    );
    assert_eq!(next_session_directory(&mut application), source);
}

/// A bare Repository has no working copy for a Session to work in, so its
/// root lands with the Worktree choice still owed, as it does when the
/// Workspace Picker chooses one.
#[test]
fn choosing_a_bare_repository_root_lands_with_the_worktree_choice_still_owed() {
    let here = directory(&["nowhere", "here"]);
    let store = here.join("store.git");
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["store.git"]);
    key(&mut application, KeyCode::Down);

    let choice = key(&mut application, KeyCode::Enter);
    let (_, _, request) = expect_choice(&choice);
    assert_eq!(request.path, store);

    resolve(&mut application, &choice, Ok(bare_repository(&store)));

    let location = landing_location(&application);
    assert!(
        location.ends_with("store.git · Choose a working copy to start a Session"),
        "{location}"
    );
    type_terminal_text(&mut application, "Keep this draft");
    assert_eq!(
        key(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue,
        "no Session begins until a working copy is chosen"
    );
    let landing = rendered_application_rows_at(&application, WIDTH, HEIGHT).join("\n");
    assert!(landing.contains("Keep this draft"), "{landing}");
}

/// A refused choice moves nothing: the Landing it opened says why, goes back
/// to naming the Workspace the client still works in, and the next Session
/// begins where it would have before.
#[test]
fn a_refused_choice_is_said_on_the_landing_and_leaves_the_workspace_as_it_was() {
    let here = directory(&["nowhere", "here"]);
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha"]);
    key(&mut application, KeyCode::Down);
    let choice = key(&mut application, KeyCode::Enter);
    type_terminal_text(&mut application, "Keep this draft");

    assert_eq!(
        resolve(
            &mut application,
            &choice,
            Err("Permission denied".to_owned())
        ),
        ApplicationTransition::Continue,
        "a refused choice asks nothing more of the Server"
    );

    let landing = rendered_application_rows_at(&application, WIDTH, HEIGHT).join("\n");
    assert!(
        landing.contains("Could not open the alpha Workspace: Permission denied"),
        "the refusal stands on the Landing the reader is on: {landing}"
    );
    assert!(
        !landing.contains("Path:"),
        "the browser stays closed: {landing}"
    );
    assert!(landing.contains("Keep this draft"), "{landing}");
    assert!(
        landing_location(&application).ends_with("here"),
        "the Landing names the Workspace the client still works in: {landing}"
    );
    assert_eq!(next_session_directory(&mut application), here);
}

/// Going to a fresh Landing lets go of a choice still resolving, as it lets
/// go of the Workspace Picker's, so the answer arriving late leaves the
/// reader where they went.
#[test]
fn a_late_answer_for_a_choice_let_go_of_moves_nothing() {
    let here = directory(&["nowhere", "here"]);
    let alpha = here.join("alpha");
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha"]);
    key(&mut application, KeyCode::Down);
    let choice = key(&mut application, KeyCode::Enter);
    assert_eq!(
        invoke(&mut application, SemanticCommandId::SessionNew),
        ApplicationTransition::DetachSession
    );
    let before = rendered_application_rows_at(&application, WIDTH, HEIGHT);

    assert_eq!(
        resolve(
            &mut application,
            &choice,
            Ok(ResolvedWorkspace::directory(alpha))
        ),
        ApplicationTransition::Continue
    );

    assert_eq!(
        rendered_application_rows_at(&application, WIDTH, HEIGHT),
        before,
        "the fresh Landing stands exactly as it was"
    );
    assert_eq!(next_session_directory(&mut application), here);
}

/// The directory chosen last is the one the reader lands in, whichever
/// answer the Server gives first.
#[test]
fn a_newer_choice_supersedes_a_directory_still_resolving() {
    let here = directory(&["nowhere", "here"]);
    let alpha = here.join("alpha");
    let beta = here.join("beta");
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha", "beta"]);
    key(&mut application, KeyCode::Down);
    let first = key(&mut application, KeyCode::Enter);
    let (_, listing_id, _) = browse_by_chord(&mut application);
    answer(&mut application, listing_id, &here, &["alpha", "beta"]);
    key(&mut application, KeyCode::Down);
    key(&mut application, KeyCode::Down);
    let second = key(&mut application, KeyCode::Enter);

    assert_eq!(
        resolve(
            &mut application,
            &first,
            Ok(ResolvedWorkspace::directory(alpha))
        ),
        ApplicationTransition::Continue
    );
    assert!(
        landing_location(&application).ends_with(&format!("beta · {SPINNER}")),
        "the superseded answer does not pull the Landing back to the earlier choice"
    );

    resolve(
        &mut application,
        &second,
        Ok(ResolvedWorkspace::directory(beta.clone())),
    );
    assert_eq!(next_session_directory(&mut application), beta);
}

/// The Remote the Outlook is turned toward may stop answering while the
/// browser stands open. Choosing is then refused as the Workspace Picker's
/// choice is: nothing is asked of the Remote and the reader is told why.
#[test]
fn choosing_is_refused_while_the_remote_has_stopped_answering() {
    let mut application = application_looking_at_studio();
    let (_, listing_id, request) =
        expect_listing(invoke(&mut application, SemanticCommandId::WorkspaceBrowse));
    answer(&mut application, listing_id, &request.path, &["alpha"]);
    key(&mut application, KeyCode::Down);
    studio_stops_answering(&mut application, 1, Duration::from_secs(5));

    assert_eq!(
        key(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue,
        "nothing is asked of a Remote that is not answering"
    );

    let screen = rendered_application_rows_at(&application, WIDTH, HEIGHT);
    let field = rendered_row(&screen, "Path:");
    assert_eq!(
        inside_box(&screen[field + 1]).trim_end(),
        "Error: studio is unreachable; this waits until it answers",
        "the refusal is said inside the browser, which stands over the Landing: {screen:#?}"
    );
    assert_eq!(
        focused(&application),
        "alpha",
        "the browser stays open where the reader was, to choose again once the Remote answers"
    );

    key(&mut application, KeyCode::Esc);
    let landing = rendered_application_rows_at(&application, WIDTH, HEIGHT).join("\n");
    assert!(
        landing.contains("studio is unreachable; this waits until it answers"),
        "the Landing goes on saying it once the browser closes: {landing}"
    );
}

/// Choosing opens the Landing, which takes the Agent Selection over from
/// the Session being left, so it waits while a change to that selection is
/// still unsettled rather than carry a choice the Server may yet reject onto
/// the Landing. Once the Server has settled it — here by rejecting it — the
/// same Enter chooses, and the Landing begins from the selection that stands.
#[test]
fn choosing_waits_while_an_agent_selection_change_is_unsettled() {
    let here = directory(&["nowhere", "here"]);
    let mut application = Application::new(&here, TerminalFacts::default());
    application
        .handle_event(ApplicationEvent::SessionAttached(
            selected_session_snapshot(SessionId::new(), &here, reasoning_selection("low")),
        ))
        .expect("attach a Session with an Agent Selection");
    warm_model_catalog(&mut application);
    let ApplicationTransition::UpdateAgentSelection {
        request: change, ..
    } = chord(&mut application, KeyCode::Char('t'), KeyModifiers::CONTROL)
    else {
        panic!("cycling reasoning asks the Server to change the Agent Selection");
    };
    assert_eq!(change.selection, reasoning_selection("high"));
    let (_, listing_id, _) = browse_by_chord(&mut application);
    answer(&mut application, listing_id, &here, &["alpha"]);
    key(&mut application, KeyCode::Down);

    assert_eq!(
        key(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue,
        "nothing is chosen while the change is unsettled"
    );
    assert_eq!(
        invoke(&mut application, SemanticCommandId::DirectoryBrowserChoose),
        ApplicationTransition::Continue,
        "however the choice is invoked"
    );
    assert_eq!(
        focused(&application),
        "alpha",
        "the browser stays open where the reader was"
    );

    application
        .handle_event(ApplicationEvent::AgentSelectionUpdateFailed {
            operation_id: change.operation_id,
            error: "Effort rejected".to_owned(),
        })
        .expect("reject the change");
    let choice = key(&mut application, KeyCode::Enter);
    let (_, _, request) = expect_choice(&choice);
    assert_eq!(request.path, here.join("alpha"));
    resolve(
        &mut application,
        &choice,
        Ok(ResolvedWorkspace::directory(here.join("alpha"))),
    );

    let landing = rendered_application_rows_at(&application, WIDTH, HEIGHT).join("\n");
    assert!(
        landing.contains("Reasoning GPT · Low") && !landing.contains("Reasoning GPT · High"),
        "the Landing begins from the selection that stands, not the rejected one: {landing}"
    );
}

/// Choosing from an open Session is plain navigation: the client stops
/// watching the Session, which goes on working and stays listed, and the
/// Sidebar goes on answering for the scope the reader gave it.
#[test]
fn choosing_leaves_the_open_session_working_and_the_sidebar_its_scope() {
    let here = directory(&["nowhere", "here"]);
    let alpha = here.join("alpha");
    let mut application = connected_application(&here);
    let (session_id, ..) = enter_active_session(&mut application, &here);
    show_sidebar_scoped_to_current_workspace(
        &mut application,
        vec![working("Long-running work", session_id, &here)],
    );
    let scope = selector_label(&rendered_application_rows_at(
        &application,
        SIDEBAR_WIDE,
        HEIGHT,
    ));
    let (_, listing_id, _) = browse_by_chord(&mut application);
    answer(&mut application, listing_id, &here, &["alpha"]);
    key(&mut application, KeyCode::Down);

    let choice = key(&mut application, KeyCode::Enter);
    expect_choice(&choice);
    resolve(
        &mut application,
        &choice,
        Ok(ResolvedWorkspace::directory(alpha)),
    );

    let rows = rendered_application_rows_at(&application, SIDEBAR_WIDE, HEIGHT);
    let frame = rows.join("\n");
    assert!(
        frame.contains("Type a prompt"),
        "the Landing stands where the Session was: {frame}"
    );
    assert!(
        drawn_in_sidebar(&rows, "Long-running work"),
        "the Session left is still listed: {frame}"
    );
    assert!(
        rows.iter()
            .any(|row| sidebar_column(row).contains("Working")),
        "and still working: {frame}"
    );
    assert_eq!(
        selector_label(&rows),
        scope,
        "the Sidebar answers for the scope it did: {frame}"
    );
}

/// Glyphs the browser draws while Icons are shown, as the Sidebar and the
/// Workspace Picker draw them.
const FOLDER: char = '\u{ea83}';
const REPOSITORY: char = '\u{ea62}';
const BRANCH: char = '\u{ec6f}';
const WORKTREE: char = '\u{ec7e}';
const COMMIT: char = '\u{eafc}';
/// The `dev-rust` and `dev-python` Icons a Workspace here may wear.
const RUST: char = '\u{e7a8}';
const PYTHON: char = '\u{e73c}';

/// A Repository's main root and a linked Worktree's root show the branch
/// each stands on as a Sidebar row draws it: the main Worktree left implicit
/// and a linked one told apart, by its glyph while Icons are shown and in
/// words while they are not, when the Repository glyph gives way to
/// `[repository]` too.
#[test]
fn a_worktree_root_shows_the_branch_it_stands_on_as_a_sidebar_row_draws_it() {
    let here = directory(&["nowhere", "here"]);
    let children = [
        ("linked", linked_worktree_root(branch("topic"))),
        ("main", repository_root(branch("main"))),
    ];

    assert_eq!(
        tree_drawn(&here, Icons::Hidden, Vec::new(), &children)[1..],
        [
            "    ▸ linked · [repository] · topic (worktree)",
            "    ▸ main · [repository] · main",
        ]
    );
    assert_eq!(
        tree_drawn(&here, Icons::Shown, Vec::new(), &children)[1..],
        [
            format!("    ▸ {REPOSITORY} linked · {WORKTREE} topic"),
            format!("    ▸ {REPOSITORY} main · {BRANCH} main"),
        ]
    );
}

#[test]
fn a_root_at_a_detached_commit_shows_the_commit_as_a_sidebar_row_draws_it() {
    let here = directory(&["nowhere", "here"]);
    let detached = Some(CheckoutRevision::Detached {
        commit: "0123456789abcdef0123456789abcdef01234567".to_owned(),
    });
    let children = [
        ("detached", repository_root(detached.clone())),
        ("linked", linked_worktree_root(detached)),
    ];

    assert_eq!(
        tree_drawn(&here, Icons::Hidden, Vec::new(), &children)[1..],
        [
            "    ▸ detached · [repository] · 0123456",
            "    ▸ linked · [repository] · 0123456",
        ],
        "a detached head names its commit, short, wherever its Worktree is"
    );
    assert_eq!(
        tree_drawn(&here, Icons::Shown, Vec::new(), &children)[1..],
        [
            format!("    ▸ {REPOSITORY} detached · {COMMIT} 0123456"),
            format!("    ▸ {REPOSITORY} linked · {COMMIT} 0123456"),
        ]
    );
}

#[test]
fn a_root_whose_checkout_state_the_server_could_not_read_is_marked_unavailable() {
    let here = directory(&["nowhere", "here"]);
    let children = [
        ("linked", linked_worktree_root(None)),
        ("main", repository_root(None)),
    ];

    assert_eq!(
        tree_drawn(&here, Icons::Hidden, Vec::new(), &children)[1..],
        [
            "    ▸ linked · [repository] · [unavailable]",
            "    ▸ main · [repository] · [unavailable]",
        ]
    );
    assert_eq!(
        tree_drawn(&here, Icons::Shown, Vec::new(), &children)[1..],
        [
            format!("    ▸ {REPOSITORY} linked · [unavailable]"),
            format!("    ▸ {REPOSITORY} main · [unavailable]"),
        ],
        "the marker is words alike either way, as a Sidebar row draws it"
    );
}

#[test]
fn a_bare_repository_is_marked_as_such() {
    let here = directory(&["nowhere", "here"]);
    let children = [
        ("bare.git", DirectorySourceControl::BareRepository),
        ("plain", DirectorySourceControl::Plain),
    ];

    assert_eq!(
        tree_drawn(&here, Icons::Hidden, Vec::new(), &children)[1..],
        ["    ▸ bare.git · [repository] · [bare]", "    ▸ plain"]
    );
    assert_eq!(
        tree_drawn(&here, Icons::Shown, Vec::new(), &children)[1..],
        [
            format!("    ▸ {REPOSITORY} bare.git · [bare]"),
            format!("    ▸ {FOLDER} plain"),
        ]
    );
}

/// A directory that is already one of the Outlook's Workspaces, as the
/// Sidebar's listing knows them, wears that Workspace's Icon, which gives
/// way to `[workspace]` while Icons are hidden; one with no Icon is drawn as
/// any other directory is.
#[test]
fn a_directory_that_is_one_of_the_outlook_s_workspaces_wears_its_icon() {
    let here = directory(&["nowhere", "here"]);
    let sessions = vec![
        session_in(&here.join("iconed"), Some("dev-rust")),
        session_in(&here.join("iconless"), None),
    ];
    let children = [
        ("iconed", DirectorySourceControl::Plain),
        ("iconless", DirectorySourceControl::Plain),
        ("unknown", DirectorySourceControl::Plain),
    ];

    assert_eq!(
        tree_drawn(&here, Icons::Shown, sessions.clone(), &children)[1..],
        [
            format!("    ▸ {RUST} iconed"),
            format!("    ▸ {FOLDER} iconless"),
            format!("    ▸ {FOLDER} unknown"),
        ]
    );
    assert_eq!(
        tree_drawn(&here, Icons::Hidden, sessions, &children)[1..],
        [
            "    ▸ iconed · [workspace]",
            "    ▸ iconless",
            "    ▸ unknown"
        ]
    );
}

/// The Workspace the Landing is in is marked current wherever its directory
/// stands in the tree, as the Workspace Picker marks it, in words whether
/// or not Icons are shown; its Icon stands beside it like any Workspace's.
#[test]
fn the_current_workspace_is_marked_current_as_the_workspace_picker_marks_it() {
    let here = directory(&["nowhere", "here"]);
    let sessions = vec![
        session_in(&here, Some("dev-python")),
        session_in(&here.join("other"), Some("dev-rust")),
    ];
    let children = [
        ("other", repository_root(branch("main"))),
        ("plain", DirectorySourceControl::Plain),
    ];

    assert_eq!(
        tree_drawn(&here, Icons::Hidden, sessions.clone(), &children),
        [
            "› ▾ here · [workspace] · [current]",
            "    ▸ other · [workspace] · main",
            "    ▸ plain",
        ],
        "only the current Workspace is marked current, not every Workspace"
    );
    assert_eq!(
        tree_drawn(&here, Icons::Shown, sessions, &children),
        [
            format!("› ▾ {PYTHON} here · [current]"),
            format!("    ▸ {RUST} other · {BRANCH} main"),
            format!("    ▸ {FOLDER} plain"),
        ]
    );
}

/// A Workspace's Icon stands where a Repository's glyph would, and the
/// Repository glyph where the folder's would; while Icons are hidden each
/// gives way to its word by the same precedence, and the folder's to none.
#[test]
fn a_workspace_icon_stands_before_a_repository_glyph_and_that_before_the_folder() {
    let here = directory(&["nowhere", "here"]);
    let sessions = vec![
        session_in(&here.join("iconed-repository"), Some("dev-rust")),
        session_in(&here.join("iconless-repository"), None),
    ];
    let children = [
        ("iconed-repository", repository_root(branch("main"))),
        ("iconless-repository", repository_root(branch("main"))),
        ("plain", DirectorySourceControl::Plain),
        ("repository", repository_root(branch("main"))),
    ];

    assert_eq!(
        tree_drawn(&here, Icons::Shown, sessions.clone(), &children)[1..],
        [
            format!("    ▸ {RUST} iconed-repository · {BRANCH} main"),
            format!("    ▸ {REPOSITORY} iconless-repository · {BRANCH} main"),
            format!("    ▸ {FOLDER} plain"),
            format!("    ▸ {REPOSITORY} repository · {BRANCH} main"),
        ]
    );
    assert_eq!(
        tree_drawn(&here, Icons::Hidden, sessions, &children)[1..],
        [
            "    ▸ iconed-repository · [workspace] · main",
            "    ▸ iconless-repository · [repository] · main",
            "    ▸ plain",
            "    ▸ repository · [repository] · main",
        ]
    );
}

/// The root is listed as each child is, so a browser opened on a
/// Repository's main root, a linked Worktree's root, or a bare Repository
/// says so of its first row, in the words and glyphs a child's row would.
#[test]
fn the_root_row_shows_what_the_server_read_of_it() {
    let main = directory(&["nowhere", "main"]);
    let linked = directory(&["nowhere", "linked"]);
    let bare = directory(&["nowhere", "bare.git"]);
    // The Landing stands at the main root of its Workspace, which is current.
    let at_main = |read: DirectorySourceControl, icons| {
        tree_drawn_from(
            connected_application(&main),
            &main,
            &read,
            icons,
            Vec::new(),
            &[],
        )[0]
        .clone()
    };

    assert_eq!(
        at_main(repository_root(branch("main")), Icons::Hidden),
        "› ▾ main · [repository] · [current] · main"
    );
    assert_eq!(
        at_main(repository_root(branch("main")), Icons::Shown),
        format!("› ▾ {REPOSITORY} main · [current] · {BRANCH} main")
    );
    assert_eq!(
        at_main(repository_root(None), Icons::Hidden),
        "› ▾ main · [repository] · [current] · [unavailable]"
    );
    assert_eq!(
        at_main(repository_root(None), Icons::Shown),
        format!("› ▾ {REPOSITORY} main · [current] · [unavailable]")
    );

    // The Landing stands in a linked Worktree of the Workspace at `main`.
    let at_linked = |icons| {
        tree_drawn_from(
            landing_in_subdirectory(&main, &linked),
            &linked,
            &linked_worktree_root(branch("topic")),
            icons,
            Vec::new(),
            &[],
        )[0]
        .clone()
    };
    assert_eq!(
        at_linked(Icons::Hidden),
        "› ▾ linked · [repository] · topic (worktree)"
    );
    assert_eq!(
        at_linked(Icons::Shown),
        format!("› ▾ {REPOSITORY} linked · {WORKTREE} topic")
    );

    let at_bare = |icons| {
        tree_drawn_from(
            connected_application(&bare),
            &bare,
            &DirectorySourceControl::BareRepository,
            icons,
            Vec::new(),
            &[],
        )[0]
        .clone()
    };
    assert_eq!(
        at_bare(Icons::Hidden),
        "› ▾ bare.git · [repository] · [current] · [bare]"
    );
    assert_eq!(
        at_bare(Icons::Shown),
        format!("› ▾ {REPOSITORY} bare.git · [current] · [bare]")
    );
}

/// Until the root's listing arrives nothing is known of what it is, so it
/// is drawn as a plain directory and says only that it is being read.
#[test]
fn the_root_row_is_plain_until_its_listing_arrives() {
    let here = directory(&["nowhere", "here"]);
    let mut application = connected_application(&here);
    let mut settings = EffectiveSettings::default();
    settings.appearance.show_icons = true;
    deliver_settings(&mut application, settings);
    browse(&mut application);

    assert_eq!(
        tree(&application),
        [
            format!("› ▾ {FOLDER} here · [current]"),
            "    Loading…".to_owned()
        ]
    );
}

#[derive(Clone, Copy)]
enum Icons {
    Shown,
    Hidden,
}

/// The tree a browser opened at `here`, a plain directory, draws once the
/// Server lists `children`, each with what it is to source control, while
/// the Sidebar's listing holds `sessions` and Icons are shown or hidden.
fn tree_drawn(
    here: &Path,
    icons: Icons,
    sessions: Vec<SessionListItem>,
    children: &[(&str, DirectorySourceControl)],
) -> Vec<String> {
    tree_drawn_from(
        connected_application(here),
        here,
        &DirectorySourceControl::Plain,
        icons,
        sessions,
        children,
    )
}

/// The tree `application` draws once a browser opened at its Execution
/// Directory, `root`, is listed by the Server as `read` with `children`.
fn tree_drawn_from(
    mut application: Application,
    root: &Path,
    read: &DirectorySourceControl,
    icons: Icons,
    sessions: Vec<SessionListItem>,
    children: &[(&str, DirectorySourceControl)],
) -> Vec<String> {
    let mut settings = EffectiveSettings::default();
    settings.appearance.show_icons = matches!(icons, Icons::Shown);
    let ApplicationTransition::ListSessions(request) = deliver_settings(&mut application, settings)
    else {
        panic!("a Sidebar coming into view asks for its Sessions");
    };
    application
        .handle_event(ApplicationEvent::SessionsListed { request, sessions })
        .expect("list the Sidebar's Sessions");
    let (_, listing_id, _) = browse(&mut application);
    answer_with(&mut application, listing_id, root, read, children);
    tree(&application)
}

/// A Session the Sidebar lists in the Workspace at `path`, wearing the Icon
/// the Catalog names `icon`.
fn session_in(path: &Path, icon: Option<&str>) -> SessionListItem {
    let mut session = listed_session(SessionId::new(), "Work", path, 1, 1);
    let SessionListItem::Readable(summary) = &mut session else {
        unreachable!("a listed Session is readable")
    };
    summary.session.workspace.icon = icon.map(str::to_owned);
    session
}

fn branch(name: &str) -> Option<CheckoutRevision> {
    Some(CheckoutRevision::Branch {
        name: name.to_owned(),
        commit: Some("0123456789abcdef0123456789abcdef01234567".to_owned()),
    })
}

fn repository_root(revision: Option<CheckoutRevision>) -> DirectorySourceControl {
    DirectorySourceControl::RepositoryRoot { revision }
}

fn linked_worktree_root(revision: Option<CheckoutRevision>) -> DirectorySourceControl {
    DirectorySourceControl::LinkedWorktreeRoot { revision }
}

/// A Landing in `workspace` whose Execution Directory is `subdirectory`, a
/// directory beneath it, as the Server resolves the launch to.
/// What the reader types goes to the path field, and the partial name after
/// its last separator narrows the root's children to those it begins, case
/// set aside. Nothing is asked of the Server, since the leading part still
/// names the root, and focus moves onto the first child left, which is the
/// one Tab completes to.
#[test]
fn typing_narrows_the_root_s_children_to_those_the_tail_begins() {
    let here = directory(&["nowhere", "here"]);
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(
        &mut application,
        listing_id,
        &here,
        &["alpha", "Beta", "bravo", "charlie"],
    );

    assert!(
        type_path(&mut application, "b").is_empty(),
        "typing within the tail asks the Server for nothing"
    );

    assert_eq!(path_field(&application), format!("{}b", spelled(&here)));
    assert_eq!(
        tree(&application),
        ["  ▾ here · [current]", "›   ▸ Beta", "    ▸ bravo"],
        "the tail narrows the root's children, case set aside"
    );
    type_path(&mut application, "r");
    assert_eq!(tree(&application), ["  ▾ here · [current]", "›   ▸ bravo"]);
}

/// A separator ends the leading part on a directory this opening has not read,
/// so the Server is asked for it as typed, read from the Execution Directory.
/// Until it answers the tree stands where it was and the field says its root
/// is being read; the answer stands the tree there.
#[test]
fn typing_a_separator_re_roots_the_tree_at_the_directory_the_leading_part_names() {
    let here = directory(&["nowhere", "here"]);
    let beta = here.join("beta");
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha", "beta"]);

    let asked = type_path(&mut application, &format!("beta{SEPARATOR}"));

    let [(listing_id, request)] = asked.as_slice() else {
        panic!("only the separator, which changes the leading part, asks: {asked:?}");
    };
    assert_eq!(
        *request,
        ListDirectoryRequest {
            path: beta.clone(),
            base: Some(here.clone()),
        },
        "the Server is asked for the leading part as typed, read from the Execution Directory"
    );
    assert_eq!(
        beneath_path_field(&application),
        ["Loading…"],
        "the field says its root is still being read"
    );
    assert_eq!(
        tree(&application),
        ["  ▾ here · [current]", "›   ▸ beta"],
        "the tree stands where it was until the Server answers"
    );

    answer(&mut application, *listing_id, &beta, &["inner"]);

    assert_eq!(
        tree(&application),
        ["› ▾ beta", "    ▸ inner"],
        "the tree stands on the directory the leading part names, focused"
    );
    assert!(beneath_path_field(&application).is_empty());
    assert_eq!(path_field(&application), spelled(&beta));
}

#[test]
fn a_paste_appends_to_the_path_field_whole() {
    let here = directory(&["nowhere", "here"]);
    let beta = here.join("beta");
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha", "beta"]);

    let (_, listing_id, request) =
        expect_listing(paste(&mut application, &format!("beta{SEPARATOR}in")));

    assert_eq!(
        request.path, beta,
        "one request, for the leading part the paste leaves"
    );
    answer(&mut application, listing_id, &beta, &["inner", "outer"]);
    assert_eq!(path_field(&application), format!("{}in", spelled(&beta)));
    assert_eq!(tree(&application), ["  ▾ beta", "›   ▸ inner"]);
}

/// Backspace takes the field's last character back, so taking back the
/// separator walks the root up to its parent, narrowed to the former root's
/// name. Coming back to a leading part this opening has read already stands
/// the tree there at once, asking nothing.
#[test]
fn backspace_deletes_the_last_character_walking_the_root_up_a_directory() {
    let parent = directory(&["nowhere"]);
    let here = parent.join("here");
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha"]);

    let (_, listing_id, request) = expect_listing(key(&mut application, KeyCode::Backspace));

    assert_eq!(path_field(&application), here.display().to_string());
    assert_eq!(
        request,
        ListDirectoryRequest {
            path: parent.clone(),
            base: Some(here.clone()),
        }
    );
    answer(
        &mut application,
        listing_id,
        &parent,
        &["here", "hereafter", "there"],
    );
    assert_eq!(
        tree(&application),
        ["  ▾ nowhere", "›   ▸ here · [current]", "    ▸ hereafter"]
    );

    assert_eq!(
        key(&mut application, KeyCode::Backspace),
        ApplicationTransition::Continue,
        "Backspace within the tail asks nothing"
    );
    assert_eq!(
        path_field(&application),
        format!("{}{SEPARATOR}her", parent.display())
    );

    assert!(
        type_path(&mut application, &format!("e{SEPARATOR}")).is_empty(),
        "the Execution Directory has been read already"
    );
    assert_eq!(tree(&application), ["› ▾ here · [current]", "    ▸ alpha"]);
}

/// Left and Right belong to the tree, and Space opens and closes rather than
/// typing; none of them moves anything in the path field.
#[test]
fn the_tree_s_keys_never_edit_the_path_field() {
    let here = directory(&["nowhere", "here"]);
    let alpha = here.join("alpha");
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha", "beta"]);
    type_path(&mut application, "a");
    let field = path_field(&application);

    let (_, listing_id, _) = expect_listing(key(&mut application, KeyCode::Right));
    answer(&mut application, listing_id, &alpha, &["inner"]);
    assert_eq!(path_field(&application), field, "Right opens the row");
    key(&mut application, KeyCode::Left);
    assert_eq!(path_field(&application), field, "Left closes it");
    key(&mut application, KeyCode::Char(' '));
    assert_eq!(path_field(&application), field, "Space opens it again");
    assert_eq!(
        tree(&application),
        ["  ▾ here · [current]", "›   ▾ alpha", "      ▸ inner"]
    );
}

#[test]
fn the_tail_narrows_only_the_root_s_children() {
    let here = directory(&["nowhere", "here"]);
    let alpha = here.join("alpha");
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha", "beta"]);
    key(&mut application, KeyCode::Down);
    let (_, listing_id, _) = expect_listing(key(&mut application, KeyCode::Right));
    answer(&mut application, listing_id, &alpha, &["deep", "zeta"]);

    type_path(&mut application, "a");

    assert_eq!(
        tree(&application),
        [
            "  ▾ here · [current]",
            "›   ▾ alpha",
            "      ▸ deep",
            "      ▸ zeta"
        ],
        "beta is narrowed away, while alpha keeps every child it has"
    );
}

/// Tab completes the field to the focused row and a separator, which stands
/// the tree on that row, asking for its children by the Server's own path
/// for it.
#[test]
fn tab_completes_the_path_field_to_the_focused_row_and_a_separator() {
    let here = directory(&["nowhere", "here"]);
    let beta = here.join("beta");
    let inner = beta.join("inner");
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha", "beta"]);
    type_path(&mut application, "b");

    let (_, listing_id, request) = expect_listing(key(&mut application, KeyCode::Tab));

    assert_eq!(path_field(&application), spelled(&beta));
    assert_eq!(
        request,
        ListDirectoryRequest {
            path: beta.clone(),
            base: Some(here.clone()),
        }
    );
    assert_eq!(tree(&application), ["› ▾ beta", "    Loading…"]);

    answer(&mut application, listing_id, &beta, &["inner"]);
    key(&mut application, KeyCode::Down);
    let (_, listing_id, _) = expect_listing(key(&mut application, KeyCode::Right));
    answer(&mut application, listing_id, &inner, &["deep"]);
    key(&mut application, KeyCode::Down);

    let (_, _, request) = expect_listing(key(&mut application, KeyCode::Tab));

    assert_eq!(
        path_field(&application),
        spelled(&inner.join("deep")),
        "a row deeper than the root's children completes through each directory above it"
    );
    assert_eq!(request.path, inner.join("deep"));
}

/// A relative path is the Server's to read, from the Landing's Execution
/// Directory: the tree stands on the directory it answers with, while the
/// field keeps what the reader typed.
#[test]
fn a_relative_path_is_read_from_the_execution_directory_and_the_server_s_root_is_shown() {
    let parent = directory(&["nowhere"]);
    let here = parent.join("here");
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha"]);
    clear_path_field(&mut application);

    let asked = type_path(&mut application, &format!("..{SEPARATOR}"));

    let [(listing_id, request)] = asked.as_slice() else {
        panic!("the relative leading part is asked for once: {asked:?}");
    };
    assert_eq!(
        *request,
        ListDirectoryRequest {
            path: PathBuf::from(".."),
            base: Some(here.clone()),
        }
    );
    answer(&mut application, *listing_id, &parent, &["here", "there"]);
    assert_eq!(
        tree(&application),
        ["› ▾ nowhere", "    ▸ here · [current]", "    ▸ there"],
        "the root is the directory the Server read the path as"
    );
    assert_eq!(
        path_field(&application),
        format!("..{SEPARATOR}"),
        "the field keeps what the reader typed"
    );
}

#[test]
fn a_tilde_is_read_from_the_server_s_home() {
    let here = directory(&["nowhere", "here"]);
    let home = directory(&["nowhere", "home"]);
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha"]);
    clear_path_field(&mut application);

    let asked = type_path(&mut application, &format!("~{SEPARATOR}"));

    let [(listing_id, request)] = asked.as_slice() else {
        panic!("the home is asked for once: {asked:?}");
    };
    assert_eq!(
        *request,
        ListDirectoryRequest {
            path: PathBuf::from("~"),
            base: Some(here.clone()),
        },
        "the Server reads `~` itself"
    );
    answer(&mut application, *listing_id, &home, &["projects"]);
    assert_eq!(tree(&application), ["› ▾ home", "    ▸ projects"]);
    assert_eq!(path_field(&application), format!("~{SEPARATOR}"));
}

/// A leading part naming no directory the Server can read leaves the tree
/// on the last root it could, with every child shown, since the tail typed
/// after it says nothing of that root; why stands beneath the field until
/// the leading part changes.
#[test]
fn a_leading_part_the_server_refuses_leaves_the_tree_on_its_last_good_root() {
    let here = directory(&["nowhere", "here"]);
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha", "beta"]);
    let asked = type_path(&mut application, &format!("missing{SEPARATOR}"));
    let [(listing_id, _)] = asked.as_slice() else {
        panic!("the leading part is asked for once: {asked:?}");
    };

    refuse(&mut application, *listing_id, "No directory there");

    assert_eq!(
        beneath_path_field(&application),
        ["Error: No directory there"]
    );
    assert_eq!(
        tree(&application),
        ["› ▾ here · [current]", "    ▸ alpha", "    ▸ beta"]
    );
    assert_eq!(
        path_field(&application),
        format!("{}missing{SEPARATOR}", spelled(&here)),
        "the field keeps what the reader typed"
    );

    key(&mut application, KeyCode::Backspace);
    assert!(
        beneath_path_field(&application).is_empty(),
        "the refusal goes with the leading part it was for"
    );
}

#[test]
fn an_answer_for_a_leading_part_since_typed_over_moves_nothing() {
    let here = directory(&["nowhere", "here"]);
    let alpha = here.join("alpha");
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha", "beta"]);
    let asked = type_path(&mut application, &format!("alpha{SEPARATOR}"));
    let [(superseded, _)] = asked.as_slice() else {
        panic!("the leading part is asked for once: {asked:?}");
    };
    let superseded = *superseded;
    key(&mut application, KeyCode::Backspace);

    answer(&mut application, superseded, &alpha, &["stale"]);
    assert_eq!(
        tree(&application),
        ["  ▾ here · [current]", "›   ▸ alpha"],
        "the leading part asked for is no longer the field's"
    );

    let asked = type_path(&mut application, MAIN_SEPARATOR_STR);
    let [(awaited, _)] = asked.as_slice() else {
        panic!("typing the separator again asks again: {asked:?}");
    };
    assert_ne!(*awaited, superseded);
    answer(&mut application, superseded, &alpha, &["stale"]);
    assert_eq!(
        tree(&application),
        ["  ▾ here · [current]", "›   ▸ alpha"],
        "an answer to the earlier request is not this one's"
    );

    answer(&mut application, *awaited, &alpha, &["fresh"]);
    assert_eq!(tree(&application), ["› ▾ alpha", "    ▸ fresh"]);
}

/// The path field is spelled as the Outlook's Server spells its paths, which
/// for a Remote need not be as this Client spells its own: these are wire
/// paths of both platforms, exercised whichever one the tests run on.
#[test]
fn the_path_field_takes_the_separator_of_the_server_whatever_this_client_runs_on() {
    for (home, separator) in [("/srv/home", '/'), (r"C:\Users\home", '\\')] {
        let home = PathBuf::from(home);
        let project = PathBuf::from(format!("{}{separator}project", home.display()));
        let mut application = application_looking_at_studio();
        expect_listing(invoke(&mut application, SemanticCommandId::WorkspaceBrowse));
        let choice = key(&mut application, KeyCode::Enter);
        resolve(
            &mut application,
            &choice,
            Ok(ResolvedWorkspace::directory(home.clone())),
        );

        let (_, listing_id, request) =
            expect_listing(invoke(&mut application, SemanticCommandId::WorkspaceBrowse));

        assert_eq!(request.path, home);
        assert_eq!(
            path_field(&application),
            format!("{}{separator}", home.display())
        );
        answer_listing(
            &mut application,
            listing_id,
            DirectoryListing {
                root: home.clone(),
                parent: None,
                source_control: DirectorySourceControl::Plain,
                children: vec![ChildDirectory {
                    name: "project".to_owned(),
                    path: project.clone(),
                    source_control: DirectorySourceControl::Plain,
                }],
            },
        );
        assert!(
            type_path(&mut application, "pro").is_empty(),
            "the leading part still names the root"
        );
        assert_eq!(focused(&application), "project");

        let (_, _, request) = expect_listing(key(&mut application, KeyCode::Tab));

        assert_eq!(
            path_field(&application),
            format!("{}{separator}", project.display()),
            "Tab completes with {separator:?}"
        );
        assert_eq!(request.path, project);
    }
}

fn landing_in_subdirectory(workspace: &Path, subdirectory: &Path) -> Application {
    let mut application = Application::new(workspace, TerminalFacts::default());
    let launch = application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
            ready_health(fixture_instance_id(), 42_424),
        )))
        .expect("connect the application");
    let (outlook, surface, request_id, _) =
        workspace_resolution(&launch).expect("connecting resolves where the client launched");
    application
        .handle_event(ApplicationEvent::WorkspaceResolved {
            outlook,
            surface,
            request_id,
            result: Ok(ResolvedWorkspace {
                execution_directory: Some(ExecutionDirectory {
                    path: subdirectory.to_owned(),
                }),
                ..ResolvedWorkspace::directory(workspace.to_owned())
            }),
        })
        .expect("resolve the Landing into a subdirectory of its Workspace");
    application
}

/// The Spinner's first frame, which is what a reading on its way shows.
const SPINNER: char = '⠋';

/// The Workspace resolution choosing asked for: the Outlook asked, the
/// surface its answer is taken by, and the request.
fn expect_choice(
    transition: &ApplicationTransition,
) -> (Outlook, WorkspaceResolutionSurface, ResolveWorkspaceRequest) {
    let ApplicationTransition::DetachSessionAndResolveWorkspace { .. } = transition else {
        panic!(
            "choosing opens the Landing, letting go of whatever the client was on, and asks \
             the Server to resolve the directory chosen, not {transition:?}"
        );
    };
    let (outlook, surface, _, request) =
        workspace_resolution(transition).expect("choosing asks for a Workspace resolution");
    (outlook, surface, request)
}

/// The Server's answer to the resolution `choice` asked for.
fn resolve(
    application: &mut Application,
    choice: &ApplicationTransition,
    result: Result<ResolvedWorkspace, String>,
) -> ApplicationTransition {
    let (outlook, surface, request_id, _) =
        workspace_resolution(choice).expect("choosing asks for a Workspace resolution");
    application
        .handle_event(ApplicationEvent::WorkspaceResolved {
            outlook,
            surface,
            request_id,
            result,
        })
        .expect("deliver the Server's Workspace resolution")
}

/// Where a Prompt sent from the Landing begins its Session.
fn next_session_directory(application: &mut Application) -> PathBuf {
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Begin here".to_owned(),
        )))
        .expect("write a Prompt");
    let transition = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("send the Prompt");
    let ApplicationTransition::CreateSession(request) = transition else {
        panic!("a Prompt sent from the Landing begins a Session, not {transition:?}");
    };
    request.execution_directory.path
}

/// The line beneath the Landing's composer, which names the Workspace the
/// Landing stands in and where in it the next Session works.
fn landing_location(application: &Application) -> String {
    let screen = rendered_application_rows_at(application, WIDTH, HEIGHT);
    let composer_bottom = screen
        .iter()
        .position(|row| row.contains('└'))
        .unwrap_or_else(|| panic!("the Landing draws its composer: {screen:#?}"));
    screen[composer_bottom + 1].trim().to_owned()
}

/// What the Server resolves `directory` to, standing inside the linked
/// Worktree at `linked` of the Repository whose main Worktree is at `main`:
/// that Repository's Workspace, on the linked Worktree's branch.
fn inside_linked_worktree(main: &Path, linked: &Path, directory: &Path) -> ResolvedWorkspace {
    let metadata = main.join(".git");
    let id = RepositoryId::from_metadata("git", &metadata);
    let repository = Repository {
        id: id.clone(),
        system: "git".to_owned(),
        metadata_directory: metadata,
        location: RepositoryLocation::Main {
            root: main.to_owned(),
        },
        availability: SourceControlAvailability::Available,
        capabilities: SourceControlCapabilities::discovery_only(),
    };
    let checkouts = [
        (main, CheckoutKind::Main, "main"),
        (linked, CheckoutKind::Linked, "feature/browse"),
    ]
    .into_iter()
    .map(|(root, kind, branch)| CheckoutSummary {
        association: CheckoutAssociation {
            recovery_revision: None,
            reclaim: None,
            id: CheckoutId::from_root(&id, root),
            repository: id.clone(),
            root: root.to_owned(),
            kind,
        },
        revision: Some(CheckoutRevision::Branch {
            name: branch.to_owned(),
            commit: Some("1234567890abcdef".to_owned()),
        }),
        availability: SourceControlAvailability::Available,
    })
    .collect::<Vec<_>>();
    ResolvedWorkspace {
        execution_status: ExecutionDirectoryStatus::Available,
        workspace: Workspace {
            id: id.workspace_id(),
            path: main.to_owned(),
            repository: Some(Box::new(repository)),
            source_control: SourceControlAvailability::Available,
            icon: None,
            description: None,
        },
        execution_directory: Some(ExecutionDirectory {
            path: directory.to_owned(),
        }),
        checkout: Some(checkouts[1].association.clone()),
        checkouts,
    }
}

/// What the Server resolves a bare Repository's root to: its Workspace, with
/// no working copy yet for a Session to work in.
fn bare_repository(root: &Path) -> ResolvedWorkspace {
    let repository = Repository {
        id: RepositoryId::from_metadata("git", root),
        system: "git".to_owned(),
        metadata_directory: root.to_owned(),
        location: RepositoryLocation::Bare {
            root: root.to_owned(),
        },
        availability: SourceControlAvailability::Available,
        capabilities: SourceControlCapabilities::discovery_only(),
    };
    ResolvedWorkspace {
        execution_status: ExecutionDirectoryStatus::RequiresWorkingCopy,
        workspace: Workspace {
            id: repository.id.workspace_id(),
            path: root.to_owned(),
            repository: Some(Box::new(repository)),
            source_control: SourceControlAvailability::Available,
            icon: None,
            description: None,
        },
        execution_directory: None,
        checkout: None,
        checkouts: Vec::new(),
    }
}

/// The Agent Selection of a Model whose reasoning effort is `effort`.
fn reasoning_selection(effort: &str) -> AgentSelection {
    AgentSelection {
        provider: ProviderId::new("codex"),
        model: ModelId::new("gpt-reasoning"),
        options: vec![ModelOptionSelection {
            id: ModelOptionId::new("reasoning_effort"),
            value: ModelOptionValue::Select {
                choice: ModelOptionChoiceId::new(effort),
            },
        }],
    }
}

/// Holds the Model Catalog the reasoning cycle reads, as the Model picker
/// leaves it once it has been opened and closed: one Model whose reasoning
/// effort is low or high.
fn warm_model_catalog(application: &mut Application) {
    let transition = invoke(application, SemanticCommandId::ModelList);
    let ApplicationTransition::ListModels(request) = transition else {
        panic!("the Model picker asks for the catalog, not {transition:?}");
    };
    let mut model = model_descriptor(
        "codex",
        "gpt-reasoning",
        "Reasoning GPT",
        true,
        ModelAvailability::Available,
    );
    model.options = vec![ModelOptionDescriptor {
        id: ModelOptionId::new("reasoning_effort"),
        label: "Reasoning".to_owned(),
        description: None,
        role: ModelOptionRole::ReasoningEffort,
        kind: ModelOptionKind::Select {
            choices: ["low", "high"]
                .into_iter()
                .map(|effort| ModelOptionChoice {
                    id: ModelOptionChoiceId::new(effort),
                    label: format!("{}{}", effort[..1].to_uppercase(), &effort[1..]),
                    description: None,
                    availability: ModelAvailability::Available,
                })
                .collect(),
            default: ModelOptionChoiceId::new("low"),
        },
    }];
    application
        .handle_event(ApplicationEvent::ModelsListed {
            request,
            catalog: ModelCatalog {
                providers: vec![ProviderModelCatalog {
                    provider: ProviderId::new("codex"),
                    display_name: "codex".to_owned(),
                    models: vec![model],
                    status: ProviderCatalogStatus::Fresh,
                }],
            },
        })
        .expect("hold the Model Catalog");
    key(application, KeyCode::Esc);
}

/// The Sidebar on screen, scoped to the current Workspace and answered with
/// `sessions`.
fn show_sidebar_scoped_to_current_workspace(
    application: &mut Application,
    sessions: Vec<SessionListItem>,
) {
    let transition = deliver_settings(
        application,
        EffectiveSettings {
            sidebar: SidebarSettings {
                initial_visibility: SidebarVisibility::Shown,
                initial_scope: SidebarScope::CurrentWorkspace,
                ..SidebarSettings::default()
            },
            ..EffectiveSettings::default()
        },
    );
    let ApplicationTransition::ListSessions(request) = transition else {
        panic!("a Sidebar coming into view asks for its Sessions, not {transition:?}");
    };
    application
        .handle_event(ApplicationEvent::SessionsListed { request, sessions })
        .expect("hydrate the Sidebar");
}

/// A listed Session mid-Turn, its times read off the clock because a Sidebar
/// shelves a Session by how long ago it last moved.
fn working(title: &str, session_id: SessionId, workspace: &Path) -> SessionListItem {
    let now = SessionTimestamp::now();
    let SessionListItem::Readable(mut summary) =
        listed_session(session_id, title, workspace, now.0, now.0)
    else {
        unreachable!("the fixture builds a readable Session");
    };
    summary.session.status = SessionStatus::Active;
    summary.session.working_since = Some(SessionTimestamp(now.0.saturating_sub(90 * 1_000)));
    SessionListItem::Readable(summary)
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

/// The Server's listing of `root` with plain child directories named
/// `children`, each spelled beneath it the way the Server spells it.
fn answer(
    application: &mut Application,
    listing_id: DirectoryListingId,
    root: &Path,
    children: &[&str],
) {
    let children = children
        .iter()
        .map(|name| (*name, DirectorySourceControl::Plain))
        .collect::<Vec<_>>();
    answer_with(
        application,
        listing_id,
        root,
        &DirectorySourceControl::Plain,
        &children,
    );
}

/// The Server's listing of `root`, read as `read`, with child directories by
/// name, each with what the Server read it to be.
fn answer_with(
    application: &mut Application,
    listing_id: DirectoryListingId,
    root: &Path,
    read: &DirectorySourceControl,
    children: &[(&str, DirectorySourceControl)],
) {
    answer_listing(
        application,
        listing_id,
        DirectoryListing {
            root: root.to_owned(),
            parent: root.parent().map(Path::to_owned),
            source_control: read.clone(),
            children: children
                .iter()
                .map(|(name, source_control)| ChildDirectory {
                    name: (*name).to_owned(),
                    path: root.join(name),
                    source_control: source_control.clone(),
                })
                .collect(),
        },
    );
}

fn answer_listing(
    application: &mut Application,
    listing_id: DirectoryListingId,
    listing: DirectoryListing,
) {
    application
        .handle_event(ApplicationEvent::DirectoryListed {
            listing_id,
            result: Ok(listing),
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

/// The tree's rows as drawn, between the path field — and whatever stands
/// beneath it about its root — and the footer, kept indented as the frame
/// indents them.
fn tree(application: &Application) -> Vec<String> {
    let screen = rendered_application_rows_at(application, WIDTH, HEIGHT);
    let field = rendered_row(&screen, "Path:");
    // The footer names the keys bare where the box is too narrow to say what
    // each does, as it is beside a Sidebar: the tree's keys begin with Up and
    // Down either way, beneath the path field's own where it has a line.
    let keys = rendered_row(&screen, "↑↓");
    let footer = if inside_box(&screen[keys - 1]).starts_with("Type") {
        keys - 1
    } else {
        keys
    };
    screen[field + 1..footer]
        .iter()
        .map(|row| inside_box(row).trim_end().to_owned())
        .skip_while(|row| says_of_the_path_field(row))
        .filter(|row| !row.is_empty())
        .collect()
}

/// What stands beneath the path field about the root it names: that it is
/// still being read, or why it cannot be.
fn beneath_path_field(application: &Application) -> Vec<String> {
    let screen = rendered_application_rows_at(application, WIDTH, HEIGHT);
    let field = rendered_row(&screen, "Path:");
    screen[field + 1..]
        .iter()
        .map(|row| inside_box(row).trim_end().to_owned())
        .take_while(|row| says_of_the_path_field(row))
        .collect()
}

/// Whether a line speaks for the path field's root rather than being a row
/// of the tree: it stands flush beneath the field, where what is said of any
/// other directory is indented beneath that directory's row.
fn says_of_the_path_field(row: &str) -> bool {
    row.starts_with("Error:") || row.starts_with("Loading")
}

/// The path field naming `directory` as the root, with nothing typed after
/// it yet.
fn spelled(directory: &Path) -> String {
    format!("{}{SEPARATOR}", directory.display())
}

/// Types `text` into the browser a key at a time, answering the listings the
/// keys asked for, in order.
fn type_path(
    application: &mut Application,
    text: &str,
) -> Vec<(DirectoryListingId, ListDirectoryRequest)> {
    text.chars()
        .filter_map(
            |character| match key(application, KeyCode::Char(character)) {
                ApplicationTransition::Continue => None,
                transition => {
                    let (_, listing_id, request) = expect_listing(transition);
                    Some((listing_id, request))
                }
            },
        )
        .collect()
}

/// Takes every character of the path field back, whatever the leading parts
/// it passes through ask for on the way.
fn clear_path_field(application: &mut Application) {
    for _ in path_field(application).chars() {
        key(application, KeyCode::Backspace);
    }
    assert_eq!(path_field(application), "");
}

fn paste(application: &mut Application, text: &str) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Paste(text.to_owned()))
        .expect("deliver a paste")
}

/// The name on the row focus stands on, without what is said beside it.
fn focused(application: &Application) -> String {
    let rows = tree(application);
    let row = rows
        .iter()
        .find(|row| row.starts_with('›'))
        .unwrap_or_else(|| panic!("no row is focused: {rows:?}"));
    let name = row.trim_start_matches(['›', ' ', '▸', '▾']);
    name.split(" · ").next().unwrap_or(name).to_owned()
}

/// A frame row's content between the overlay box's borders: the last two on
/// the row, since a Sidebar shown beside the main view draws its edge
/// further left.
fn inside_box(row: &str) -> &str {
    let mut borders = row.rmatch_indices('│').map(|(index, _)| index);
    match (borders.next(), borders.next()) {
        (Some(end), Some(start)) => &row[start + '│'.len_utf8()..end],
        _ => row,
    }
}
