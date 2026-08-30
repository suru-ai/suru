//! The Workspace Picker: opening it, the Workspaces it puts on offer, the
//! order it stands them in, narrowing them by typing, choosing one, and
//! walking away from it unchanged.

use std::path::{Path, PathBuf};

use crate::support::{
    SIDEBAR_WIDE, add_workspace, connected_application, deliver_settings, drawn_in_sidebar,
    enter_active_session, fixture_instance_id, noncanonical_spelling, ready_health,
    rendered_application_rows, rendered_application_rows_at, rendered_row, selector_label,
    sidebar_column, type_terminal_text, workspace_dir,
};
use crossterm::event::{
    Event as InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use suru::{
    managed_client::ManagedEvent,
    protocol::{
        AgentSelection, EffectiveSettings, ModelAvailability, ModelId, ProviderId, Session,
        SessionId, SessionListItem, SessionStatus, SessionSummary, SessionTimestamp,
        SidebarSettings, SidebarVisibility, SkillCatalog, SkillCatalogCapabilities,
        SkillCatalogRequest, SkillCatalogStatus, SkillDescriptor, SkillId, SkillPromptDelivery,
        Workspace,
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

    let rows = picker_rows_at(&application, 28, 9);
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

/// The whole of the switch: Enter on a row takes the reader out of the picker
/// and puts them on the Landing of the Workspace they chose, ready to write
/// the first Prompt of a Session rooted there.
#[test]
fn enter_closes_the_picker_and_shows_the_landing_of_the_workspace_chosen() {
    let root = workspace_dir();
    let here = workspace_in(root.path(), "here");
    let atlas = workspace_in(root.path(), "atlas");
    let mut application = connected_application(&here);

    open_picker_with(&mut application, vec![rooted("Newer", &atlas, 30)]);
    press(&mut application, KeyCode::Down);
    assert_eq!(selected_row(&application), "atlas");

    assert_eq!(
        choose(&mut application),
        ApplicationTransition::Continue,
        "with no Session open there is nothing to leave behind, and the switch \
         itself asks the server for nothing"
    );

    // Wide enough for the footer to spell the Workspace rather than truncate it.
    let landing = rendered_application_rows_at(&application, 120, 20).join("\n");
    assert!(
        !landing.contains("Workspaces"),
        "the picker is done with: {landing}"
    );
    assert!(
        landing.contains("What would you like to work on?"),
        "the Landing stands in its place: {landing}"
    );
    assert!(
        landing.contains(atlas.to_string_lossy().as_ref()),
        "and it stands in the Workspace the reader chose: {landing}"
    );
}

/// A Workspace offered from old work may have disappeared since the listing
/// was recorded. Choosing it costs nothing: the refusal stands where the
/// reader can see it, and the picker leaves both their row and its listing in
/// place so they can choose again.
#[test]
fn a_workspace_whose_directory_is_gone_is_refused_in_place_and_moves_nothing() {
    let here = workspace_dir();
    let gone = here.path().join("gone");
    let atlas = workspace_in(here.path(), "atlas");
    let mut application = connected_application(here.path());

    let mut sessions = vec![
        rooted("Gone work", &gone, 100),
        rooted("Atlas work", &atlas, 90),
    ];
    for (index, name) in [
        "ledger", "engine", "notes", "website", "service", "client", "tools", "archive",
    ]
    .into_iter()
    .enumerate()
    {
        sessions.push(rooted(
            "Older work",
            &workspace(&["work", name]),
            80 - index as u64,
        ));
    }
    open_picker_with(&mut application, sessions);
    press(&mut application, KeyCode::Down);
    let rows_before = picker_rows(&application);
    assert_eq!(selected_row(&application), "gone");

    assert_eq!(
        choose(&mut application),
        ApplicationTransition::Continue,
        "a refused pick asks nothing of the server and opens no Landing"
    );

    let frame = rendered_application_rows(&application).join("\n");
    assert!(
        frame.contains("No directory there"),
        "the path entry's refusal stands in the picker: {frame}"
    );
    assert!(
        frame.contains("Workspaces"),
        "the picker stays open: {frame}"
    );
    for height in [5, 6] {
        let compact = rendered_application_rows_at(&application, 80, height).join("\n");
        assert!(
            compact.contains("No directory there") && compact.contains("gone"),
            "the refusal and selected row both remain visible at 80x{height}: {compact}"
        );
    }
    assert_eq!(selected_row(&application), "gone");
    assert_eq!(
        picker_rows(&application),
        rows_before,
        "the refused pick neither removes nor rearranges any visible row, even when the viewport is full"
    );

    press(&mut application, KeyCode::Down);
    assert_eq!(selected_row(&application), "atlas");
    assert_eq!(choose(&mut application), ApplicationTransition::Continue);
    let switched = rendered_application_rows_at(&application, 120, 20).join("\n");
    assert!(
        switched.contains("What would you like to work on?"),
        "a subsequent valid pick from the still-open picker closes it on a Landing: {switched}"
    );
    assert_eq!(
        session_picker_scope(&mut application),
        SessionListScope::CurrentWorkspace(atlas),
        "the subsequent valid pick switched normally"
    );
}

/// A path that now stands at a file is refused for what it is. The refusal
/// leaves every reading of "where I am" alone.
#[test]
fn a_workspace_replaced_by_a_file_is_refused_without_moving_any_scope() {
    let here = workspace_dir();
    let file = here.path().join("old-work");
    std::fs::write(&file, "not a directory").expect("replace the old Workspace with a file");
    let mut application = application_choosing_skills(here.path());
    load_skills(&mut application, here.path(), "review");
    type_terminal_text(&mut application, "$rev");
    show_sidebar(&mut application, Vec::new());

    open_picker_with(&mut application, vec![rooted("Old work", &file, 30)]);
    press(&mut application, KeyCode::Down);
    assert_eq!(selected_row(&application), "old-work");

    assert_eq!(choose(&mut application), ApplicationTransition::Continue);
    let refusal = rendered_application_rows_at(&application, SIDEBAR_WIDE, 20).join("\n");
    assert!(
        refusal.contains("Not a directory"),
        "the picker gives the path entry's corresponding refusal: {refusal}"
    );
    assert!(
        refusal.contains("Workspaces"),
        "the picker stays open: {refusal}"
    );
    assert_eq!(selected_row(&application), "old-work");

    press(&mut application, KeyCode::Esc);
    let unchanged = rendered_application_rows_at(&application, SIDEBAR_WIDE, 20);
    let unchanged_frame = unchanged.join("\n");
    assert!(
        unchanged_frame.contains("│ $rev"),
        "the Landing's draft was untouched: {unchanged_frame}"
    );
    assert!(
        unchanged_frame.contains("$review"),
        "the Skill Catalog still answers for the Workspace the reader is in: {unchanged_frame}"
    );
    assert_eq!(
        selector_label(&unchanged),
        "▸ All Workspaces",
        "the Sidebar's chosen scope was untouched"
    );
    assert_eq!(
        session_picker_scope(&mut application),
        SessionListScope::CurrentWorkspace(here.path().to_owned()),
        "current-Workspace scope did not follow the refused path"
    );
}

/// Choosing a Workspace is choosing where the work goes: the Session made
/// next is rooted there rather than where the client was launched.
#[test]
fn the_workspace_the_reader_chose_roots_the_sessions_they_make_next() {
    let root = workspace_dir();
    let here = workspace_in(root.path(), "here");
    let atlas = workspace_in(root.path(), "atlas");
    let mut application = connected_application(&here);

    open_picker_with(&mut application, vec![rooted("Newer", &atlas, 30)]);
    press(&mut application, KeyCode::Down);
    choose(&mut application);

    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Initial Prompt".to_owned(),
        )))
        .expect("type an initial Prompt");
    let ApplicationTransition::CreateSession(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit the initial Prompt")
    else {
        panic!("a Landing submission creates a Session");
    };
    assert_eq!(request.workspace.path, atlas);
}

/// The Skills on offer are the picked Workspace's: the catalog the client was
/// holding answers for a Workspace the reader has left, and the one answered
/// for where they are now is what the composer completes from.
#[test]
fn the_skill_catalog_answers_for_the_workspace_the_reader_chose() {
    let root = workspace_dir();
    let here = workspace_in(root.path(), "here");
    let atlas = workspace_in(root.path(), "atlas");
    let mut application = application_choosing_skills(&here);
    load_skills(&mut application, &here, "review");

    type_terminal_text(&mut application, "$rev");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("$review"),
        "the Workspace the client launched in answers for its own Skills"
    );

    open_picker_with(&mut application, vec![rooted("Newer", &atlas, 30)]);
    press(&mut application, KeyCode::Down);
    choose(&mut application);

    type_terminal_text(&mut application, "$rev");
    assert!(
        !rendered_application_rows(&application)
            .join("\n")
            .contains("$review"),
        "the catalog the client held answers for a Workspace the reader has left"
    );

    load_skills(&mut application, &atlas, "revise");
    assert!(
        rendered_application_rows(&application)
            .join("\n")
            .contains("$revise"),
        "and the Skill Catalog the client now asks for is the picked Workspace's"
    );
}

