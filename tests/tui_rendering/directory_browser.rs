//! The Directory Browser: opening it by `/browse` or its chord at the
//! Landing's Execution Directory, walking and opening its tree, the listings
//! the Outlook's Server answers it with, the path field that re-roots and
//! narrows the tree as it is typed, choosing a directory to land in, the
//! pointer and wheel driving the tree, and closing it.
//!
//! Every directory here is one the Client's own disk does not hold, so a row
//! the tree draws can only have come from the Server's answer. Each is rooted
//! per platform and spelled with the platform's separator, which is the local
//! Server's own.

use std::path::{MAIN_SEPARATOR as SEPARATOR, MAIN_SEPARATOR_STR, Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::support::{
    SIDEBAR_WIDE, application_looking_at_studio, click_mouse, connected_application,
    deliver_settings, drawn_in_sidebar, enter_active_session, fixture_instance_id, invoke, key,
    listed_session, model_descriptor, ready_health, rendered_application_buffer,
    rendered_application_rows_at, rendered_row, selected_session_snapshot, selector_label,
    sidebar_column, studio_stops_answering, text_position, type_terminal_text,
    workspace_resolution,
};
use crossterm::event::{
    Event as InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use std::time::{Duration, Instant};
use suru::{
    managed_client::ManagedEvent,
    protocol::{
        AgentSelection, CheckoutAssociation, CheckoutId, CheckoutKind, CheckoutRevision,
        CheckoutSummary, ChildDirectory, DRIVE_LIST, DirectoryListing, DirectorySourceControl,
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

/// The Landing working in a subdirectory of a Worktree names it beneath the
/// composer, and that name is a way in: pressing it opens the browser rooted
/// at the subdirectory, its root row focused and its children asked of the
/// Outlook's Server.
#[test]
fn pressing_the_landing_s_subdirectory_opens_the_browser_rooted_there() {
    let main = directory(&["nowhere", "repo", "main"]);
    let linked = directory(&["nowhere", "repo", "linked"]);
    let source = linked.join("src");
    let mut application = landing_in_worktree_subdirectory(&main, &linked, &source);

    let (outlook, _, request) = expect_listing(press_on(&mut application, "src"));

    assert_eq!(
        outlook,
        Outlook::Local,
        "the Outlook's Server is asked for its directories"
    );
    assert_eq!(
        request,
        ListDirectoryRequest {
            path: source.clone(),
            base: Some(source.clone()),
        },
        "the root is the subdirectory the Landing named"
    );
    assert_eq!(path_field(&application), spelled(&source));
    assert_eq!(tree(&application), ["› ▾ src", "    Loading…"]);
}

/// Opened from the Landing, the browser chooses as it does from anywhere:
/// the directory chosen becomes where the next Session works.
#[test]
fn choosing_from_the_browser_the_landing_s_subdirectory_opened_lands_there() {
    let main = directory(&["nowhere", "repo", "main"]);
    let linked = directory(&["nowhere", "repo", "linked"]);
    let source = linked.join("src");
    let library = source.join("lib");
    let mut application = landing_in_worktree_subdirectory(&main, &linked, &source);
    let (_, listing_id, _) = expect_listing(press_on(&mut application, "src"));
    answer(&mut application, listing_id, &source, &["lib"]);
    key(&mut application, KeyCode::Down);

    let choice = key(&mut application, KeyCode::Enter);

    let (_, surface, request) = expect_choice(&choice);
    assert_eq!(surface, WorkspaceResolutionSurface::WorkspacePicker);
    assert_eq!(
        request,
        ResolveWorkspaceRequest {
            checkout_id: None,
            remembered_execution_directory: None,
            workspace_id: None,
            base: Some(source.clone()),
            path: library.clone(),
        }
    );
    resolve(
        &mut application,
        &choice,
        Ok(inside_linked_worktree(&main, &linked, &library)),
    );
    let location = landing_location(&application);
    assert!(
        location.ends_with(&format!(
            "feature/browse (worktree) · {}",
            Path::new("src").join("lib").display()
        )),
        "the Landing works in the directory chosen: {location}"
    );
    assert_eq!(next_session_directory(&mut application), library);
}

/// Opened from the Landing rather than over the Workspace Picker, the
/// browser has nothing beneath it to go back to: Esc closes it outright.
#[test]
fn escape_closes_the_browser_the_landing_s_subdirectory_opened_outright() {
    let main = directory(&["nowhere", "repo", "main"]);
    let linked = directory(&["nowhere", "repo", "linked"]);
    let source = linked.join("src");
    let mut application = landing_in_worktree_subdirectory(&main, &linked, &source);
    let location = landing_location(&application);
    let (_, listing_id, _) = expect_listing(press_on(&mut application, "src"));
    answer(&mut application, listing_id, &source, &["lib"]);

    assert_eq!(
        key(&mut application, KeyCode::Esc),
        ApplicationTransition::Continue,
        "backing out of the browser asks the Server for nothing"
    );

    let screen = rendered_application_rows_at(&application, WIDTH, HEIGHT).join("\n");
    assert!(!screen.contains("Path:"), "the browser is gone: {screen}");
    assert!(
        !screen.contains(" Workspaces "),
        "and no Workspace Picker stands in its place: {screen}"
    );
    assert_eq!(
        landing_location(&application),
        location,
        "the Landing is as it was"
    );
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
    for named in ["Type a path", "Backspace", "Tab complete", "Alt+H hidden"] {
        assert!(
            path_keys.contains(named),
            "{named} is named above the tree's keys: {path_keys}"
        );
    }
}

/// The Server lists every directory and flags the hidden ones — dot-named,
/// or marked hidden by the platform as `marked` stands for here — so the
/// browser leaves them out by that flag alone, and shows them again without
/// asking the Server a second time.
#[test]
fn hidden_directories_are_left_out_until_alt_h_shows_them_in_place() {
    let here = directory(&["nowhere", "here"]);
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer_flagging_hidden(
        &mut application,
        listing_id,
        &here,
        &[
            (".config", true),
            ("alpha", false),
            ("marked", true),
            ("zeta", false),
        ],
    );
    assert_eq!(
        tree(&application),
        ["› ▾ here · [current]", "    ▸ alpha", "    ▸ zeta"],
        "a hidden directory is left out by default"
    );

    assert_eq!(
        show_or_hide_hidden(&mut application),
        ApplicationTransition::Continue,
        "showing hidden directories asks the Server for nothing"
    );
    assert_eq!(
        tree(&application),
        [
            "› ▾ here · [current]",
            "    ▸ .config",
            "    ▸ alpha",
            "    ▸ marked",
            "    ▸ zeta"
        ],
        "each stands in its place among the others, as the Server ordered them"
    );

    assert_eq!(
        show_or_hide_hidden(&mut application),
        ApplicationTransition::Continue,
        "nor does leaving them out again"
    );
    assert_eq!(
        tree(&application),
        ["› ▾ here · [current]", "    ▸ alpha", "    ▸ zeta"]
    );
}

#[test]
fn hidden_directories_are_left_out_beneath_every_open_directory() {
    let here = directory(&["nowhere", "here"]);
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha"]);
    key(&mut application, KeyCode::Down);
    let (_, listing_id, _) = expect_listing(key(&mut application, KeyCode::Char(' ')));
    answer_flagging_hidden(
        &mut application,
        listing_id,
        &here.join("alpha"),
        &[(".git", true), ("src", false)],
    );
    assert_eq!(
        tree(&application),
        ["  ▾ here · [current]", "›   ▾ alpha", "      ▸ src"]
    );

    show_or_hide_hidden(&mut application);
    assert_eq!(
        tree(&application),
        [
            "  ▾ here · [current]",
            "›   ▾ alpha",
            "      ▸ .git",
            "      ▸ src"
        ]
    );
}

/// The path field's tail and the hidden flag each leave directories out: a
/// hidden child of the root stays out though the tail begins it, and shown,
/// it is still narrowed like any other; beneath the root, where the tail
/// narrows nothing, hidden directories are left out all the same.
#[test]
fn a_hidden_directory_the_tail_begins_is_left_out_until_alt_h_shows_it() {
    let here = directory(&["nowhere", "here"]);
    let mint = here.join("mint");
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer_flagging_hidden(
        &mut application,
        listing_id,
        &here,
        &[
            (".config", true),
            ("alpha", false),
            ("marked", true),
            ("mint", false),
        ],
    );
    type_path(&mut application, "m");
    let (_, listing_id, _) = expect_listing(key(&mut application, KeyCode::Right));
    answer_flagging_hidden(
        &mut application,
        listing_id,
        &mint,
        &[(".git", true), ("src", false)],
    );
    assert_eq!(
        tree(&application),
        ["  ▾ here · [current]", "›   ▾ mint", "      ▸ src"],
        "the tail begins marked, but marked is hidden, so focus lands on mint"
    );

    assert_eq!(
        show_or_hide_hidden(&mut application),
        ApplicationTransition::Continue,
        "showing hidden directories asks the Server for nothing"
    );
    assert_eq!(
        tree(&application),
        [
            "  ▾ here · [current]",
            "    ▸ marked",
            "›   ▾ mint",
            "      ▸ .git",
            "      ▸ src"
        ],
        "marked stands in its place, .config is still narrowed away by the \
         tail, and mint's hidden child is shown though the tail does not \
         begin it"
    );

    assert_eq!(
        show_or_hide_hidden(&mut application),
        ApplicationTransition::Continue
    );
    assert_eq!(
        tree(&application),
        ["  ▾ here · [current]", "›   ▾ mint", "      ▸ src"]
    );
}

/// Every opening otherwise begins afresh, but whether hidden directories are
/// shown is the reader's choice for the rest of the client run.
#[test]
fn the_hidden_directories_choice_lasts_over_closing_and_reopening_the_browser() {
    let here = directory(&["nowhere", "here"]);
    let mut application = connected_application(&here);
    let listed = [(".config", true), ("alpha", false)];
    let (_, listing_id, _) = browse(&mut application);
    answer_flagging_hidden(&mut application, listing_id, &here, &listed);
    show_or_hide_hidden(&mut application);
    key(&mut application, KeyCode::Esc);

    let (_, listing_id, _) = browse(&mut application);
    answer_flagging_hidden(&mut application, listing_id, &here, &listed);
    assert_eq!(
        tree(&application),
        ["› ▾ here · [current]", "    ▸ .config", "    ▸ alpha"],
        "hidden directories are still shown"
    );
    show_or_hide_hidden(&mut application);
    key(&mut application, KeyCode::Esc);

    let (_, listing_id, _) = browse(&mut application);
    answer_flagging_hidden(&mut application, listing_id, &here, &listed);
    assert_eq!(
        tree(&application),
        ["› ▾ here · [current]", "    ▸ alpha"],
        "and once left out again, still left out"
    );
}

/// Leaving hidden directories out takes away the row focus stood on when it
/// is one of them or beneath one, so focus moves to the nearest row drawn
/// above it that remains: never outside the hidden directory's parent, and
/// at furthest the parent itself.
#[test]
fn hiding_the_focused_directory_moves_focus_to_the_nearest_row_above_that_remains() {
    let here = directory(&["nowhere", "here"]);
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer_flagging_hidden(
        &mut application,
        listing_id,
        &here,
        &[("alpha", false), ("marked", true), ("zeta", false)],
    );
    show_or_hide_hidden(&mut application);
    key(&mut application, KeyCode::Down);
    let (_, listing_id, _) = expect_listing(key(&mut application, KeyCode::Char(' ')));
    answer(&mut application, listing_id, &here.join("alpha"), &["src"]);
    key(&mut application, KeyCode::Down);
    key(&mut application, KeyCode::Down);
    let (_, listing_id, _) = expect_listing(key(&mut application, KeyCode::Char(' ')));
    answer(
        &mut application,
        listing_id,
        &here.join("marked"),
        &["inner"],
    );
    key(&mut application, KeyCode::Down);
    assert_eq!(focused(&application), "inner");

    show_or_hide_hidden(&mut application);
    assert_eq!(
        tree(&application),
        [
            "  ▾ here · [current]",
            "    ▾ alpha",
            "›     ▸ src",
            "    ▸ zeta"
        ],
        "focus stands on the row drawn just above the hidden directory"
    );

    show_or_hide_hidden(&mut application);
    assert_eq!(
        tree(&application),
        [
            "  ▾ here · [current]",
            "    ▾ alpha",
            "›     ▸ src",
            "    ▾ marked",
            "      ▸ inner",
            "    ▸ zeta"
        ],
        "shown again, the hidden directory is as it was left, and focus stays put"
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

/// A Workspace the Workspace Picker's listing knows is one of the Outlook's
/// Workspaces as much as one the Sidebar's knows: with the Sidebar hidden
/// from the start, so it never lists, a browser the picker opens draws each
/// Workspace the picker just drew wearing its Icon, and marks only the
/// current one current. What either listing knows is the Outlook's own, so
/// turning toward another Server leaves it behind.
#[test]
fn a_workspace_the_workspace_picker_listed_wears_its_icon_while_the_sidebar_is_hidden() {
    let here = directory(&["nowhere", "here"]);
    let mut application = connected_application(&here);
    let mut settings = EffectiveSettings::default();
    settings.appearance.show_icons = true;
    settings.sidebar.initial_visibility = SidebarVisibility::Hidden;
    let transition = deliver_settings(&mut application, settings);
    assert!(
        !matches!(transition, ApplicationTransition::ListSessions(_)),
        "a hidden Sidebar asks for no Sessions: {transition:?}"
    );
    let ApplicationTransition::ListSessions(request) =
        invoke(&mut application, SemanticCommandId::WorkspaceList)
    else {
        panic!("the Workspace Picker asks for its Sessions");
    };
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request,
            sessions: vec![
                session_in(&here, Some("dev-python")),
                session_in(&here.join("iconed"), Some("dev-rust")),
            ],
        })
        .expect("list the Workspace Picker's Sessions");
    let children = [
        ("iconed", repository_root(branch("main"))),
        ("plain", DirectorySourceControl::Plain),
    ];

    let (_, listing_id, _) = expect_listing(chord(
        &mut application,
        KeyCode::Char('o'),
        KeyModifiers::CONTROL,
    ));
    answer_with(
        &mut application,
        listing_id,
        &here,
        &DirectorySourceControl::Plain,
        &children,
    );

    assert_eq!(
        tree(&application),
        [
            format!("› ▾ {PYTHON} here · [current]"),
            format!("    ▸ {RUST} iconed · {BRANCH} main"),
            format!("    ▸ {FOLDER} plain"),
        ]
    );

    crate::connecting::turn_to_studio(&mut application);
    let (outlook, listing_id, _) =
        expect_listing(invoke(&mut application, SemanticCommandId::WorkspaceBrowse));
    assert_eq!(outlook, Outlook::Remote("studio".to_owned()));
    answer_with(
        &mut application,
        listing_id,
        &here,
        &DirectorySourceControl::Plain,
        &children,
    );

    let toward_studio = tree(&application);
    assert_eq!(
        toward_studio[1..],
        [
            format!("    ▸ {REPOSITORY} iconed · {BRANCH} main"),
            format!("    ▸ {FOLDER} plain"),
        ],
        "the same directory on another Server is no Workspace this client knows there"
    );
    assert!(
        !toward_studio[0].contains(PYTHON) && !toward_studio[0].contains("[current]"),
        "{toward_studio:?}"
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

/// A separator ends the leading part on one of the root's children, which
/// the tree stands on at once, asking for its children by the Server's own
/// path for it, read from the Execution Directory.
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
        }
    );
    assert_eq!(
        tree(&application),
        ["› ▾ beta", "    Loading…"],
        "the tree stands on the directory the leading part names, focused, still being read"
    );

    answer(&mut application, *listing_id, &beta, &["inner"]);

    assert_eq!(tree(&application), ["› ▾ beta", "    ▸ inner"]);
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
/// Directory: `..` names a directory on its own, before any separator, and
/// the tree stands on the directory the Server answers with, while the field
/// keeps what the reader typed.
#[test]
fn a_relative_path_is_read_from_the_execution_directory_and_the_server_s_root_is_shown() {
    let parent = directory(&["nowhere"]);
    let here = parent.join("here");
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha"]);
    clear_path_field(&mut application);

    let asked = type_path(&mut application, "..");

    let Some((listing_id, request)) = asked.last() else {
        panic!("`..` is asked for as soon as it is typed");
    };
    assert_eq!(
        *request,
        ListDirectoryRequest {
            path: PathBuf::from(".."),
            base: Some(here.clone()),
        }
    );
    assert_eq!(
        tree(&application),
        ["› ▾ here · [current]", "    ▸ alpha"],
        "`..` is not a partial name narrowing the children"
    );
    answer(&mut application, *listing_id, &parent, &["here", "there"]);
    assert_eq!(
        tree(&application),
        ["› ▾ nowhere", "    ▸ here · [current]", "    ▸ there"],
        "the root is the directory the Server read the path as"
    );
    assert!(
        type_path(&mut application, MAIN_SEPARATOR_STR).is_empty(),
        "a separator after `..` leaves the leading part as it was"
    );
    assert_eq!(
        path_field(&application),
        format!("..{SEPARATOR}"),
        "the field keeps what the reader typed"
    );
}

/// `.` names the Execution Directory itself, so it lists every child rather
/// than narrowing them to the dot-named.
#[test]
fn a_dot_alone_is_read_as_the_execution_directory() {
    let here = directory(&["nowhere", "here"]);
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha", "beta"]);
    clear_path_field(&mut application);

    let asked = type_path(&mut application, ".");

    let [(listing_id, request)] = asked.as_slice() else {
        panic!("`.` is asked for as soon as it is typed: {asked:?}");
    };
    assert_eq!(
        *request,
        ListDirectoryRequest {
            path: PathBuf::from("."),
            base: Some(here.clone()),
        }
    );
    answer(&mut application, *listing_id, &here, &["alpha", "beta"]);
    assert_eq!(
        tree(&application),
        ["› ▾ here · [current]", "    ▸ alpha", "    ▸ beta"]
    );
    assert_eq!(path_field(&application), ".");
}

/// `~` names the Server's home on its own, before any separator; until the
/// Server answers, the tree stands where it was and the field says its root
/// is being read.
#[test]
fn a_tilde_is_read_from_the_server_s_home() {
    let here = directory(&["nowhere", "here"]);
    let home = directory(&["nowhere", "home"]);
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha"]);
    clear_path_field(&mut application);

    let asked = type_path(&mut application, "~");

    let [(listing_id, request)] = asked.as_slice() else {
        panic!("the home is asked for as soon as `~` is typed: {asked:?}");
    };
    assert_eq!(
        *request,
        ListDirectoryRequest {
            path: PathBuf::from("~"),
            base: Some(here.clone()),
        },
        "the Server reads `~` itself"
    );
    assert_eq!(beneath_path_field(&application), ["Loading…"]);
    assert_eq!(tree(&application), ["› ▾ here · [current]", "    ▸ alpha"]);

    answer(&mut application, *listing_id, &home, &["projects"]);
    assert!(beneath_path_field(&application).is_empty());
    assert_eq!(tree(&application), ["› ▾ home", "    ▸ projects"]);
    assert!(type_path(&mut application, MAIN_SEPARATOR_STR).is_empty());
    assert_eq!(path_field(&application), format!("~{SEPARATOR}"));
}

/// A home the Server cannot read has nothing shorter to fall back to, so the
/// tree stays on the last root it read, with every child shown.
#[test]
fn a_home_the_server_cannot_read_leaves_the_last_good_root_standing() {
    let here = directory(&["nowhere", "here"]);
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha"]);
    clear_path_field(&mut application);
    let asked = type_path(&mut application, "~");
    let [(listing_id, _)] = asked.as_slice() else {
        panic!("the home is asked for once: {asked:?}");
    };

    assert_eq!(
        refuse(
            &mut application,
            *listing_id,
            "The Server's home is unknown"
        ),
        ApplicationTransition::Continue,
        "nothing shorter than `~` is asked for"
    );

    assert_eq!(
        beneath_path_field(&application),
        ["Error: The Server's home is unknown"]
    );
    assert_eq!(tree(&application), ["› ▾ here · [current]", "    ▸ alpha"]);
}

/// A leading part naming no directory the Server can read leaves the tree on
/// the longest part of it that does, which here is the root it already stood
/// on: the name after that part narrows its children as it did before the
/// separator, and why the rest was refused stands beneath the field until
/// the leading part changes.
#[test]
fn a_leading_part_the_server_refuses_leaves_the_tree_on_the_longest_part_it_reads() {
    let here = directory(&["nowhere", "here"]);
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha", "beta"]);
    let asked = type_path(&mut application, &format!("al{SEPARATOR}"));
    let [(listing_id, request)] = asked.as_slice() else {
        panic!("the leading part is asked for once: {asked:?}");
    };
    assert_eq!(request.path, here.join("al"));

    assert_eq!(
        refuse(&mut application, *listing_id, "No directory there"),
        ApplicationTransition::Continue,
        "the root's own listing is had already, so nothing more is asked"
    );

    assert_eq!(
        beneath_path_field(&application),
        ["Error: No directory there"]
    );
    assert_eq!(tree(&application), ["  ▾ here · [current]", "›   ▸ alpha"]);
    assert_eq!(
        path_field(&application),
        format!("{}al{SEPARATOR}", spelled(&here)),
        "the field keeps what the reader typed"
    );

    key(&mut application, KeyCode::Backspace);
    assert!(
        beneath_path_field(&application).is_empty(),
        "the refusal goes with the leading part it was for"
    );
}

/// A pasted path whose middle names nothing is walked back a directory at a
/// time, one request at a time, until the Server reads one: that becomes the
/// root, narrowed by the name after it, while the refusal of the whole
/// leading part is what the field says.
#[test]
fn a_pasted_path_missing_its_middle_stands_the_tree_on_its_longest_existing_part() {
    let here = directory(&["nowhere", "here"]);
    let elsewhere = directory(&["elsewhere"]);
    let gone = elsewhere.join("gone");
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha"]);
    clear_path_field(&mut application);

    let pasted = format!("{}{SEPARATOR}missing{SEPARATOR}deep", gone.display());
    let (_, listing_id, request) = expect_listing(paste(&mut application, &pasted));
    assert_eq!(request.path, gone.join("missing"));

    let (_, listing_id, request) =
        expect_listing(refuse(&mut application, listing_id, "No directory there"));
    assert_eq!(
        request.path, gone,
        "the next shorter leading part is asked for"
    );
    let (_, listing_id, request) =
        expect_listing(refuse(&mut application, listing_id, "Not a directory"));
    assert_eq!(request.path, elsewhere);
    assert_eq!(
        tree(&application),
        ["› ▾ here · [current]", "    ▸ alpha"],
        "until one is read the tree stands where it was"
    );

    answer(
        &mut application,
        listing_id,
        &elsewhere,
        &["gone-by", "gondola", "here"],
    );

    assert_eq!(
        tree(&application),
        ["  ▾ elsewhere", "›   ▸ gone-by"],
        "the longest part read is the root, narrowed by the name after it"
    );
    assert_eq!(
        beneath_path_field(&application),
        ["Error: No directory there"]
    );
    assert_eq!(path_field(&application), pasted);
}

/// A directory this opening has already listed is not asked for again when
/// the field comes to it, whether by the name the Server listed it under or
/// by the Server's own path for a root it read another way.
#[test]
fn a_directory_already_listed_is_not_asked_for_again_when_the_field_reaches_it() {
    let parent = directory(&["nowhere"]);
    let here = parent.join("here");
    let alpha = here.join("alpha");
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha", "beta"]);
    key(&mut application, KeyCode::Down);
    let (_, listing_id, _) = expect_listing(key(&mut application, KeyCode::Right));
    answer(&mut application, listing_id, &alpha, &["inner"]);

    assert!(
        type_path(&mut application, &format!("alpha{SEPARATOR}")).is_empty(),
        "alpha was listed when its row was opened"
    );
    assert_eq!(tree(&application), ["› ▾ alpha", "    ▸ inner"]);

    clear_path_field(&mut application);
    let asked = type_path(
        &mut application,
        &format!("..{SEPARATOR}sibling{SEPARATOR}"),
    );
    let Some((listing_id, request)) = asked.last() else {
        panic!("the relative leading part is asked for");
    };
    assert_eq!(request.path, Path::new("..").join("sibling"), "{asked:?}");
    let sibling = parent.join("sibling");
    answer(&mut application, *listing_id, &sibling, &["leaf"]);
    clear_path_field(&mut application);

    assert_eq!(
        paste(&mut application, &spelled(&sibling)),
        ApplicationTransition::Continue,
        "the Server's own path for a root it read by another spelling asks nothing"
    );
    assert_eq!(tree(&application), ["› ▾ sibling", "    ▸ leaf"]);
}

/// Left leaves the former root open beneath its parent even where the reader
/// had closed it.
#[test]
fn left_on_a_closed_root_leaves_it_open_beneath_its_parent() {
    let parent = directory(&["nowhere"]);
    let here = parent.join("here");
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha"]);
    key(&mut application, KeyCode::Char(' '));
    assert_eq!(tree(&application), ["› ▸ here · [current]"]);

    let (_, listing_id, _) = expect_listing(key(&mut application, KeyCode::Left));
    answer(&mut application, listing_id, &parent, &["here", "there"]);

    assert_eq!(
        tree(&application),
        [
            "  ▾ nowhere",
            "›   ▾ here · [current]",
            "      ▸ alpha",
            "    ▸ there"
        ]
    );
}

/// The Server reads a link where it leads, so a directory completed to by
/// its link's name is the tree's root by the path the Server resolved it to,
/// and Left walks up from there.
#[test]
fn tab_into_a_link_then_left_walks_up_from_where_the_link_leads() {
    let parent = directory(&["nowhere"]);
    let here = parent.join("here");
    let link = here.join("link");
    let target = parent.join("target");
    let project = target.join("project");
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["link"]);
    type_path(&mut application, "l");

    let (_, listing_id, request) = expect_listing(key(&mut application, KeyCode::Tab));
    assert_eq!(request.path, link);
    answer_listing(
        &mut application,
        listing_id,
        DirectoryListing {
            root: project.clone(),
            parent: Some(target.clone()),
            source_control: DirectorySourceControl::Plain,
            children: vec![ChildDirectory {
                name: "inner".to_owned(),
                path: project.join("inner"),
                source_control: DirectorySourceControl::Plain,
                hidden: false,
            }],
        },
    );
    assert_eq!(
        tree(&application),
        ["› ▾ project", "    ▸ inner"],
        "the root goes by where the link leads"
    );
    assert_eq!(
        path_field(&application),
        spelled(&link),
        "the field keeps the link's name"
    );
    assert!(
        type_path(&mut application, "i").is_empty(),
        "the root reached by the link's name is the one the tail narrows"
    );
    key(&mut application, KeyCode::Backspace);
    key(&mut application, KeyCode::Up);

    let (_, listing_id, request) = expect_listing(key(&mut application, KeyCode::Left));
    assert_eq!(request.path, target);
    answer(&mut application, listing_id, &target, &["other", "project"]);

    assert_eq!(
        tree(&application),
        [
            "  ▾ target",
            "    ▸ other",
            "›   ▾ project",
            "      ▸ inner"
        ],
        "the former root stands open and focused beneath where the link leads"
    );
}

/// A leading part the root does not list is asked of the Server; typing over
/// it lets that request go, so its answer moves nothing, and asking again is
/// a request of its own.
#[test]
fn an_answer_for_a_leading_part_since_typed_over_moves_nothing() {
    let here = directory(&["nowhere", "here"]);
    let gamma = here.join("gamma");
    let mut application = connected_application(&here);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha", "beta"]);
    let asked = type_path(&mut application, &format!("gamma{SEPARATOR}"));
    let [(superseded, _)] = asked.as_slice() else {
        panic!("the leading part is asked for once: {asked:?}");
    };
    let superseded = *superseded;
    key(&mut application, KeyCode::Backspace);

    answer(&mut application, superseded, &gamma, &["stale"]);
    assert_eq!(
        tree(&application),
        ["› ▾ here · [current]"],
        "the leading part asked for is no longer the field's"
    );

    let asked = type_path(&mut application, MAIN_SEPARATOR_STR);
    let [(awaited, _)] = asked.as_slice() else {
        panic!("typing the separator again asks again: {asked:?}");
    };
    assert_ne!(*awaited, superseded);
    answer(&mut application, superseded, &gamma, &["stale"]);
    assert_eq!(
        tree(&application),
        ["› ▾ here · [current]"],
        "an answer to the earlier request is not this one's"
    );

    answer(&mut application, *awaited, &gamma, &["fresh"]);
    assert_eq!(tree(&application), ["› ▾ gamma", "    ▸ fresh"]);
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
                    hidden: false,
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

/// The browser with `children` listed beneath its root at `here`, on a
/// clock the test moves by hand (see [`pointing`]).
fn browsing(here: &Path, children: &[&str]) -> (Application, Clock) {
    let (mut application, clock) = pointing(connected_application(here));
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, here, children);
    (application, clock)
}

/// A single press on a row focuses it and opens it as Space does, asking the
/// Server for its children; a press on it once the double-press interval has
/// passed is a single press again, and closes it.
#[test]
fn a_press_on_a_row_focuses_it_and_opens_or_closes_it_as_space_does() {
    let here = directory(&["nowhere", "here"]);
    let (mut keyed, _) = browsing(&here, &["alpha", "beta"]);
    key(&mut keyed, KeyCode::Down);
    let spaced = key(&mut keyed, KeyCode::Char(' '));
    let (mut pointed, clock) = browsing(&here, &["alpha", "beta"]);

    let pressed = press_on(&mut pointed, "alpha");

    assert_eq!(pressed, spaced, "the press asks what Space asks");
    let (_, _, request) = expect_listing(pressed);
    assert_eq!(request.path, here.join("alpha"));
    assert_eq!(
        tree(&pointed),
        [
            "  ▾ here · [current]",
            "›   ▾ alpha",
            "      Loading…",
            "    ▸ beta"
        ],
        "the row pressed is focused and open"
    );

    wait(&clock, CLICK_INTERVAL + Duration::from_millis(1));
    assert_eq!(
        press_on(&mut pointed, "alpha"),
        ApplicationTransition::Continue,
        "closing a row asks nothing"
    );
    assert_eq!(
        tree(&pointed),
        ["  ▾ here · [current]", "›   ▸ alpha", "    ▸ beta"],
        "a press past the interval is a single press, closing the row"
    );
}

/// Two presses on a row within the double-press interval choose it as Enter
/// does, and the same Landing stands in the browser's place; the first of
/// them has already opened the row, as any single press does.
#[test]
fn a_double_press_on_a_row_chooses_it_as_enter_does() {
    let here = directory(&["nowhere", "here"]);
    let (mut keyed, _) = browsing(&here, &["alpha", "beta"]);
    key(&mut keyed, KeyCode::Down);
    key(&mut keyed, KeyCode::Down);
    key(&mut keyed, KeyCode::Char(' '));
    let entered = key(&mut keyed, KeyCode::Enter);
    let (mut pointed, clock) = browsing(&here, &["alpha", "beta"]);

    expect_listing(press_on(&mut pointed, "beta"));
    wait(&clock, CLICK_INTERVAL);
    let pressed = press_on(&mut pointed, "beta");

    let (_, surface, request) = expect_choice(&pressed);
    assert_eq!(surface, WorkspaceResolutionSurface::WorkspacePicker);
    assert_eq!(request.path, here.join("beta"));
    assert_eq!(pressed, entered, "the double press asks what Enter asks");
    assert_eq!(
        rendered_application_rows_at(&pointed, WIDTH, HEIGHT),
        rendered_application_rows_at(&keyed, WIDTH, HEIGHT),
        "and the same Landing stands in the browser's place"
    );
}

/// A double press on a row already open chooses it all the same: the first
/// press closes the row, as any single press on an open row does, and the
/// second, landing on the row where it still stands, chooses it.
#[test]
fn a_double_press_on_an_open_row_closes_it_then_chooses_it() {
    let here = directory(&["nowhere", "here"]);
    let opened = || {
        let (mut application, clock) = browsing(&here, &["alpha", "beta"]);
        key(&mut application, KeyCode::Down);
        let (_, listing_id, _) = expect_listing(key(&mut application, KeyCode::Char(' ')));
        answer(
            &mut application,
            listing_id,
            &here.join("alpha"),
            &["inner"],
        );
        (application, clock)
    };
    let (mut keyed, _) = opened();
    key(&mut keyed, KeyCode::Char(' '));
    let entered = key(&mut keyed, KeyCode::Enter);
    let (mut pointed, clock) = opened();
    assert_eq!(
        tree(&pointed),
        [
            "  ▾ here · [current]",
            "›   ▾ alpha",
            "      ▸ inner",
            "    ▸ beta"
        ]
    );

    assert_eq!(
        press_on(&mut pointed, "alpha"),
        ApplicationTransition::Continue,
        "the first press closes the row, asking nothing"
    );
    assert_eq!(
        tree(&pointed),
        ["  ▾ here · [current]", "›   ▸ alpha", "    ▸ beta"]
    );
    wait(&clock, CLICK_INTERVAL);
    let pressed = press_on(&mut pointed, "alpha");

    let (_, _, request) = expect_choice(&pressed);
    assert_eq!(request.path, here.join("alpha"));
    assert_eq!(pressed, entered, "the second asks what Enter asks");
}

/// The first press of a double press asks for the row's children and the
/// second chooses the row before the Server answers: that answer, arriving
/// once the browser has closed, moves nothing.
#[test]
fn a_listing_answered_after_a_double_press_chose_its_row_moves_nothing() {
    let here = directory(&["nowhere", "here"]);
    let (mut application, clock) = browsing(&here, &["alpha"]);
    let (_, listing_id, request) = expect_listing(press_on(&mut application, "alpha"));
    assert_eq!(request.path, here.join("alpha"));
    wait(&clock, Duration::from_millis(1));
    expect_choice(&press_on(&mut application, "alpha"));
    let landing = rendered_application_rows_at(&application, WIDTH, HEIGHT);
    assert!(
        !landing.join("\n").contains("Path:"),
        "the choice closed the browser: {landing:#?}"
    );

    assert_eq!(
        answer(
            &mut application,
            listing_id,
            &here.join("alpha"),
            &["inner"]
        ),
        ApplicationTransition::Continue,
        "the late answer asks for nothing more"
    );
    assert_eq!(
        rendered_application_rows_at(&application, WIDTH, HEIGHT),
        landing,
        "no browser is drawn again, and the Landing stands as it was"
    );
}

/// Each opening begins afresh, the press it last took included: the first
/// press in a new opening is a single press however soon it follows one in
/// the last, even on the very same cell.
#[test]
fn the_first_press_after_reopening_the_browser_is_a_single_press() {
    let here = directory(&["nowhere", "here"]);
    let (mut application, clock) = browsing(&here, &["alpha"]);
    let alpha = text_position(
        &rendered_application_buffer(&application, WIDTH, HEIGHT),
        "alpha",
    );
    expect_listing(press_at(&mut application, alpha));
    key(&mut application, KeyCode::Esc);
    let (_, listing_id, _) = browse(&mut application);
    answer(&mut application, listing_id, &here, &["alpha"]);
    assert_eq!(
        text_position(
            &rendered_application_buffer(&application, WIDTH, HEIGHT),
            "alpha"
        ),
        alpha,
        "alpha is drawn where it was in the last opening"
    );

    wait(&clock, Duration::from_millis(1));
    let (_, _, request) = expect_listing(press_at(&mut application, alpha));

    assert_eq!(
        request.path,
        here.join("alpha"),
        "the press opens alpha rather than choosing it"
    );
    assert_eq!(
        tree(&application),
        ["  ▾ here · [current]", "›   ▾ alpha", "      Loading…"]
    );
}

/// A double press is two presses on one row: a second press within the
/// interval on the row beside the first is a single press there, opening
/// that row rather than choosing either.
#[test]
fn presses_on_two_rows_within_the_interval_are_each_a_single_press() {
    let here = directory(&["nowhere", "here"]);
    let (mut application, clock) = browsing(&here, &["alpha", "beta"]);
    let (_, listing_id, _) = expect_listing(press_on(&mut application, "alpha"));
    answer(&mut application, listing_id, &here.join("alpha"), &[]);
    assert_eq!(
        tree(&application),
        ["  ▾ here · [current]", "›   ▾ alpha", "    ▸ beta"],
        "alpha lists nothing, so beta is drawn on the line beneath it"
    );

    wait(&clock, Duration::from_millis(1));
    let (_, _, request) = expect_listing(press_on(&mut application, "beta"));

    assert_eq!(request.path, here.join("beta"), "beta is opened");
    assert_eq!(
        tree(&application),
        [
            "  ▾ here · [current]",
            "    ▾ alpha",
            "›   ▾ beta",
            "      Loading…"
        ],
        "and nothing is chosen"
    );
}

/// The wheel over the browser moves its tree a step at a time, as the
/// Sidebar's wheel moves its list, and never focus, asking the Server for
/// nothing; a press then lands on the row drawn where it now stands.
#[test]
fn the_wheel_scrolls_the_tree_without_moving_focus() {
    let here = directory(&["nowhere", "here"]);
    let names = (1..=40)
        .map(|index| format!("dir{index:02}"))
        .collect::<Vec<_>>();
    let (mut application, _) =
        browsing(&here, &names.iter().map(String::as_str).collect::<Vec<_>>());
    for _ in 0..3 {
        key(&mut application, KeyCode::Down);
    }
    let before = tree(&application);
    assert_eq!(before[0], "  ▾ here · [current]");
    assert_eq!(focused(&application), "dir03");

    assert_eq!(
        wheel_on(&mut application, "dir05", MouseEventKind::ScrollDown),
        ApplicationTransition::Continue,
        "the wheel asks nothing of the Server"
    );

    let wheeled = tree(&application);
    assert_eq!(
        wheeled[..before.len() - 3],
        before[3..],
        "the tree moves up three rows: {wheeled:#?}"
    );
    assert_eq!(wheeled.len(), before.len(), "and fills the window still");
    assert_eq!(focused(&application), "dir03", "focus stays where it was");

    wheel_on(&mut application, "dir05", MouseEventKind::ScrollUp);
    assert_eq!(tree(&application), before, "the wheel back up undoes it");

    for _ in 0..20 {
        wheel_on(&mut application, "Path:", MouseEventKind::ScrollDown);
    }
    let foot = tree(&application);
    assert_eq!(
        foot.last().map(String::as_str),
        Some("    ▸ dir40"),
        "the wheel stops where the last row is drawn: {foot:#?}"
    );
    assert_eq!(foot.len(), before.len());
    assert!(
        !foot.iter().any(|row| row.starts_with('›')),
        "the focused row is left behind out of view: {foot:#?}"
    );

    let (_, _, request) = expect_listing(press_on(&mut application, "dir30"));
    assert_eq!(
        request.path,
        here.join("dir30"),
        "a press lands on the row drawn under it"
    );
    assert_eq!(focused(&application), "dir30");
}

/// Focus left where the wheel took the tree away from is where the keys
/// walk on from, bringing the tree back to it.
#[test]
fn the_keys_walk_on_from_focus_the_wheel_left_out_of_view() {
    let here = directory(&["nowhere", "here"]);
    let names = (1..=40)
        .map(|index| format!("dir{index:02}"))
        .collect::<Vec<_>>();
    let (mut application, _) =
        browsing(&here, &names.iter().map(String::as_str).collect::<Vec<_>>());
    key(&mut application, KeyCode::Down);
    for _ in 0..3 {
        wheel_on(&mut application, "Path:", MouseEventKind::ScrollDown);
    }
    assert!(
        !tree(&application).iter().any(|row| row.starts_with('›')),
        "dir01 is out of view"
    );

    key(&mut application, KeyCode::Down);

    assert_eq!(focused(&application), "dir02");
}

/// A press outside the browser closes it exactly as Esc does, and is spent
/// there rather than reaching whatever the browser stood over.
#[test]
fn a_press_outside_the_browser_closes_it_as_escape_does() {
    let here = directory(&["nowhere", "here"]);
    let (mut keyed, _) = browsing(&here, &["alpha"]);
    let escaped = key(&mut keyed, KeyCode::Esc);
    let (mut pointed, _) = browsing(&here, &["alpha"]);
    let screen = rendered_application_rows_at(&pointed, WIDTH, HEIGHT);
    let field = rendered_row(&screen, "Path:");
    assert!(
        !screen[field].starts_with('│'),
        "the browser stands clear of the frame's left column: {screen:#?}"
    );

    let pressed = press_at(
        &mut pointed,
        (0, u16::try_from(field).expect("a frame row")),
    );

    assert_eq!(pressed, escaped, "the press asks what Esc asks");
    let landing = rendered_application_rows_at(&pointed, WIDTH, HEIGHT);
    assert!(
        !landing.join("\n").contains("Path:"),
        "the browser is gone: {landing:#?}"
    );
    assert_eq!(
        landing,
        rendered_application_rows_at(&keyed, WIDTH, HEIGHT),
        "leaving the Landing as Esc leaves it"
    );
}

/// The path field has no cursor to place, the lines beneath a directory
/// still being read or refused are no rows to stand on, and the footer only
/// names keys, so a press on any of them moves nothing.
#[test]
fn presses_on_the_path_field_a_child_being_read_a_refusal_and_the_footer_do_nothing() {
    let here = directory(&["nowhere", "here"]);
    let (mut application, _) = browsing(&here, &["alpha", "beta", "gamma"]);
    key(&mut application, KeyCode::Down);
    key(&mut application, KeyCode::Char(' '));
    key(&mut application, KeyCode::Down);
    let (_, listing_id, _) = expect_listing(key(&mut application, KeyCode::Char(' ')));
    refuse(&mut application, listing_id, "Permission denied");
    let before = rendered_application_rows_at(&application, WIDTH, HEIGHT);
    assert_eq!(
        tree(&application),
        [
            "  ▾ here · [current]",
            "    ▾ alpha",
            "      Loading…",
            "›   ▾ beta",
            "      Error: Permission denied",
            "    ▸ gamma"
        ]
    );

    for needle in [
        "Path:",
        "Loading…",
        "Error: Permission denied",
        "Tab complete",
        "Esc close",
    ] {
        assert_eq!(
            press_on(&mut application, needle),
            ApplicationTransition::Continue,
            "a press on {needle:?} asks nothing"
        );
        assert_eq!(
            rendered_application_rows_at(&application, WIDTH, HEIGHT),
            before,
            "and moves nothing"
        );
    }
}

/// Beside a Sidebar the browser stands further right, centered over the
/// main view, and a press anywhere along a row still lands on the row drawn
/// under it, as far as the box's right edge.
#[test]
fn a_press_lands_on_the_row_drawn_under_it_beside_a_sidebar() {
    let here = directory(&["nowhere", "here"]);
    let (alone, _) = browsing(&here, &["alpha", "beta"]);
    let mut application = connected_application(&here);
    show_sidebar_scoped_to_current_workspace(&mut application, Vec::new());
    let (_, listing_id, _) = browse_by_chord(&mut application);
    answer(&mut application, listing_id, &here, &["alpha", "beta"]);
    let (column_alone, _) =
        text_position(&rendered_application_buffer(&alone, WIDTH, HEIGHT), "beta");
    let (column, line) = text_position(
        &rendered_application_buffer(&application, WIDTH, HEIGHT),
        "beta",
    );
    assert!(
        column > column_alone,
        "the Sidebar moves the browser right, from {column_alone} to {column}"
    );
    let screen = rendered_application_rows_at(&application, WIDTH, HEIGHT);
    let right_edge = screen[usize::from(line)]
        .chars()
        .collect::<Vec<_>>()
        .iter()
        .rposition(|cell| *cell == '│')
        .expect("the browser's box is drawn");

    let (_, _, request) = expect_listing(press_at(
        &mut application,
        (u16::try_from(right_edge - 1).expect("a frame column"), line),
    ));

    assert_eq!(request.path, here.join("beta"));
    assert_eq!(focused(&application), "beta");
}

/// A press goes by the frame the reader saw: where a listing arrives between
/// that frame and the press, moving the rows beneath the directory it lists,
/// the press lands on the row the frame drew under it rather than on
/// whatever the listing has since moved there.
#[test]
fn a_press_lands_on_the_row_the_last_frame_drew_there_though_a_listing_moved_it_since() {
    let here = directory(&["nowhere", "here"]);
    let (mut application, _) = browsing(&here, &["alpha", "beta"]);
    key(&mut application, KeyCode::Down);
    let (_, listing_id, _) = expect_listing(key(&mut application, KeyCode::Char(' ')));
    let beta = text_position(
        &rendered_application_buffer(&application, WIDTH, HEIGHT),
        "beta",
    );
    answer(
        &mut application,
        listing_id,
        &here.join("alpha"),
        &["inner", "other"],
    );

    let (_, _, request) = expect_listing(press_at(&mut application, beta));

    assert_eq!(
        request.path,
        here.join("beta"),
        "the press opens beta, drawn there when it was made, rather than the row the listing \
         has since moved onto that line"
    );
    assert_eq!(
        tree(&application),
        [
            "  ▾ here · [current]",
            "    ▾ alpha",
            "      ▸ inner",
            "      ▸ other",
            "›   ▾ beta",
            "      Loading…"
        ]
    );
}

/// A row the keys have taken out of the tree since the last frame — here
/// narrowed away by a letter typed into the path field — is no longer there
/// to press, so a press where it was drawn moves nothing.
#[test]
fn a_press_where_a_row_since_narrowed_away_was_drawn_moves_nothing() {
    let here = directory(&["nowhere", "here"]);
    let (mut application, _) = browsing(&here, &["alpha", "beta"]);
    let beta = text_position(
        &rendered_application_buffer(&application, WIDTH, HEIGHT),
        "beta",
    );
    assert!(
        type_path(&mut application, "a").is_empty(),
        "the leading part still names the root"
    );

    assert_eq!(
        press_at(&mut application, beta),
        ApplicationTransition::Continue,
        "nothing is asked for beta"
    );
    assert_eq!(
        tree(&application),
        ["  ▾ here · [current]", "›   ▸ alpha"],
        "and focus stays on alpha, the one row the tail leaves"
    );
}

/// In a narrow terminal a long name is cut at the box's edge, and a press on
/// what is left of its row still lands on it.
#[test]
fn a_press_on_a_row_cut_short_by_a_narrow_terminal_lands_on_it() {
    const NARROW: (u16, u16) = (50, HEIGHT);
    let long = "a-directory-whose-name-runs-well-past-the-edge-of-the-box";
    let here = directory(&["nowhere", "here"]);
    let (mut application, _) = browsing(&here, &["alpha", long]);
    let rows = rendered_application_rows_at(&application, NARROW.0, NARROW.1);
    let cut = &rows[rendered_row(&rows, "a-directory-whose")];
    assert!(
        !cut.contains(long) && cut.contains('…'),
        "the name is cut at the box's edge: {cut:?}"
    );

    let (_, _, request) =
        expect_listing(press_on_in(&mut application, NARROW, "a-directory-whose"));

    assert_eq!(request.path, here.join(long));
    assert_eq!(focused(&application), long, "the row pressed is focused");
}

/// The wheel outside the browser's box goes on reaching what it is over —
/// the Sidebar's list over the Sidebar — and never moves the tree.
#[test]
fn the_wheel_outside_the_browser_moves_what_it_is_over_and_not_the_tree() {
    let here = directory(&["nowhere", "here"]);
    let titles = (1..=20)
        .map(|index| format!("Work {index:02}"))
        .collect::<Vec<_>>();
    let mut application = connected_application(&here);
    show_sidebar_scoped_to_current_workspace(
        &mut application,
        titles
            .iter()
            .map(|title| working(title, SessionId::new(), &here))
            .collect(),
    );
    let (_, listing_id, _) = browse_by_chord(&mut application);
    let names = (1..=40)
        .map(|index| format!("dir{index:02}"))
        .collect::<Vec<_>>();
    answer(
        &mut application,
        listing_id,
        &here,
        &names.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    let drawn_titles = |application: &Application| {
        let rows = rendered_application_rows_at(application, WIDTH, HEIGHT);
        titles
            .iter()
            .filter(|title| drawn_in_sidebar(&rows, title))
            .cloned()
            .collect::<Vec<_>>()
    };
    let listed = drawn_titles(&application);
    let tree_before = tree(&application);
    let rows = rendered_application_rows_at(&application, WIDTH, HEIGHT);
    let line = u16::try_from(rendered_row(&rows, &listed[0])).expect("a frame row");

    assert_eq!(
        wheel_at(&mut application, (1, line), MouseEventKind::ScrollDown),
        ApplicationTransition::Continue
    );

    let wheeled = drawn_titles(&application);
    assert!(
        !wheeled.contains(&listed[0]),
        "the wheel over the Sidebar moves its list: {listed:?} became {wheeled:?}"
    );
    assert_eq!(tree(&application), tree_before, "and never the tree");
}

/// The browser offers no menu of its own, and a right press while it stands
/// opens none beneath it either, over the Sidebar's rows or its own.
#[test]
fn a_right_press_opens_no_menu_while_the_browser_stands() {
    let here = directory(&["nowhere", "here"]);
    let mut application = connected_application(&here);
    let now = SessionTimestamp::now().0;
    show_sidebar_scoped_to_current_workspace(
        &mut application,
        vec![listed_session(
            SessionId::new(),
            "Earlier work",
            &here,
            now,
            now,
        )],
    );
    let (_, listing_id, _) = browse_by_chord(&mut application);
    answer(&mut application, listing_id, &here, &["alpha"]);
    let rows = tree(&application);
    assert!(drawn_in_sidebar(
        &rendered_application_rows_at(&application, WIDTH, HEIGHT),
        "Earlier work"
    ));

    for needle in ["Earlier work", "alpha"] {
        let (column, row) = text_position(
            &rendered_application_buffer(&application, WIDTH, HEIGHT),
            needle,
        );
        assert_eq!(
            click_mouse(
                &mut application,
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Right),
                    column,
                    row,
                    modifiers: KeyModifiers::NONE,
                },
            )
            .expect("press the right button"),
            ApplicationTransition::Continue
        );
        let screen = rendered_application_rows_at(&application, WIDTH, HEIGHT).join("\n");
        assert!(
            !screen.contains("Delete") && !screen.contains("Settle"),
            "a right press on {needle:?} opens no menu: {screen}"
        );
        assert_eq!(tree(&application), rows, "and leaves the tree as it was");
    }
}

/// A double press chooses through the command Enter invokes, so a Remote
/// that has stopped answering refuses it as it refuses Enter, saying why
/// inside the browser.
#[test]
fn a_double_press_is_refused_while_the_remote_has_stopped_answering_as_enter_is() {
    let browsing_studio = || {
        let (mut application, _) = pointing(application_looking_at_studio());
        let (_, listing_id, request) =
            expect_listing(invoke(&mut application, SemanticCommandId::WorkspaceBrowse));
        answer(&mut application, listing_id, &request.path, &["alpha"]);
        studio_stops_answering(&mut application, 1, Duration::from_secs(5));
        application
    };
    let mut keyed = browsing_studio();
    key(&mut keyed, KeyCode::Down);
    key(&mut keyed, KeyCode::Char(' '));
    let entered = key(&mut keyed, KeyCode::Enter);
    let mut pointed = browsing_studio();

    press_on(&mut pointed, "alpha");
    let pressed = press_on(&mut pointed, "alpha");

    assert_eq!(entered, ApplicationTransition::Continue, "Enter is refused");
    assert_eq!(pressed, entered, "and so is the double press");
    assert_eq!(
        rendered_application_rows_at(&pointed, WIDTH, HEIGHT),
        rendered_application_rows_at(&keyed, WIDTH, HEIGHT)
    );
    assert!(
        rendered_application_rows_at(&pointed, WIDTH, HEIGHT)
            .join("\n")
            .contains("Error: studio is unreachable"),
        "the refusal is said inside the browser"
    );
}

/// A double press waits out an unsettled Agent Selection change as Enter
/// does, leaving the browser open where the reader pressed.
#[test]
fn a_double_press_waits_while_an_agent_selection_change_is_unsettled_as_enter_does() {
    let here = directory(&["nowhere", "here"]);
    let changing = || {
        let (mut application, _) = pointing(Application::new(&here, TerminalFacts::default()));
        application
            .handle_event(ApplicationEvent::SessionAttached(
                selected_session_snapshot(SessionId::new(), &here, reasoning_selection("low")),
            ))
            .expect("attach a Session with an Agent Selection");
        warm_model_catalog(&mut application);
        chord(&mut application, KeyCode::Char('t'), KeyModifiers::CONTROL);
        let (_, listing_id, _) = browse_by_chord(&mut application);
        answer(&mut application, listing_id, &here, &["alpha"]);
        application
    };
    let mut keyed = changing();
    key(&mut keyed, KeyCode::Down);
    key(&mut keyed, KeyCode::Char(' '));
    let entered = key(&mut keyed, KeyCode::Enter);
    let mut pointed = changing();

    press_on(&mut pointed, "alpha");
    let pressed = press_on(&mut pointed, "alpha");

    assert_eq!(entered, ApplicationTransition::Continue, "Enter waits");
    assert_eq!(pressed, entered, "and so does the double press");
    assert_eq!(focused(&pointed), "alpha");
    assert_eq!(
        rendered_application_rows_at(&pointed, WIDTH, HEIGHT),
        rendered_application_rows_at(&keyed, WIDTH, HEIGHT)
    );
}

/// How long two presses may stand apart on the clock [`pointing`] gives an
/// Application and still be one double press.
const CLICK_INTERVAL: Duration = Duration::from_millis(300);

/// Gives `application` a clock the test moves by hand, with a double-press
/// interval of [`CLICK_INTERVAL`] on it, so whether two presses are one
/// double press is the test's to say rather than how fast it runs.
fn pointing(application: Application) -> (Application, Clock) {
    let now = Arc::new(Mutex::new(Instant::now()));
    let clock = Arc::clone(&now);
    (
        application
            .with_presentation_clock(move || *clock.lock().expect("read the test's clock"))
            .with_click_interval(CLICK_INTERVAL),
        now,
    )
}

/// A clock a test moves by hand.
type Clock = Arc<Mutex<Instant>>;

fn wait(clock: &Clock, by: Duration) {
    *clock.lock().expect("move the test's clock") += by;
}

/// Presses the left button on the first cell of `needle` as the frame draws
/// it, so the press lands where the reader would point.
fn press_on(application: &mut Application, needle: &str) -> ApplicationTransition {
    press_on_in(application, (WIDTH, HEIGHT), needle)
}

/// [`press_on`] in a frame of `size`.
fn press_on_in(
    application: &mut Application,
    (width, height): (u16, u16),
    needle: &str,
) -> ApplicationTransition {
    let buffer = rendered_application_buffer(application, width, height);
    press_at(application, text_position(&buffer, needle))
}

fn press_at(application: &mut Application, (column, row): (u16, u16)) -> ApplicationTransition {
    click_mouse(
        application,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        },
    )
    .expect("press the left button")
}

/// Turns the wheel a step `kind` over the first cell of `needle` as the
/// frame draws it.
fn wheel_on(
    application: &mut Application,
    needle: &str,
    kind: MouseEventKind,
) -> ApplicationTransition {
    let buffer = rendered_application_buffer(application, WIDTH, HEIGHT);
    wheel_at(application, text_position(&buffer, needle), kind)
}

/// Turns the wheel a step `kind` over one cell of the frame.
fn wheel_at(
    application: &mut Application,
    (column, row): (u16, u16),
    kind: MouseEventKind,
) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }))
        .expect("turn the wheel")
}

