//! The Directory Browser: opening it by `/browse` or its chord at the
//! Landing's Execution Directory, walking and opening its tree, the listings
//! the Outlook's Server answers it with, choosing a directory to land in, and
//! closing it.
//!
//! Every directory here is one the Client's own disk does not hold, so a row
//! the tree draws can only have come from the Server's answer.

use std::path::{Path, PathBuf};

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
    assert_eq!(tree(&application), ["› ▾ here · [current]", "    ▸ alpha"]);
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
    let footer = &screen[rendered_row(&screen, "Esc close")];
    for named in ["↑↓", "Space", "→", "←", "Enter choose", "Esc"] {
        assert!(footer.contains(named), "{named} is named: {footer}");
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
/// words while they are not.
#[test]
fn a_worktree_root_shows_the_branch_it_stands_on_as_a_sidebar_row_draws_it() {
    let here = directory(&["nowhere", "here"]);
    let children = [
        ("linked", linked_worktree_root(branch("topic"))),
        ("main", repository_root(branch("main"))),
    ];

    assert_eq!(
        tree_drawn(&here, Icons::Hidden, Vec::new(), &children)[1..],
        ["    ▸ linked · topic (worktree)", "    ▸ main · main"]
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
        ["    ▸ detached · 0123456", "    ▸ linked · 0123456"],
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
        ["    ▸ linked · [unavailable]", "    ▸ main · [unavailable]"]
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
        ["    ▸ bare.git · [bare]", "    ▸ plain"]
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
/// Sidebar's listing knows them, wears that Workspace's Icon; one with none
/// is drawn as any other directory is, and no Icon is drawn while Icons are
/// hidden, as the Workspace Picker draws its rows.
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
        ["    ▸ iconed", "    ▸ iconless", "    ▸ unknown"]
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
        ["› ▾ here · [current]", "    ▸ other · main", "    ▸ plain",],
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
/// Repository glyph where the folder's would; while Icons are hidden none of
/// them is drawn, and what each row says in words still tells them apart.
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
            "    ▸ iconed-repository · main",
            "    ▸ iconless-repository · main",
            "    ▸ plain",
            "    ▸ repository · main",
        ]
    );
}

#[derive(Clone, Copy)]
enum Icons {
    Shown,
    Hidden,
}

/// The tree a browser opened at `here` draws once the Server lists
/// `children`, each with what it is to source control, while the Sidebar's
/// listing holds `sessions` and Icons are shown or hidden.
fn tree_drawn(
    here: &Path,
    icons: Icons,
    sessions: Vec<SessionListItem>,
    children: &[(&str, DirectorySourceControl)],
) -> Vec<String> {
    let mut application = connected_application(here);
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
    answer_with(&mut application, listing_id, here, children);
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
    answer_with(application, listing_id, root, &children);
}

/// The Server's listing of `root` with child directories by name, each with
/// what the Server read it to be.
fn answer_with(
    application: &mut Application,
    listing_id: DirectoryListingId,
    root: &Path,
    children: &[(&str, DirectorySourceControl)],
) {
    application
        .handle_event(ApplicationEvent::DirectoryListed {
            listing_id,
            result: Ok(DirectoryListing {
                root: root.to_owned(),
                parent: root.parent().map(Path::to_owned),
                children: children
                    .iter()
                    .map(|(name, source_control)| ChildDirectory {
                        name: (*name).to_owned(),
                        path: root.join(name),
                        source_control: source_control.clone(),
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
    // The footer names the keys bare where the box is too narrow to say what
    // each does, as it is beside a Sidebar, and begins with Up and Down
    // either way.
    let footer = rendered_row(&screen, "↑↓");
    screen[field + 1..footer]
        .iter()
        .map(|row| inside_box(row).trim_end().to_owned())
        // A refusal of the root stands flush beneath the path field, where a
        // refusal of any other directory is indented beneath its row.
        .skip_while(|row| row.starts_with("Error:"))
        .filter(|row| !row.is_empty())
        .collect()
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