/// "Where I am" is one idea across the client: the session picker's own
/// narrowing follows the reader to the Workspace they chose.
#[test]
fn the_session_pickers_current_workspace_scope_comes_to_mean_the_chosen_workspace() {
    let root = workspace_dir();
    let here = workspace_in(root.path(), "here");
    let atlas = workspace_in(root.path(), "atlas");
    let mut application = connected_application(&here);

    open_picker_with(&mut application, vec![rooted("Newer", &atlas, 30)]);
    press(&mut application, KeyCode::Down);
    choose(&mut application);

    assert_eq!(
        session_picker_scope(&mut application),
        SessionListScope::CurrentWorkspace(atlas),
        "the picker's current-Workspace scope means the Workspace the reader chose"
    );
}

/// A path typed at the Sidebar's entry is read from the Workspace the reader
/// is working in, and after a switch that is the Workspace they chose.
#[test]
fn a_relative_path_at_the_sidebars_entry_reads_from_the_chosen_workspace() {
    let root = workspace_dir();
    let atlas = root.path().join("atlas");
    let notes = atlas.join("notes");
    std::fs::create_dir_all(&notes).expect("create the directories the reader moves between");
    let mut application = connected_application(root.path());
    show_sidebar(&mut application, Vec::new());

    open_picker_with(&mut application, vec![rooted("Newer", &atlas, 30)]);
    press(&mut application, KeyCode::Down);
    choose(&mut application);

    add_workspace(&mut application, "notes");

    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Initial Prompt".to_owned(),
        )))
        .expect("type an initial Prompt");
    let ApplicationTransition::CreateSession(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit the initial Prompt")
    else {
        panic!("a Landing submission creates a Session");
    };
    assert_eq!(
        request.workspace.path, notes,
        "the relative path was read from the Workspace the reader chose in the picker, \
         not from the one the client was launched in"
    );
}