/// Above a drive on Windows stands the drive list, which the Server names as
/// the drive root's parent: Left reaches it as Left reaches any parent, the
/// former drive open and focused beneath it, and the path field names it as
/// nothing, since it is no path that exists. The drives are what the Server
/// can see, so these are a Windows Remote's answers whichever platform this
/// Client runs on.
#[test]
fn left_on_a_drive_root_reaches_the_drive_list_with_the_path_field_empty() {
    let mut application = browsing_a_windows_drive(r"C:\", &["Users", "Windows"]);

    let (_, listing_id, request) = expect_listing(key(&mut application, KeyCode::Left));

    assert_eq!(
        request,
        ListDirectoryRequest {
            path: PathBuf::from(DRIVE_LIST),
            base: Some(PathBuf::from(r"C:\")),
        },
        "the drive list is asked for by the path the Server named the drive's parent"
    );
    assert_eq!(path_field(&application), "", "the drive list is no path");
    assert_eq!(tree(&application), ["  ▾ Drives", "    Loading…"]);

    answer_drive_list(&mut application, listing_id, &[r"C:\", r"D:\"]);
    assert_eq!(path_field(&application), "");
    assert_eq!(
        tree(&application),
        [
            "  ▾ Drives",
            r"›   ▾ C:\ · [current]",
            "      ▸ Users",
            "      ▸ Windows",
            r"    ▸ D:\"
        ],
        "the former drive stands open and focused among the drives"
    );
}

/// The drive list has no parent, so there is nowhere further up to go.
#[test]
fn left_on_the_drive_list_moves_nothing() {
    let mut application = drive_list_reached_from(r"C:\", &[r"C:\", r"D:\"]);
    key(&mut application, KeyCode::Up);
    assert_eq!(focused(&application), "Drives");
    let rows = tree(&application);

    assert_eq!(
        key(&mut application, KeyCode::Left),
        ApplicationTransition::Continue,
        "nothing is asked of the Server"
    );
    assert_eq!(path_field(&application), "");
    assert_eq!(tree(&application), rows);
}

/// The drive list is not a directory a Session can work in, so Enter on its
/// row chooses nothing and leaves the browser open where it stands.
#[test]
fn enter_on_the_drive_list_chooses_nothing() {
    let mut application = drive_list_reached_from(r"C:\", &[r"C:\", r"D:\"]);
    key(&mut application, KeyCode::Up);
    let rows = tree(&application);

    assert_eq!(
        key(&mut application, KeyCode::Enter),
        ApplicationTransition::Continue,
        "no Workspace resolution is asked for"
    );
    assert_eq!(path_field(&application), "");
    assert_eq!(tree(&application), rows, "the browser stays open");
}

/// A drive is a directory like any other, so Enter on one chooses it.
#[test]
fn enter_on_a_drive_in_the_drive_list_chooses_it() {
    let mut application = drive_list_reached_from(r"C:\", &[r"C:\", r"D:\"]);
    assert_eq!(focused(&application), r"C:\");

    let (_, _, request) = expect_choice(&key(&mut application, KeyCode::Enter));

    assert_eq!(request.path, Path::new(r"C:\"));
}

/// Opening a drive from the drive list roots the tree there rather than
/// unfolding it beneath a root the path field cannot name, so the field
/// names the drive and Left goes back up to the drive list.
#[test]
fn opening_another_drive_from_the_drive_list_roots_the_tree_there() {
    for opens in [KeyCode::Right, KeyCode::Char(' ')] {
        let mut application = drive_list_reached_from(r"C:\", &[r"C:\", r"D:\"]);
        for _ in ["Users", "Windows", r"D:\"] {
            key(&mut application, KeyCode::Down);
        }
        assert_eq!(focused(&application), r"D:\");

        let (_, listing_id, request) = expect_listing(key(&mut application, opens));

        assert_eq!(request.path, Path::new(r"D:\"), "{opens:?}");
        assert_eq!(path_field(&application), r"D:\", "{opens:?}");
        assert_eq!(
            tree(&application),
            [r"› ▾ D:\", "    Loading…"],
            "{opens:?}: the drive is the root, still being read"
        );

        answer_from_windows(
            &mut application,
            listing_id,
            r"D:\",
            Some(DRIVE_LIST),
            &["Data"],
        );
        assert_eq!(tree(&application), [r"› ▾ D:\", "    ▸ Data"], "{opens:?}");

        assert_eq!(
            key(&mut application, KeyCode::Left),
            ApplicationTransition::Continue,
            "{opens:?}: the drive list is listed already"
        );
        assert_eq!(path_field(&application), "", "{opens:?}");
        assert_eq!(
            tree(&application),
            [
                "  ▾ Drives",
                r"    ▸ C:\ · [current]",
                r"›   ▾ D:\",
                "      ▸ Data"
            ],
            "{opens:?}: the drive opened stands open and focused among the drives"
        );
    }
}

/// Left then Right walks up to the drive list and back down into the drive
/// it came from, which is listed already and so asks nothing again.
#[test]
fn right_on_the_former_drive_roots_the_tree_back_there() {
    let mut application = drive_list_reached_from(r"C:\", &[r"C:\", r"D:\"]);

    assert_eq!(
        key(&mut application, KeyCode::Right),
        ApplicationTransition::Continue,
        "the drive's children are had already"
    );

    assert_eq!(path_field(&application), r"C:\");
    assert_eq!(
        tree(&application),
        [r"› ▾ C:\ · [current]", "    ▸ Users", "    ▸ Windows"]
    );
}

/// The empty path field names the drive list, so what is typed into it
/// narrows the drives, and a drive typed whole roots the tree there as any
/// absolute path does.
#[test]
fn typing_a_drive_into_the_empty_path_field_roots_the_tree_there() {
    let mut application = drive_list_reached_from(r"C:\", &[r"C:\", r"D:\"]);

    assert!(
        type_path(&mut application, "d:").is_empty(),
        "the empty leading part still names the drive list"
    );
    assert_eq!(tree(&application), ["  ▾ Drives", r"›   ▸ D:\"]);

    let asked = type_path(&mut application, r"\");

    let [(listing_id, request)] = asked.as_slice() else {
        panic!("the drive typed is asked for once: {asked:?}");
    };
    assert_eq!(request.path, Path::new(r"d:\"));
    answer_from_windows(
        &mut application,
        *listing_id,
        r"D:\",
        Some(DRIVE_LIST),
        &["Data"],
    );
    assert_eq!(path_field(&application), r"d:\");
    assert_eq!(tree(&application), [r"› ▾ D:\", "    ▸ Data"]);
}

/// Before the drive list has been reached an empty path field names the
/// Execution Directory, as it does on any Server, though the Server has named
/// the drive list as a drive's parent by then: clearing the field stands the
/// tree back on the Execution Directory, which is listed already, so nothing
/// is asked — the drive list least of all.
#[test]
fn before_the_drive_list_is_reached_an_empty_path_field_names_the_execution_directory() {
    let mut application = browsing_on_windows(r"C:\Users\me", Some(r"C:\Users"), &["src"]);
    let (_, listing_id, _) = expect_listing(key(&mut application, KeyCode::Left));
    answer_from_windows(
        &mut application,
        listing_id,
        r"C:\Users",
        Some(r"C:\"),
        &["me", "you"],
    );
    key(&mut application, KeyCode::Up);
    let (_, listing_id, _) = expect_listing(key(&mut application, KeyCode::Left));
    answer_from_windows(
        &mut application,
        listing_id,
        r"C:\",
        Some(DRIVE_LIST),
        &["Users", "Windows"],
    );
    assert_eq!(path_field(&application), r"C:\");

    for remaining in [r"C:", "C", ""] {
        assert_eq!(
            key(&mut application, KeyCode::Backspace),
            ApplicationTransition::Continue,
            "{remaining:?}: nothing is asked of the Server"
        );
        assert_eq!(path_field(&application), remaining);
    }

    assert_eq!(
        tree(&application),
        ["› ▾ me · [current]", "    ▸ src"],
        "the tree stands on the Execution Directory, not the drive list"
    );
}

/// Tab on the drive list completes the empty path field to the focused drive,
/// standing the tree there once the Server reads it, as Tab does beneath any
/// root; on the drive list's own row there is nothing to complete.
#[test]
fn tab_on_the_drive_list_completes_the_path_field_to_the_focused_drive() {
    let mut application = drive_list_reached_from(r"C:\", &[r"C:\", r"D:\"]);
    key(&mut application, KeyCode::Up);
    assert_eq!(focused(&application), "Drives");
    assert_eq!(
        key(&mut application, KeyCode::Tab),
        ApplicationTransition::Continue,
        "the drive list's row completes to nothing"
    );
    assert_eq!(path_field(&application), "");
    for _ in [r"C:\", "Users", "Windows", r"D:\"] {
        key(&mut application, KeyCode::Down);
    }
    assert_eq!(focused(&application), r"D:\");

    let (_, listing_id, request) = expect_listing(key(&mut application, KeyCode::Tab));

    assert_eq!(
        request,
        ListDirectoryRequest {
            path: PathBuf::from(r"D:\"),
            base: Some(PathBuf::from(r"C:\")),
        }
    );
    assert_eq!(path_field(&application), r"D:\");
    assert_eq!(beneath_path_field(&application), ["Loading…"]);
    answer_from_windows(
        &mut application,
        listing_id,
        r"D:\",
        Some(DRIVE_LIST),
        &["Data"],
    );
    assert_eq!(path_field(&application), r"D:\");
    assert_eq!(tree(&application), [r"› ▾ D:\", "    ▸ Data"]);
}

/// The path field is empty while the drive list is the root, so Backspace
/// has nothing to take back.
#[test]
fn backspace_on_the_empty_path_field_of_the_drive_list_does_nothing() {
    let mut application = drive_list_reached_from(r"C:\", &[r"C:\", r"D:\"]);
    let rows = tree(&application);

    assert_eq!(
        key(&mut application, KeyCode::Backspace),
        ApplicationTransition::Continue
    );
    assert_eq!(path_field(&application), "");
    assert_eq!(tree(&application), rows);
}

/// The pointer answers on the drive list's row as on any other row: a press
/// closes or opens it as Space does. A double press chooses nothing, as Enter
/// on it chooses nothing, leaving the browser open on the drive list.
#[test]
fn a_press_on_the_drive_list_s_row_opens_or_closes_it_and_a_double_press_chooses_nothing() {
    let (mut application, clock) = pointing(drive_list_reached_from(r"C:\", &[r"C:\", r"D:\"]));

    assert_eq!(
        press_on(&mut application, "Drives"),
        ApplicationTransition::Continue,
        "closing a row asks nothing"
    );
    assert_eq!(tree(&application), ["› ▸ Drives"]);

    wait(&clock, CLICK_INTERVAL);
    assert_eq!(
        press_on(&mut application, "Drives"),
        ApplicationTransition::Continue,
        "the double press chooses nothing"
    );
    assert_eq!(path_field(&application), "", "the browser stays open");
    assert_eq!(tree(&application), ["› ▸ Drives"]);

    wait(&clock, CLICK_INTERVAL + Duration::from_millis(1));
    assert_eq!(
        press_on(&mut application, "Drives"),
        ApplicationTransition::Continue,
        "the drives are listed already"
    );
    assert_eq!(
        tree(&application),
        [
            "› ▾ Drives",
            r"    ▾ C:\ · [current]",
            "      ▸ Users",
            "      ▸ Windows",
            r"    ▸ D:\"
        ],
        "a single press past the interval opens the row again"
    );
}

/// A Client turned toward a Remote on Windows, its Landing working at that
/// Remote's `drive` and the browser open there, the drive listed with plain
/// children by name and the drive list named as its parent.
fn browsing_a_windows_drive(drive: &str, children: &[&str]) -> Application {
    browsing_on_windows(drive, Some(DRIVE_LIST), children)
}

/// A Client turned toward a Remote on Windows, its Landing working at that
/// Remote's `directory` and the browser open there, the directory listed with
/// `parent` and plain children by name. The Remote has said its paths are
/// Windows', so the Client names them as Windows does on every platform.
fn browsing_on_windows(directory: &str, parent: Option<&str>, children: &[&str]) -> Application {
    use suru::protocol::{
        PathStyle, SessionCatalogRevision, SessionCatalogSnapshot, WorkspacePaths,
    };

    let mut application = application_looking_at_studio();
    application
        .handle_event(ApplicationEvent::OriginCatalog {
            outlook: Outlook::Remote("studio".to_owned()),
            event: ManagedEvent::SessionCatalogReconciled(SessionCatalogSnapshot {
                workspace_paths: WorkspacePaths {
                    home: None,
                    style: PathStyle::Windows,
                    sidekick_workspace: None,
                },
                revision: SessionCatalogRevision::INITIAL,
                session_ids: Vec::new(),
                checkout_states: Vec::new(),
            }),
        })
        .expect("the Remote says its paths are Windows'");
    expect_listing(invoke(&mut application, SemanticCommandId::WorkspaceBrowse));
    let choice = key(&mut application, KeyCode::Enter);
    resolve(
        &mut application,
        &choice,
        Ok(ResolvedWorkspace::directory(PathBuf::from(directory))),
    );
    let (_, listing_id, request) =
        expect_listing(invoke(&mut application, SemanticCommandId::WorkspaceBrowse));
    assert_eq!(request.path, Path::new(directory));
    answer_from_windows(&mut application, listing_id, directory, parent, children);
    application
}

/// The browser on a Windows Remote's `drive`, holding `Users` and `Windows`,
/// walked up to the drive list, which the Remote answers with `drives`.
fn drive_list_reached_from(drive: &str, drives: &[&str]) -> Application {
    let mut application = browsing_a_windows_drive(drive, &["Users", "Windows"]);
    let (_, listing_id, _) = expect_listing(key(&mut application, KeyCode::Left));
    answer_drive_list(&mut application, listing_id, drives);
    application
}

/// A Windows Server's listing of `root`, with `parent` and plain children by
/// name, each spelled beneath `root` in that Server's syntax whatever this
/// Client's own is.
fn answer_from_windows(
    application: &mut Application,
    listing_id: DirectoryListingId,
    root: &str,
    parent: Option<&str>,
    children: &[&str],
) -> ApplicationTransition {
    let beneath = if root.ends_with('\\') {
        root.to_owned()
    } else {
        format!(r"{root}\")
    };
    answer_listing(
        application,
        listing_id,
        DirectoryListing {
            root: PathBuf::from(root),
            parent: parent.map(PathBuf::from),
            source_control: DirectorySourceControl::Plain,
            children: children
                .iter()
                .map(|name| ChildDirectory {
                    name: (*name).to_owned(),
                    path: PathBuf::from(format!("{beneath}{name}")),
                    source_control: DirectorySourceControl::Plain,
                    hidden: false,
                })
                .collect(),
        },
    )
}

/// A Windows Server's drive list: parentless, a child for each of `drives`
/// named and spelled as its root.
fn answer_drive_list(
    application: &mut Application,
    listing_id: DirectoryListingId,
    drives: &[&str],
) -> ApplicationTransition {
    answer_listing(
        application,
        listing_id,
        DirectoryListing {
            root: PathBuf::from(DRIVE_LIST),
            parent: None,
            source_control: DirectorySourceControl::Plain,
            children: drives
                .iter()
                .map(|drive| ChildDirectory {
                    name: (*drive).to_owned(),
                    path: PathBuf::from(drive),
                    source_control: DirectorySourceControl::Plain,
                    hidden: false,
                })
                .collect(),
        },
    )
}

fn landing_in_subdirectory(workspace: &Path, subdirectory: &Path) -> Application {
    landing_resolved_to(
        workspace,
        ResolvedWorkspace {
            execution_directory: Some(ExecutionDirectory {
                path: subdirectory.to_owned(),
            }),
            ..ResolvedWorkspace::directory(workspace.to_owned())
        },
    )
}

/// A Landing working in `directory` within the linked Worktree at `linked` of
/// the Repository whose main Worktree is at `main`, which names `directory`
/// beneath its composer as a subdirectory of the Worktree.
fn landing_in_worktree_subdirectory(main: &Path, linked: &Path, directory: &Path) -> Application {
    landing_resolved_to(directory, inside_linked_worktree(main, linked, directory))
}

/// A Landing launched in `launched` that its Server resolved to `resolved`.
fn landing_resolved_to(launched: &Path, resolved: ResolvedWorkspace) -> Application {
    let mut application = Application::new(launched, TerminalFacts::default());
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
            result: Ok(resolved),
        })
        .expect("resolve the Landing where the client launched");
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

/// Presses Alt+H, which shows hidden directories or leaves them out again.
fn show_or_hide_hidden(application: &mut Application) -> ApplicationTransition {
    chord(application, KeyCode::Char('h'), KeyModifiers::ALT)
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
) -> ApplicationTransition {
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
    )
}

/// The Server's listing of `root`, read as `read`, with child directories by
/// name, each with what the Server read it to be.
fn answer_with(
    application: &mut Application,
    listing_id: DirectoryListingId,
    root: &Path,
    read: &DirectorySourceControl,
    children: &[(&str, DirectorySourceControl)],
) -> ApplicationTransition {
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
                    hidden: false,
                })
                .collect(),
        },
    )
}

/// The Server's listing of `root` with plain child directories by name, in
/// the order given, each flagged hidden or not as the Server flagged it.
fn answer_flagging_hidden(
    application: &mut Application,
    listing_id: DirectoryListingId,
    root: &Path,
    children: &[(&str, bool)],
) -> ApplicationTransition {
    answer_listing(
        application,
        listing_id,
        DirectoryListing {
            root: root.to_owned(),
            parent: root.parent().map(Path::to_owned),
            source_control: DirectorySourceControl::Plain,
            children: children
                .iter()
                .map(|(name, hidden)| ChildDirectory {
                    name: (*name).to_owned(),
                    path: root.join(name),
                    source_control: DirectorySourceControl::Plain,
                    hidden: *hidden,
                })
                .collect(),
        },
    )
}

fn answer_listing(
    application: &mut Application,
    listing_id: DirectoryListingId,
    listing: DirectoryListing,
) -> ApplicationTransition {
    application
        .handle_event(ApplicationEvent::DirectoryListed {
            listing_id,
            result: Ok(listing),
        })
        .expect("deliver the Server's listing")
}

fn refuse(
    application: &mut Application,
    listing_id: DirectoryListingId,
    reason: &str,
) -> ApplicationTransition {
    application
        .handle_event(ApplicationEvent::DirectoryListed {
            listing_id,
            result: Err(reason.to_owned()),
        })
        .expect("deliver the Server's refusal")
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