/// The one thing a switch leaves alone. The Sidebar's scope is a view the
/// reader configured, and switching Workspaces is navigation rather than
/// narrowing, so the column goes on showing what they asked it to show.
#[test]
fn the_sidebars_chosen_scope_is_left_where_the_reader_put_it() {
    let root = workspace_dir();
    let here = workspace_in(root.path(), "here");
    let atlas = workspace_in(root.path(), "atlas");
    let mut application = connected_application(&here);
    show_sidebar(
        &mut application,
        vec![
            rooted("Work where I am", &here, 20),
            rooted("Work elsewhere", &atlas, 30),
        ],
    );

    open_picker_with(
        &mut application,
        vec![
            rooted("Work where I am", &here, 20),
            rooted("Work elsewhere", &atlas, 30),
        ],
    );
    press(&mut application, KeyCode::Down);
    choose(&mut application);

    let rows = rendered_application_rows_at(&application, SIDEBAR_WIDE, 20);
    assert_eq!(
        selector_label(&rows),
        "▸ All Workspaces",
        "the switch left the Sidebar answering for the whole body of work: {rows:?}"
    );
    assert!(
        drawn_in_sidebar(&rows, "Work where I am"),
        "including the Sessions of the Workspace the reader left: {rows:?}"
    );
    assert!(drawn_in_sidebar(&rows, "Work elsewhere"), "{rows:?}");
}

/// Leaving an open Session is plain navigation: nothing is asked of the
/// reader, nothing is asked of the server about the Turn, and the Session goes
/// on working where it stands.
#[test]
fn an_open_session_is_left_working_and_listed_and_nothing_is_asked() {
    let root = workspace_dir();
    let atlas = workspace_in(root.path(), "atlas");
    let mut application = connected_application(root.path());
    let (session_id, ..) = enter_active_session(&mut application, root.path());
    show_sidebar(
        &mut application,
        vec![working("Long-running work", session_id, root.path())],
    );

    open_picker_with(&mut application, vec![rooted("Newer", &atlas, 30)]);
    press(&mut application, KeyCode::Down);

    assert_eq!(
        choose(&mut application),
        ApplicationTransition::DetachSession,
        "the client stops watching the Session; the Turn is neither interrupted \
         nor confirmed away"
    );

    let rows = rendered_application_rows_at(&application, SIDEBAR_WIDE, 20);
    let frame = rows.join("\n");
    assert!(
        frame.contains("What would you like to work on?"),
        "the Landing stands where the Session was, with nothing asked in between: {frame}"
    );
    assert!(
        drawn_in_sidebar(&rows, "Long-running work"),
        "the Session the reader left is still listed: {rows:?}"
    );
    assert!(
        sidebar_text(&rows).contains("Working"),
        "and it is still working: {rows:?}"
    );
}

/// The command doubles as "take me to a fresh start here": choosing the
/// Workspace the client is already in opens the Landing rather than doing
/// nothing.
#[test]
fn choosing_the_workspace_the_client_is_already_in_opens_the_landing() {
    let root = workspace_dir();
    let here = workspace_in(root.path(), "here");
    let atlas = workspace_in(root.path(), "atlas");
    let mut application = connected_application(&here);

    open_picker_with(&mut application, vec![rooted("Newer", &atlas, 30)]);
    assert_eq!(
        selected_row(&application),
        "here",
        "the picker opens on the Workspace the reader is in"
    );

    assert_eq!(choose(&mut application), ApplicationTransition::Continue);

    let landing = rendered_application_rows_at(&application, 120, 20).join("\n");
    assert!(
        !landing.contains("Workspaces"),
        "the picker is done with: {landing}"
    );
    assert!(
        landing.contains("What would you like to work on?"),
        "{landing}"
    );
    assert!(
        landing.contains(here.to_string_lossy().as_ref()),
        "and the Workspace is where it was: {landing}"
    );
}

/// Enter before the listing lands names no Workspace, so it chooses none: the
/// picker stands on its loading line rather than switching to whatever row
/// would have stood first.
#[test]
fn enter_while_the_listing_is_on_its_way_chooses_nothing() {
    let here = workspace(&["work", "here"]);
    let mut application = connected_application(&here);
    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::WorkspaceList,
        )))
        .expect("open the Workspace Picker");

    assert_eq!(choose(&mut application), ApplicationTransition::Continue);

    let picker = rendered_application_rows(&application).join("\n");
    assert!(picker.contains("Loading Workspaces"), "{picker}");
}

/// Typing narrows the picker the way it narrows the session picker: the
/// characters stand in order but need not stand together, and the case the
/// reader types in is never the case they have to type in.
#[test]
fn typing_narrows_the_rows_by_fuzzy_name_match() {
    let mut application = connected_application(&workspace(&["work", "here"]));
    open_picker_with(&mut application, narrowable_sessions());

    type_terminal_text(&mut application, "ENE");
    assert_eq!(picker_query(&application), "ENE");

    let rows = picker_rows(&application);
    assert_eq!(
        rows.len(),
        1,
        "only the Workspace carrying it stands: {rows:?}"
    );
    assert!(
        rows[0].contains("engine"),
        "the characters stand in order without standing together, and the case \
         the reader reached for is not the case they have to type: {rows:?}"
    );
}

/// Narrowing takes rows away and never rearranges the ones it leaves: the
/// current Workspace goes on standing first, and the rest go on standing by
/// which held work most recently.
#[test]
fn the_rows_a_query_leaves_keep_the_picker_s_order() {
    let mut application = connected_application(&workspace(&["work", "here"]));
    open_picker_with(&mut application, narrowable_sessions());

    type_terminal_text(&mut application, "e");

    let rows = picker_rows(&application);
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert!(
        rows[0].contains("here") && rows[0].contains("[current]"),
        "the Workspace the client works in still stands first: {rows:?}"
    );
    assert!(
        rows[1].contains("engine"),
        "then the Workspace whose newest Session is newest: {rows:?}"
    );
    assert!(rows[2].contains("ledger"), "{rows:?}");
}

/// A row spells its Workspace's path beside the name, but the name is what a
/// query is read against — as a Title is in the session picker — so the
/// directories a Workspace stands under never answer for it.
#[test]
fn a_query_is_read_against_the_name_rather_than_the_path() {
    let mut application = connected_application(&workspace(&["work", "here"]));
    open_picker_with(
        &mut application,
        vec![rooted("Older", &workspace(&["zephyr", "atlas"]), 10)],
    );

    type_terminal_text(&mut application, "zephyr");

    let rows = picker_rows(&application);
    assert_eq!(
        rows,
        vec!["No Workspaces found"],
        "the head of the path is not part of the name: {rows:?}"
    );
}

#[test]
fn backspace_widens_the_rows_and_giving_the_query_up_restores_them_all() {
    let mut application = connected_application(&workspace(&["work", "here"]));
    open_picker_with(&mut application, narrowable_sessions());

    type_terminal_text(&mut application, "eng");
    assert_eq!(picker_rows(&application).len(), 1, "narrowed to one");

    backspace(&mut application);
    backspace(&mut application);
    assert_eq!(picker_query(&application), "e");
    let widened = picker_rows(&application);
    assert_eq!(
        widened.len(),
        3,
        "one character back widens again: {widened:?}"
    );

    backspace(&mut application);
    assert_eq!(picker_query(&application), "");
    let restored = picker_rows(&application);
    assert_eq!(
        restored.len(),
        4,
        "and giving the query up puts every Workspace back: {restored:?}"
    );
    assert!(
        restored[0].contains("here") && restored[3].contains("ledger"),
        "{restored:?}"
    );
}

/// A query nothing carries says so, rather than leaving a box that reads as
/// having no Workspaces at all.
#[test]
fn a_query_no_workspace_carries_says_so() {
    let mut application = connected_application(&workspace(&["work", "here"]));
    open_picker_with(&mut application, narrowable_sessions());

    type_terminal_text(&mut application, "qqq");

    let picker = rendered_application_rows(&application).join("\n");
    assert!(picker.contains("No Workspaces found"), "{picker}");
    assert!(picker.contains("Search: qqq"), "{picker}");
    assert!(
        !picker.contains("engine"),
        "no row survives the query: {picker}"
    );
}

/// Narrowing never leaves the reader on a row that is not there: they keep the
/// Workspace they were on while the query still carries it, and land on the
/// first row that survives when it does not.
#[test]
fn the_reader_is_left_on_a_workspace_the_query_still_offers() {
    let mut application = connected_application(&workspace(&["work", "here"]));
    open_picker_with(&mut application, narrowable_sessions());

    press(&mut application, KeyCode::Down);
    assert_eq!(selected_row(&application), "engine");

    type_terminal_text(&mut application, "l");
    assert_eq!(
        selected_row(&application),
        "atlas",
        "the Workspace they were on is gone, so the first row that stands takes them"
    );

    backspace(&mut application);
    assert_eq!(
        selected_row(&application),
        "atlas",
        "and widening leaves them where they are, because it is still offered"
    );
}

/// A Title carried in from somewhere else is as good a way to find a Workspace
/// as one the reader types out, as it is in the session picker.
#[test]
fn a_paste_goes_into_the_query_whole() {
    let mut application = connected_application(&workspace(&["work", "here"]));
    open_picker_with(&mut application, narrowable_sessions());

    application
        .handle_terminal_event(InputEvent::Paste("ledger".to_owned()))
        .expect("paste into the Workspace Picker");

    assert_eq!(picker_query(&application), "ledger");
    let rows = picker_rows(&application);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(rows[0].contains("ledger"), "{rows:?}");
}

/// A query belongs to the look the reader was taking: closing the picker ends
/// that look, so opening it again offers every Workspace afresh.
#[test]
fn opening_the_picker_again_starts_from_no_query() {
    let mut application = connected_application(&workspace(&["work", "here"]));
    open_picker_with(&mut application, narrowable_sessions());
    type_terminal_text(&mut application, "eng");
    press(&mut application, KeyCode::Esc);

    open_picker_with(&mut application, narrowable_sessions());

    assert_eq!(picker_query(&application), "");
    let rows = picker_rows(&application);
    assert_eq!(rows.len(), 4, "every Workspace is on offer again: {rows:?}");
    assert!(
        rows[0].contains("here") && rows[0].contains("[current]"),
        "and the reader is back on the Workspace they work in: {rows:?}"
    );
}

/// The picker stays a searchable one down to the small terminals it supports:
/// the line saying what the reader has typed holds its place beside the rows
/// it narrows, so a query is never applied invisibly.
#[test]
fn the_picker_stays_searchable_on_the_small_terminals_it_supports() {
    let mut application = connected_application(&workspace(&["work", "here"]));
    open_picker_with(&mut application, narrowable_sessions());
    type_terminal_text(&mut application, "l");

    for (width, height) in [(43, 10), (28, 9)] {
        let rows = picker_rows_at(&application, width, height);
        let picker = rendered_application_rows_at(&application, width, height).join("\n");
        assert!(
            picker.contains("Search: l"),
            "the query stands beside the rows it narrows at {width}x{height}: {picker}"
        );
        assert!(
            rows.iter().any(|row| row.contains("atlas"))
                && rows.iter().any(|row| row.contains("ledger")),
            "and the rows it leaves are still drawn at {width}x{height}: {rows:?}"
        );
    }
}

/// A query can empty the picker as surely as a listing still on its way can,
/// and Enter answers the same either way: with no row the reader is on there
/// is nothing to choose, so the picker stands rather than switching to
/// whatever would have stood first.
#[test]
fn enter_on_a_query_no_workspace_carries_chooses_nothing() {
    let mut application = connected_application(&workspace(&["work", "here"]));
    open_picker_with(&mut application, narrowable_sessions());
    type_terminal_text(&mut application, "qqq");

    assert_eq!(choose(&mut application), ApplicationTransition::Continue);

    let picker = rendered_application_rows(&application).join("\n");
    assert!(
        picker.contains("No Workspaces found"),
        "the picker stands on the query that emptied it: {picker}"
    );
    assert!(
        picker.contains("Search: qqq"),
        "and the query the reader typed is still theirs: {picker}"
    );
}

/// The Workspaces the narrowing tests narrow: the client works in `here`, and
/// the rest stand by which held work most recently — `engine`, `atlas`, then
/// `ledger`.
fn narrowable_sessions() -> Vec<SessionListItem> {
    vec![
        rooted("Older", &workspace(&["work", "ledger"]), 10),
        rooted("Newest", &workspace(&["work", "engine"]), 30),
        rooted("Newer", &workspace(&["work", "atlas"]), 20),
    ]
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

/// A real Workspace beneath a fixture root, returned in the same canonical
/// spelling the client and server use. Tests that choose a row use real
/// directories because choosing re-reads the path at that moment.
fn workspace_in(root: &Path, name: &str) -> PathBuf {
    let workspace = root.join(name);
    std::fs::create_dir(&workspace).expect("create the Workspace fixture");
    std::fs::canonicalize(workspace).expect("canonicalize the Workspace fixture")
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
            working_since: None,
            parent: None,
        },
        title: title.to_owned(),
        emoji: None,
        settled_at: None,
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

/// Opens the Session Picker and returns the scope its listing asks for, which
/// is the observable reading of what current-Workspace scope means.
fn session_picker_scope(application: &mut Application) -> SessionListScope {
    let ApplicationTransition::ListSessions(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::SessionList,
        )))
        .expect("open the Session Picker")
    else {
        panic!("opening the Session Picker asks for its Sessions");
    };
    assert_eq!(request.surface(), SessionListSurface::SessionPicker);
    request.scope().clone()
}

fn press(application: &mut Application, code: KeyCode) {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
        .expect("drive the Workspace Picker");
}

/// The picker's own rows, taken from the frame between the search line and the
/// footer and trimmed of the box that draws them.
fn picker_rows(application: &Application) -> Vec<String> {
    picker_rows_at(application, 80, 15)
}

fn picker_rows_at(application: &Application, width: u16, height: u16) -> Vec<String> {
    let rows = rendered_application_rows_at(application, width, height);
    let search = rendered_row(&rows, "Search:");
    let footer = rows
        .iter()
        .position(|row| {
            row.contains("Esc")
                || row.contains("No directory there")
                || row.contains("Not a directory")
        })
        .expect("the picker draws its footer");
    rows[search + 1..footer]
        .iter()
        .map(|row| row.trim_matches(['│', ' ']).to_owned())
        .filter(|row| !row.is_empty())
        .collect()
}

/// What the picker says the reader has typed, read off its search line.
fn picker_query(application: &Application) -> String {
    let rows = rendered_application_rows(application);
    rows[rendered_row(&rows, "Search:")]
        .trim_matches(['│', ' '])
        .trim_start_matches("Search:")
        .trim()
        .to_owned()
}

fn backspace(application: &mut Application) {
    press(application, KeyCode::Backspace);
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

/// Enter on the row the reader is on, which is how a Workspace is chosen.
fn choose(application: &mut Application) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("choose the Workspace the reader is on")
}

/// A listed Session mid-Turn, named by the Session it stands for so a frame
/// can be read for the Session the reader left rather than for any row. Its
/// times are read off the clock, because a Sidebar shelves a Session by how
/// long ago it last moved.
fn working(title: &str, session_id: SessionId, workspace: &Path) -> SessionListItem {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("read the clock")
        .as_millis()
        .try_into()
        .expect("the clock fits a Session timestamp");
    let SessionListItem::Readable(mut summary) = rooted(title, workspace, now) else {
        unreachable!("the fixture builds a readable Session");
    };
    summary.session.id = session_id;
    summary.session.status = SessionStatus::Active;
    summary.session.working_since = Some(SessionTimestamp(now.saturating_sub(90 * 1_000)));
    SessionListItem::Readable(summary)
}

/// A client whose Landing has an Agent chosen, which is what it takes for a
/// Skill Catalog to be asked for at all.
fn application_choosing_skills(workspace: &Path) -> Application {
    let mut application = Application::new(workspace);
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
            ready_health(fixture_instance_id(), 42_424).with_landing_agent_selection(Some(
                AgentSelection {
                    provider: ProviderId::new("codex"),
                    model: ModelId::new("gpt-fixture"),
                    options: Vec::new(),
                },
            )),
        )))
        .expect("connect a client with an Agent chosen");
    application
}

/// The Skill Catalog the server answers with for one Workspace, holding the
/// one Skill named — which is how a frame says which Workspace the catalog on
/// offer answers for.
fn load_skills(application: &mut Application, workspace: &Path, skill: &str) {
    let request = SkillCatalogRequest {
        provider: ProviderId::new("codex"),
        workspace: Workspace {
            path: workspace.to_owned(),
        },
    };
    application
        .handle_event(ApplicationEvent::SkillsListed {
            request: request.clone(),
            catalog: SkillCatalog {
                provider: request.provider.clone(),
                workspace: request.workspace.clone(),
                skills: vec![SkillDescriptor {
                    id: SkillId::new(skill),
                    name: skill.to_owned(),
                    description: format!("{skill} the current change"),
                    scope: Some("Workspace".to_owned()),
                }],
                capabilities: SkillCatalogCapabilities {
                    max_distinct_invocations: None,
                    supported_deliveries: vec![SkillPromptDelivery::Initial],
                },
                status: SkillCatalogStatus::Fresh { warning: None },
            },
        })
        .expect("load the Skill Catalog");
}

/// The Sidebar on screen and answered with `sessions`, which is what it takes
/// to read the scope it stands under or the Sessions it lists.
fn show_sidebar(application: &mut Application, sessions: Vec<SessionListItem>) {
    let transition = deliver_settings(
        application,
        EffectiveSettings {
            sidebar: SidebarSettings {
                initial_visibility: SidebarVisibility::Shown,
                ..SidebarSettings::default()
            },
            ..EffectiveSettings::default()
        },
    );
    let ApplicationTransition::ListSessions(request) = transition else {
        panic!("a Sidebar coming into view asks for its Sessions, not {transition:?}");
    };
    assert_eq!(request.surface(), SessionListSurface::Sidebar);
    application
        .handle_event(ApplicationEvent::SessionsListed { request, sessions })
        .expect("hydrate the Sidebar");
}

fn sidebar_text(rows: &[String]) -> String {
    rows.iter()
        .map(|row| sidebar_column(row))
        .collect::<Vec<_>>()
        .join("\n")
}
