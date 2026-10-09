//! The Workspace Picker: opening it, the Workspaces it puts on offer, the
//! order it stands them in, narrowing them by typing, choosing one, and
//! walking away from it unchanged.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use crate::support::{
    SIDEBAR_WIDE, answer_workspace_resolution, application_looking_at_studio,
    connected_application, deliver_settings, drawn_in_sidebar, enter_active_session,
    fixture_instance_id, model_descriptor, noncanonical_spelling, ready_health,
    rendered_application_buffer, rendered_application_rows, rendered_application_rows_at,
    rendered_row, selected_session_snapshot, selector_label, sidebar_column,
    studio_stops_answering, text_position, type_terminal_text, workspace_dir, workspace_resolution,
};
use crossterm::event::{
    Event as InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use suru::{
    managed_client::ManagedEvent,
    protocol::{
        AgentSelection, ChildDirectory, DirectoryListing, DirectorySourceControl,
        EffectiveSettings, ListDirectoryRequest, ModelAvailability, ModelCatalog, ModelId,
        ProviderCatalogStatus, ProviderId, ProviderModelCatalog, ResolvedWorkspace, Session,
        SessionId, SessionListItem, SessionStatus, SessionSummary, SessionTimestamp,
        SidebarSettings, SidebarVisibility, SkillCatalog, SkillCatalogCapabilities,
        SkillCatalogRequest, SkillCatalogStatus, SkillDescriptor, SkillId, SkillPromptDelivery,
        Workspace,
    },
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, DirectoryListingId,
        SemanticCommandId, SessionListRequest, SessionListScope, SessionListSurface, TerminalFacts,
        WorkspaceResolutionSurface,
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

/// The Sidekick Workspace holds no body of work to choose among — `/sidekick`
/// is the way into it — so the picker leaves it out of the Workspaces the
/// reader is not in, however recently a Sidekick worked there.
#[test]
fn the_sidekick_workspace_is_not_offered_where_the_reader_is_not_in_it() {
    let here = workspace(&["work", "here"]);
    let sidekick = workspace(&["data", "sidekick"]);
    let other = workspace(&["two", "api"]);
    let mut application = connected_application(&here);
    name_sidekick_workspace(&mut application, &sidekick);

    open_picker_with(
        &mut application,
        vec![rooted("Survey", &sidekick, 30), rooted("Older", &other, 20)],
    );

    let rows = picker_rows(&application);
    assert!(
        !rows.iter().any(|row| row.contains("Sidekick")),
        "the Sidekick Workspace is not offered: {rows:?}"
    );
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(
        rows[1].contains(other.to_string_lossy().as_ref()),
        "every other Workspace is still offered: {rows:?}"
    );
}

/// Searching cannot bring the Sidekick Workspace back: the query narrows the
/// Workspaces on offer and never widens them.
#[test]
fn searching_for_sidekick_does_not_offer_the_sidekick_workspace() {
    let here = workspace(&["work", "here"]);
    let sidekick = workspace(&["data", "sidekick"]);
    let mut application = connected_application(&here);
    name_sidekick_workspace(&mut application, &sidekick);
    open_picker_with(&mut application, vec![rooted("Survey", &sidekick, 30)]);

    type_terminal_text(&mut application, "sidekick");

    let rows = picker_rows(&application);
    assert!(
        !rows.iter().any(|row| row.contains("Sidekick")),
        "the Sidekick Workspace is not offered: {rows:?}"
    );
}

/// Where the reader is in the Sidekick Workspace it stands first as any
/// current Workspace does, so they see where they are and its Icon and
/// Description stay theirs to change; its directory lies with the Server's
/// data, so its row goes by **Sidekick** alone.
#[test]
fn the_sidekick_workspace_stands_first_by_name_alone_where_the_reader_is_in_it() {
    let sidekick = workspace(&["data", "sidekick"]);
    let other = workspace(&["two", "api"]);
    let mut application = connected_application(&sidekick);
    name_sidekick_workspace(&mut application, &sidekick);

    open_picker_with(
        &mut application,
        vec![rooted("Survey", &sidekick, 30), rooted("Older", &other, 20)],
    );

    let rows = picker_rows(&application);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0], "› Sidekick · [current]", "{rows:?}");
    assert!(
        rows[1].contains(other.to_string_lossy().as_ref()),
        "every other Workspace still spells its path: {rows:?}"
    );
}

/// A Workspace Picker row draws the Workspace's own Icon in place of the
/// folder glyph where one has been derived, the folder glyph while it has
/// none, and neither with Icons off.
#[test]
fn a_row_draws_the_workspaces_own_icon_in_place_of_the_folder_glyph() {
    let here = workspace(&["work", "here"]);
    let iconed = workspace(&["group", "iconed-ws"]);
    let plain = workspace(&["group", "plain-ws"]);
    let mut application = connected_application(&here);

    let mut iconed_session = rooted("Newer", &iconed, 30);
    let SessionListItem::Readable(summary) = &mut iconed_session else {
        unreachable!()
    };
    summary.session.workspace.icon = Some("dev-rust".to_owned());

    let mut settings = EffectiveSettings::default();
    settings.appearance.show_icons = false;
    deliver_settings(&mut application, settings);
    open_picker_with(
        &mut application,
        vec![iconed_session.clone(), rooted("Older", &plain, 20)],
    );
    let rows = picker_rows(&application);
    assert!(
        !rows
            .iter()
            .any(|row| row.contains('\u{ea83}') || row.contains('\u{e7a8}')),
        "no Icon is drawn while the reader keeps Icons off: {rows:?}"
    );

    let mut settings = EffectiveSettings::default();
    settings.appearance.show_icons = true;
    deliver_settings(&mut application, settings);
    open_picker_with(
        &mut application,
        vec![iconed_session, rooted("Older", &plain, 20)],
    );
    let rows = picker_rows(&application);
    let iconed_row = rows
        .iter()
        .find(|row| row.contains("iconed-ws"))
        .expect("the iconed row is offered");
    assert!(
        iconed_row.contains('\u{e7a8}'),
        "the Workspace's own derived Icon replaces the folder glyph: {rows:?}"
    );
    let plain_row = rows
        .iter()
        .find(|row| row.contains("plain-ws"))
        .expect("the plain row is offered");
    assert!(
        plain_row.contains('\u{ea83}'),
        "the folder glyph stands in for a Workspace with no derived Icon: {rows:?}"
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
        "Browse",
        "walking past the last row comes back to the first, the Browse row"
    );
    press(&mut application, KeyCode::Down);
    assert_eq!(selected_row(&application), "here");
    press(&mut application, KeyCode::Up);
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

/// The frames the picker's pointer tests are drawn in: roomy, then shorter and
/// narrower, each moving where the box and its rows stand and holding fewer
/// rows in its list window.
const PICKER_FRAMES: [(u16, u16); 3] = [(120, 20), (80, 15), (60, 12)];

/// A left press on a row chooses the Workspace it names exactly as Enter on
/// that row does: the same request goes to its Server, and the same Landing
/// stands in the picker's place before and after the answer.
#[test]
fn a_left_press_on_a_row_chooses_it_exactly_as_enter_does() {
    for frame in PICKER_FRAMES {
        let root = workspace_dir();
        let here = workspace_in(root.path(), "here");
        let atlas = workspace_in(root.path(), "atlas");
        let picking = || {
            let mut application = connected_application(&here);
            open_picker_with(&mut application, vec![rooted("Newer", &atlas, 30)]);
            application
        };

        let mut keyed = picking();
        press(&mut keyed, KeyCode::Down);
        let entered = choose_unanswered(&mut keyed);

        let mut pointed = picking();
        assert_eq!(
            selected_row(&pointed),
            "here",
            "the press lands on a row the reader is not on"
        );
        let pressed = press_row(&mut pointed, frame, MouseButton::Left, "atlas");

        assert!(
            matches!(
                pressed,
                ApplicationTransition::DetachSessionAndResolveWorkspace { .. }
            ),
            "{frame:?}: {pressed:?}"
        );
        assert_eq!(
            pressed, entered,
            "{frame:?}: the press asks what Enter asks"
        );
        assert_eq!(
            rendered_application_rows_at(&pointed, frame.0, frame.1),
            rendered_application_rows_at(&keyed, frame.0, frame.1),
            "{frame:?}: and the same Landing stands in the picker's place"
        );

        answer_workspace_resolution(&mut keyed, entered);
        answer_workspace_resolution(&mut pointed, pressed);
        assert_eq!(
            rendered_application_rows_at(&pointed, frame.0, frame.1),
            rendered_application_rows_at(&keyed, frame.0, frame.1),
            "{frame:?}: and the same Landing once the Server has answered"
        );
    }
}

/// A row the list window has scrolled into view is chosen by a press where it
/// is drawn now, not where it would stand unscrolled.
#[test]
fn a_left_press_on_a_row_scrolled_into_view_chooses_that_row() {
    const NAMES: [&str; 12] = [
        "alder", "birch", "cedar", "dogwood", "elm", "fir", "ginkgo", "hazel", "ironwood",
        "juniper", "larch", "maple",
    ];
    for frame in PICKER_FRAMES {
        let root = workspace_dir();
        let here = workspace_in(root.path(), "here");
        let listed = NAMES
            .iter()
            .zip(0..)
            .map(|(name, age)| rooted(name, &workspace_in(root.path(), name), 100 - age))
            .collect::<Vec<_>>();
        let picking = || {
            let mut application = connected_application(&here);
            open_picker_with(&mut application, listed.clone());
            application
        };
        // The last row but one, which the window holds only once the reader
        // has walked past its end — past the Browse row above the current
        // Workspace, which is where walking up from it goes first.
        let target = NAMES[NAMES.len() - 2];

        let mut keyed = picking();
        for _ in 0..3 {
            press(&mut keyed, KeyCode::Up);
        }
        assert_eq!(selected_row_at(&keyed, frame), target, "{frame:?}");
        let entered = choose_unanswered(&mut keyed);

        let mut pointed = picking();
        let unscrolled = rendered_application_rows_at(&pointed, frame.0, frame.1).join("\n");
        assert!(
            !unscrolled.contains(target),
            "{frame:?}: the window does not hold {target} before it scrolls: {unscrolled}"
        );
        press(&mut pointed, KeyCode::Up);
        press(&mut pointed, KeyCode::Up);
        let pressed = press_row(&mut pointed, frame, MouseButton::Left, target);

        assert!(
            matches!(
                pressed,
                ApplicationTransition::DetachSessionAndResolveWorkspace { .. }
            ),
            "{frame:?}: {pressed:?}"
        );
        assert_eq!(pressed, entered, "{frame:?}: the press chooses {target}");
    }
}

/// While an Agent Selection update is still on its way to its Server, Enter on
/// a row waits rather than race it, and a press on the row waits exactly as
/// Enter does: nothing is asked, and the picker stands as Enter leaves it.
#[test]
fn a_left_press_on_a_row_waits_out_a_pending_agent_selection_as_enter_does() {
    for frame in PICKER_FRAMES {
        let root = workspace_dir();
        let here = workspace_in(root.path(), "here");
        let atlas = workspace_in(root.path(), "atlas");
        let picking = || {
            let mut application = application_awaiting_agent_selection(&here);
            open_picker_with(&mut application, vec![rooted("Newer", &atlas, 30)]);
            application
        };

        let mut keyed = picking();
        press(&mut keyed, KeyCode::Down);
        let entered = choose_unanswered(&mut keyed);
        let mut pointed = picking();
        let pressed = press_row(&mut pointed, frame, MouseButton::Left, "atlas");

        assert_eq!(
            entered,
            ApplicationTransition::Continue,
            "{frame:?}: Enter waits for the Agent Selection"
        );
        assert_eq!(pressed, entered, "{frame:?}: and so does the press");
        assert_eq!(
            rendered_application_rows_at(&pointed, frame.0, frame.1),
            rendered_application_rows_at(&keyed, frame.0, frame.1),
            "{frame:?}"
        );
    }
}

/// A Workspace the picker offers is resolved on the Server the Outlook is
/// turned toward, so once that Remote stops answering Enter on a row is
/// refused, saying why, and a press on the row is refused exactly as Enter is.
#[test]
fn a_left_press_on_a_row_is_refused_for_an_unreachable_remote_as_enter_is() {
    for frame in PICKER_FRAMES {
        let picking = || {
            let mut application = application_looking_at_studio();
            open_picker_with(
                &mut application,
                vec![rooted(
                    "Remote work",
                    &crate::support::named_workspace_path("atlas"),
                    30,
                )],
            );
            studio_stops_answering(&mut application, 1, Duration::from_secs(5));
            application
        };

        let mut keyed = picking();
        press(&mut keyed, KeyCode::Down);
        let entered = choose_unanswered(&mut keyed);
        let mut pointed = picking();
        let pressed = press_row(&mut pointed, frame, MouseButton::Left, "atlas");

        assert_eq!(
            entered,
            ApplicationTransition::Continue,
            "{frame:?}: Enter is refused"
        );
        assert_eq!(pressed, entered, "{frame:?}: and so is the press");
        assert_eq!(
            rendered_application_rows_at(&pointed, frame.0, frame.1),
            rendered_application_rows_at(&keyed, frame.0, frame.1),
            "{frame:?}"
        );
        let lines = picker_lines(&pointed);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("studio is unreachable")),
            "{frame:?}: the refusal is said inside the picker, which covers the Landing: {lines:?}"
        );
        press(&mut pointed, KeyCode::Esc);
        let refused = rendered_application_rows_at(&pointed, frame.0, frame.1).join("\n");
        assert!(
            refused.contains("studio is unreachable"),
            "{frame:?}: the refusal is said: {refused}"
        );
    }
}

/// A right press on a row still opens that row's own menu rather than
/// choosing it, leaving the picker standing beneath the menu.
#[test]
fn a_right_press_on_a_row_still_opens_its_menu() {
    for frame in PICKER_FRAMES {
        let here = workspace(&["work", "here"]);
        let ledger = workspace(&["work", "ledger"]);
        let mut application = connected_application(&here);
        open_picker_with(&mut application, vec![rooted("Older", &ledger, 10)]);

        assert_eq!(
            press_row(&mut application, frame, MouseButton::Right, "ledger"),
            ApplicationTransition::Continue,
            "{frame:?}: nothing is chosen"
        );

        let drawn = rendered_application_rows_at(&application, frame.0, frame.1).join("\n");
        assert!(drawn.contains("Edit description"), "{frame:?}: {drawn}");
        assert!(drawn.contains(" Workspaces "), "{frame:?}: {drawn}");
    }
}

/// A press inside the picker that lands on no row — its search line — chooses
/// nothing and leaves the picker standing.
#[test]
fn a_press_inside_the_picker_off_its_rows_moves_nothing() {
    for frame in PICKER_FRAMES {
        let here = workspace(&["work", "here"]);
        let ledger = workspace(&["work", "ledger"]);
        let mut application = connected_application(&here);
        open_picker_with(&mut application, vec![rooted("Older", &ledger, 10)]);
        let before = rendered_application_rows_at(&application, frame.0, frame.1);

        assert_eq!(
            press_row(&mut application, frame, MouseButton::Left, "Search:"),
            ApplicationTransition::Continue,
            "{frame:?}"
        );
        assert_eq!(
            rendered_application_rows_at(&application, frame.0, frame.1),
            before,
            "{frame:?}"
        );
    }
}

/// The whole of the switch: Enter on a row takes the reader out of the picker
/// and puts them on the Landing of the Workspace they chose, ready to write
/// the first Prompt of a Session rooted there. It does so at once: resolving a
/// Workspace can take its Server seconds, and a reader who has chosen has
/// already said where they are going, so the Landing names the Workspace
/// chosen while the Server works out the rest.
#[test]
fn enter_closes_the_picker_and_lands_in_the_workspace_chosen_before_its_server_answers() {
    let root = workspace_dir();
    let here = workspace_in(root.path(), "here");
    let atlas = workspace_in(root.path(), "atlas");
    let mut application = connected_application(&here);

    open_picker_with(&mut application, vec![rooted("Newer", &atlas, 30)]);
    press(&mut application, KeyCode::Down);
    assert_eq!(selected_row(&application), "atlas");

    let ApplicationTransition::DetachSessionAndResolveWorkspace { request, .. } =
        choose_unanswered(&mut application)
    else {
        panic!(
            "moving Workspace opens the Landing, which is the client letting go of whatever \
             it was on or on its way to, and asks the Server to resolve the Workspace chosen"
        );
    };
    assert_eq!(request.path, atlas);
    assert_eq!(
        request.workspace_id,
        Some(suru::protocol::WorkspaceId::directory(&atlas))
    );

    let landing = rendered_application_rows_at(&application, 120, 20).join("\n");
    assert!(
        !landing.contains("Workspaces"),
        "the picker is done with: {landing}"
    );
    assert!(
        landing.contains("Type a prompt"),
        "the Landing stands in its place: {landing}"
    );
    // The Landing's line is only as wide as its composer, so a long temporary
    // path gives up its front: the Workspace is known by the end it keeps.
    assert!(
        landing_location(&application).ends_with(&format!("atlas · {SPINNER}")),
        "and it stands in the Workspace the reader chose before the Server has answered: \
         {landing}"
    );
}

/// A Prompt written while the Landing waits on its Workspace is a Prompt for
/// that Workspace, which the client cannot yet say where to begin in. Enter
/// begins nothing until the Server has answered — the draft stands where the
/// reader wrote it, as it does in a Session still loading — and once it has,
/// the same draft begins its Session in the Workspace chosen.
#[test]
fn a_prompt_written_before_the_server_answers_waits_for_it() {
    let root = workspace_dir();
    let here = workspace_in(root.path(), "here");
    let atlas = workspace_in(root.path(), "atlas");
    let mut application = connected_application(&here);

    open_picker_with(&mut application, vec![rooted("Newer", &atlas, 30)]);
    press(&mut application, KeyCode::Down);
    let resolution = choose_unanswered(&mut application);

    type_terminal_text(&mut application, "Initial Prompt");
    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
            .expect("submit before the Server answers"),
        ApplicationTransition::Continue,
        "no Session begins, in the Workspace left behind or in one not yet resolved"
    );
    let waiting = rendered_application_rows_at(&application, 120, 20).join("\n");
    assert!(
        waiting.contains("Initial Prompt"),
        "the draft stands where the reader wrote it: {waiting}"
    );
    assert!(
        waiting.contains("atlas is still resolving; this waits until its Server answers"),
        "and the Landing says why Enter began nothing, as work waiting on a Remote does: \
         {waiting}"
    );

    assert_eq!(
        answer_workspace_resolution(&mut application, resolution),
        ApplicationTransition::Continue,
        "the answer begins nothing on the reader's behalf"
    );
    let answered = rendered_application_rows_at(&application, 120, 20).join("\n");
    assert!(
        !answered.contains("still resolving"),
        "what the Landing was waiting on has arrived, so it no longer says so: {answered}"
    );
    let ApplicationTransition::CreateSession(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit once the Server has answered")
    else {
        panic!("a Landing submission creates a Session once its Workspace is resolved");
    };
    assert_eq!(request.execution_directory.path, atlas);
    assert_eq!(request.prompt.text, "Initial Prompt");
}

/// While its Server works the chosen Workspace out, the Landing says so where
/// the Checkout State will stand, with the Spinner a row being read carries,
/// and keeps it turning for as long as it is drawn; the answer puts the
/// Workspace's own reading in its place and lets the tick go.
#[test]
fn a_workspace_still_resolving_turns_a_spinner_where_its_checkout_state_will_stand() {
    let root = workspace_dir();
    let here = workspace_in(root.path(), "here");
    let atlas = workspace_in(root.path(), "atlas");
    let mut application = connected_application(&here);

    open_picker_with(&mut application, vec![rooted("Newer", &atlas, 30)]);
    press(&mut application, KeyCode::Down);
    let resolution = choose_unanswered(&mut application);

    let waiting = landing_location(&application);
    assert!(
        waiting.ends_with(&format!("atlas · {SPINNER}")),
        "the Landing names the Workspace chosen and that its reading is on its way: {waiting}"
    );
    assert!(
        application.wants_spinner(),
        "a Spinner on screen keeps the presentation tick armed"
    );

    answer_workspace_resolution(&mut application, resolution);
    let answered = landing_location(&application);
    assert!(
        answered.ends_with("atlas") && !answered.contains(SPINNER),
        "the answer takes the Spinner's place: {answered}"
    );
    assert!(
        !application.wants_spinner(),
        "and nothing left on screen animates"
    );
}

/// The Server's answer fills in what the Landing could not say before it, and
/// moves every reading of "where I am" with it, beneath a reader who has
/// already begun writing: the draft is theirs, and the answer is not a fresh
/// Landing.
#[test]
fn the_answer_fills_in_the_landing_beneath_the_draft_written_while_it_waited() {
    let root = workspace_dir();
    let here = workspace_in(root.path(), "here");
    let atlas = workspace_in(root.path(), "atlas");
    let mut application = connected_application(&here);

    open_picker_with(&mut application, vec![rooted("Newer", &atlas, 30)]);
    press(&mut application, KeyCode::Down);
    let resolution = choose_unanswered(&mut application);
    assert_eq!(
        session_picker_scope(&mut application),
        SessionListScope::CurrentWorkspace(here.clone().into()),
        "nothing but the Landing's naming moves before the Server answers"
    );
    press(&mut application, KeyCode::Esc);
    type_terminal_text(&mut application, "half a thought");

    assert_eq!(
        answer_workspace_resolution(&mut application, resolution),
        ApplicationTransition::Continue,
        "the Landing is already open, so the answer has no Session to let go of"
    );

    let landing = rendered_application_rows_at(&application, 120, 20).join("\n");
    assert!(
        landing.contains("half a thought"),
        "the draft written while the Landing waited survives its answer: {landing}"
    );
    assert!(
        landing_location(&application).ends_with("atlas"),
        "{landing}"
    );
    assert_eq!(
        session_picker_scope(&mut application),
        SessionListScope::CurrentWorkspace(atlas.into()),
        "and current-Workspace scope has come to mean the Workspace chosen"
    );
}

/// A Workspace offered from old work may have disappeared since the listing
/// was recorded. The reader is already on its Landing when the Server says
/// so, and the refusal is said there: the Landing goes back to naming the
/// Workspace the client still works in, because a refused choice moves
/// nothing, and the draft written meanwhile stays for the reader to send
/// there or take elsewhere.
#[test]
fn a_workspace_whose_directory_is_gone_is_refused_on_its_landing_and_moves_nothing() {
    let root = workspace_dir();
    let here = workspace_in(root.path(), "here");
    let gone = root.path().join("gone");
    let atlas = workspace_in(root.path(), "atlas");
    let mut application = connected_application(&here);

    open_picker_with(
        &mut application,
        vec![
            rooted("Gone work", &gone, 100),
            rooted("Atlas work", &atlas, 90),
        ],
    );
    press(&mut application, KeyCode::Down);
    assert_eq!(selected_row(&application), "gone");
    let resolution = choose_unanswered(&mut application);
    type_terminal_text(&mut application, "Keep this draft");

    assert_eq!(
        answer_workspace_resolution(&mut application, resolution),
        ApplicationTransition::Continue,
        "a refused pick asks nothing more of the server"
    );

    let frame = rendered_application_rows_at(&application, 120, 20).join("\n");
    assert!(
        frame.contains("Could not open the gone Workspace: No directory there"),
        "the refusal stands on the Landing the reader is on: {frame}"
    );
    assert!(
        !frame.contains("Workspaces"),
        "the picker stays closed: {frame}"
    );
    assert!(frame.contains("Keep this draft"), "{frame}");
    assert!(
        landing_location(&application).ends_with("here"),
        "the Landing names the Workspace the client still works in: {frame}"
    );
    assert_eq!(
        session_picker_scope(&mut application),
        SessionListScope::CurrentWorkspace(here.into()),
        "current-Workspace scope did not follow the refused path"
    );
    press(&mut application, KeyCode::Esc);

    open_picker_with(
        &mut application,
        vec![
            rooted("Gone work", &gone, 100),
            rooted("Atlas work", &atlas, 90),
        ],
    );
    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Down);
    assert_eq!(selected_row(&application), "atlas");
    choose(&mut application);
    let switched = rendered_application_rows_at(&application, 120, 20).join("\n");
    assert!(
        !switched.contains("Could not open"),
        "a later choice is not told of the earlier refusal: {switched}"
    );
    assert_eq!(
        session_picker_scope(&mut application),
        SessionListScope::CurrentWorkspace(atlas.into()),
        "a valid pick after a refused one switches normally"
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
    show_sidebar(&mut application, Vec::new());

    open_picker_with(&mut application, vec![rooted("Old work", &file, 30)]);
    press(&mut application, KeyCode::Down);
    assert_eq!(selected_row(&application), "old-work");

    choose(&mut application);
    let refusal = rendered_application_rows_at(&application, SIDEBAR_WIDE, 20).join("\n");
    assert!(
        refusal.contains("Not a directory"),
        "the Landing says why the choice was refused: {refusal}"
    );

    type_terminal_text(&mut application, "$rev");
    let unchanged = rendered_application_rows_at(&application, SIDEBAR_WIDE, 20);
    let unchanged_frame = unchanged.join("\n");
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
        SessionListScope::CurrentWorkspace((here.path().to_owned()).into()),
        "current-Workspace scope did not follow the refused path"
    );
}

/// The Workspace a reader chose last is the one they land in. Choosing again
/// before the Server has answered the first choice lets go of it, so an answer
/// to it arriving late finds nothing left to answer.
#[test]
fn a_newer_choice_supersedes_a_workspace_still_resolving() {
    let root = workspace_dir();
    let here = workspace_in(root.path(), "here");
    let atlas = workspace_in(root.path(), "atlas");
    let ledger = workspace_in(root.path(), "ledger");
    let listed = || vec![rooted("Newer", &atlas, 30), rooted("Older", &ledger, 20)];
    let mut application = connected_application(&here);

    open_picker_with(&mut application, listed());
    press(&mut application, KeyCode::Down);
    assert_eq!(selected_row(&application), "atlas");
    let first = choose_unanswered(&mut application);

    open_picker_with(&mut application, listed());
    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Down);
    assert_eq!(selected_row(&application), "ledger");
    let second = choose_unanswered(&mut application);
    assert!(
        landing_location(&application).ends_with(&format!("ledger · {SPINNER}")),
        "the Landing names the Workspace chosen last"
    );

    assert_eq!(
        answer_workspace_resolution(&mut application, first),
        ApplicationTransition::Continue
    );
    assert!(
        landing_location(&application).ends_with(&format!("ledger · {SPINNER}")),
        "the superseded answer does not pull the Landing back to the earlier choice, \
         which is still waiting on its own"
    );
    assert_eq!(
        session_picker_scope(&mut application),
        SessionListScope::CurrentWorkspace(here.clone().into()),
        "nor does it move the client there"
    );
    press(&mut application, KeyCode::Esc);

    answer_workspace_resolution(&mut application, second);
    assert_eq!(
        session_picker_scope(&mut application),
        SessionListScope::CurrentWorkspace(ledger.into()),
        "the answer to the latest choice is the one adopted"
    );
}

/// Going to a fresh Landing is a newer route than the one still resolving,
/// as it is for a `/sidekick` still waiting: the reader is not going to the
/// Workspace they chose after all, and its answer arriving late leaves them,
/// and the draft they have since begun, where they are.
#[test]
fn a_fresh_landing_lets_go_of_a_workspace_still_resolving() {
    let root = workspace_dir();
    let here = workspace_in(root.path(), "here");
    let atlas = workspace_in(root.path(), "atlas");
    let mut application = connected_application(&here);

    open_picker_with(&mut application, vec![rooted("Newer", &atlas, 30)]);
    press(&mut application, KeyCode::Down);
    let resolution = choose_unanswered(&mut application);

    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
                SemanticCommandId::SessionNew,
            )))
            .expect("go to a fresh Landing instead"),
        ApplicationTransition::DetachSession
    );
    assert!(
        landing_location(&application).ends_with("here"),
        "the fresh Landing stands in the Workspace the client still works in"
    );
    type_terminal_text(&mut application, "a different thought");

    assert_eq!(
        answer_workspace_resolution(&mut application, resolution),
        ApplicationTransition::Continue,
        "the reader went elsewhere, so the answer has nothing left to answer"
    );
    let landing = rendered_application_rows_at(&application, 120, 20).join("\n");
    assert!(landing.contains("a different thought"), "{landing}");
    assert!(
        landing_location(&application).ends_with("here"),
        "{landing}"
    );
    assert_eq!(
        session_picker_scope(&mut application),
        SessionListScope::CurrentWorkspace(here.into())
    );
}

/// A refusal is an answer too. Arriving for a Workspace the reader has since
/// let go of by going to a fresh Landing, it is said nowhere, and that Landing
/// and the draft begun on it stand exactly as they were.
#[test]
fn a_late_refusal_for_a_workspace_let_go_of_moves_nothing() {
    let root = workspace_dir();
    let here = workspace_in(root.path(), "here");
    let atlas = workspace_in(root.path(), "atlas");
    let mut application = connected_application(&here);

    open_picker_with(&mut application, vec![rooted("Newer", &atlas, 30)]);
    press(&mut application, KeyCode::Down);
    let resolution = choose_unanswered(&mut application);
    let (outlook, surface, request_id, _) = crate::support::workspace_resolution(&resolution)
        .expect("choosing a Workspace asks its Server to resolve it");
    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::SessionNew,
        )))
        .expect("go to a fresh Landing instead");
    type_terminal_text(&mut application, "a different thought");
    let before = rendered_application_rows_at(&application, 120, 20);

    assert_eq!(
        application
            .handle_event(ApplicationEvent::WorkspaceResolved {
                outlook,
                surface,
                request_id,
                result: Err("the disk is full".to_owned()),
            })
            .expect("deliver the late refusal"),
        ApplicationTransition::Continue,
        "the reader went elsewhere, so the refusal has nothing left to answer"
    );
    let after = rendered_application_rows_at(&application, 120, 20);
    let landing = after.join("\n");
    assert!(
        !landing.contains("the disk is full") && !landing.contains("Could not open"),
        "no refusal is said for a choice the reader let go of: {landing}"
    );
    assert_eq!(
        after, before,
        "the fresh Landing and its draft stand exactly as they were"
    );
    assert_eq!(
        session_picker_scope(&mut application),
        SessionListScope::CurrentWorkspace(here.into())
    );
}

/// A Workspace asked for anywhere else — here the Sidekick's — is asked for
/// later than the one the Landing awaits, so it is the one the reader is left
/// in, whichever of the two the Server answers first.
#[test]
fn a_workspace_asked_for_elsewhere_supersedes_one_still_resolving() {
    let root = workspace_dir();
    let atlas = workspace_in(root.path(), "atlas");
    let sidekick = workspace_in(root.path(), "sidekick");
    let mut application = connected_application(root.path());

    open_picker_with(&mut application, vec![rooted("Newer", &atlas, 30)]);
    press(&mut application, KeyCode::Down);
    let resolution = choose_unanswered(&mut application);

    let ApplicationTransition::ResolveSidekickWorkspace {
        outlook,
        request_id,
    } = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::SessionSidekick,
        )))
        .expect("ask for the Sidekick while the chosen Workspace resolves")
    else {
        panic!("/sidekick asks its Server for the Sidekick Workspace");
    };
    application
        .handle_event(ApplicationEvent::WorkspaceResolved {
            outlook,
            surface: suru::tui::WorkspaceResolutionSurface::Sidekick,
            request_id,
            result: Ok(suru::protocol::ResolvedWorkspace::directory(
                sidekick.clone(),
            )),
        })
        .expect("answer the Sidekick Workspace");
    assert_eq!(
        answer_workspace_resolution(&mut application, resolution),
        ApplicationTransition::Continue
    );

    type_terminal_text(&mut application, "Initial Prompt");
    let ApplicationTransition::CreateSession(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit the initial Prompt")
    else {
        panic!("a Landing submission creates a Session");
    };
    assert_eq!(
        request.execution_directory.path, sidekick,
        "the late answer to the picker did not move the reader off the Workspace asked for since"
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
    assert_eq!(request.execution_directory.path, atlas);
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
        SessionListScope::CurrentWorkspace((atlas).into()),
        "the picker's current-Workspace scope means the Workspace the reader chose"
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

    assert!(
        matches!(
            choose(&mut application),
            ApplicationTransition::DetachSessionAndResolveWorkspace { .. }
        ),
        "the client stops watching the Session; the Turn is neither interrupted \
         nor confirmed away"
    );

    let rows = rendered_application_rows_at(&application, SIDEBAR_WIDE, 20);
    let frame = rows.join("\n");
    assert!(
        frame.contains("Type a prompt"),
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

    assert!(
        matches!(
            choose(&mut application),
            ApplicationTransition::DetachSessionAndResolveWorkspace { .. }
        ),
        "opening the Landing is the client leaving whatever it was on"
    );

    let landing = rendered_application_rows_at(&application, 120, 20).join("\n");
    assert!(
        !landing.contains("Workspaces"),
        "the picker is done with: {landing}"
    );
    assert!(landing.contains("Type a prompt"), "{landing}");
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

/// What the Browse row says, which tells it from the Workspace rows beneath it.
const BROWSE_ROW: &str = "Browse directories…";

/// Above its Workspaces the picker stands a Browse row, its way out to a
/// directory it does not list, and the current Workspace stands first beneath
/// it.
#[test]
fn the_browse_row_stands_first_above_the_workspace_rows() {
    let here = workspace(&["work", "here"]);
    let ledger = workspace(&["work", "ledger"]);
    let mut application = connected_application(&here);

    open_picker_with(&mut application, vec![rooted("Older", &ledger, 10)]);

    let lines = picker_lines(&application);
    assert_eq!(
        lines[0], BROWSE_ROW,
        "the Browse row stands first: {lines:?}"
    );
    assert!(
        lines[1].contains("here") && lines[1].contains("[current]"),
        "the current Workspace stands first beneath it: {lines:?}"
    );
    assert!(lines[2].contains("ledger"), "{lines:?}");
}

/// The picker opens on the Workspace the reader is in rather than on the
/// Browse row above it, so Enter goes on choosing what it always chose.
#[test]
fn the_picker_opens_on_the_current_workspace_rather_than_the_browse_row() {
    let here = workspace(&["work", "here"]);
    let mut application = connected_application(&here);

    open_picker_with(&mut application, narrowable_sessions());

    assert_eq!(selected_row(&application), "here");
    let (_, _, _, request) = workspace_resolution(&choose_unanswered(&mut application))
        .expect("Enter chooses the Workspace the reader is on");
    assert_eq!(request.path, here);
}

/// Search results are only Workspaces: a query takes the Browse row away, and
/// giving the query up brings it back above the Workspaces, the reader left
/// on the Workspace they were on.
#[test]
fn a_query_hides_the_browse_row_and_clearing_it_brings_it_back() {
    let mut application = connected_application(&workspace(&["work", "here"]));
    open_picker_with(&mut application, narrowable_sessions());
    press(&mut application, KeyCode::Up);
    assert_eq!(selected_row(&application), "Browse");

    type_terminal_text(&mut application, "e");

    let narrowed = picker_lines(&application);
    assert!(
        !narrowed.iter().any(|line| line.contains(BROWSE_ROW)),
        "a query leaves only Workspaces: {narrowed:?}"
    );
    assert_eq!(
        selected_row(&application),
        "here",
        "the reader is put on the first Workspace the query leaves"
    );
    press(&mut application, KeyCode::Down);
    assert_eq!(selected_row(&application), "engine");

    backspace(&mut application);

    let restored = picker_lines(&application);
    assert_eq!(
        restored[0], BROWSE_ROW,
        "the Browse row stands first again once the query is given up: {restored:?}"
    );
    assert_eq!(
        selected_row(&application),
        "engine",
        "and the reader stays on the Workspace they were on"
    );
}

/// Enter on the Browse row opens the Directory Browser in the picker's place,
/// rooted where `/browse` roots it: at the Landing's Execution Directory,
/// here a subdirectory of its Workspace.
#[test]
fn enter_on_the_browse_row_opens_the_directory_browser_at_the_execution_directory() {
    let repository = workspace(&["work", "repo"]);
    let here = repository.join("here");
    let mut application = landing_in_subdirectory(&repository, &here);
    open_picker_with(&mut application, narrowable_sessions());
    press(&mut application, KeyCode::Up);

    let (_, request) = expect_directory_listing(choose_unanswered(&mut application));

    assert_eq!(
        request,
        ListDirectoryRequest {
            path: here.clone(),
            base: Some(here),
        },
        "the browser is rooted at the Landing's Execution Directory, and reads from it"
    );
    assert_eq!(
        overlays_drawn(&application),
        (false, true),
        "the browser stands in the picker's place"
    );
}

/// Ctrl+O opens the Directory Browser from anywhere in the picker, a query
/// typed or not, so the reader need not walk to the Browse row — which a
/// query takes away.
#[test]
fn ctrl_o_opens_the_directory_browser_with_or_without_a_query() {
    for query in ["", "eng"] {
        let here = workspace(&["work", "here"]);
        let mut application = connected_application(&here);
        open_picker_with(&mut application, narrowable_sessions());
        type_terminal_text(&mut application, query);

        let (_, request) = expect_directory_listing(ctrl_o(&mut application));

        assert_eq!(
            request.path, here,
            "{query:?}: rooted at the Landing's Execution Directory"
        );
        assert_eq!(
            overlays_drawn(&application),
            (false, true),
            "{query:?}: the browser stands in the picker's place"
        );
    }
}

/// Esc from a browser the picker opened goes back to the picker as the reader
/// left it — the query typed, the row they were on, and how far the list had
/// scrolled to keep that row in view — asking the Server for nothing. A
/// second Esc closes the picker as it always does.
#[test]
fn escape_from_a_browser_opened_from_the_picker_returns_to_the_picker_as_it_was() {
    let here = workspace(&["work", "here"]);
    let mut application = connected_application(&here);
    open_picker_with(
        &mut application,
        (1..=20)
            .map(|index| {
                rooted(
                    "Work",
                    &workspace(&["work", &format!("ws{index:02}")]),
                    index,
                )
            })
            .collect(),
    );
    type_terminal_text(&mut application, "ws");
    press(&mut application, KeyCode::Up);
    let left = rendered_application_rows(&application);
    assert!(
        !left.join("\n").contains("ws20"),
        "the row the reader walked to is beneath the first ones, so the list has scrolled: {left:#?}"
    );
    let (listing_id, _) = expect_directory_listing(ctrl_o(&mut application));
    application
        .handle_event(ApplicationEvent::DirectoryListed {
            listing_id,
            result: Ok(DirectoryListing {
                root: here.clone(),
                parent: here.parent().map(Path::to_owned),
                source_control: DirectorySourceControl::Plain,
                children: Vec::new(),
            }),
        })
        .expect("list the browser's root");

    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Esc,
                KeyModifiers::NONE
            )))
            .expect("back out of the browser"),
        ApplicationTransition::Continue,
        "going back to the picker asks the Server for nothing"
    );

    assert_eq!(
        rendered_application_rows(&application),
        left,
        "the picker stands exactly as the reader left it"
    );
    press(&mut application, KeyCode::Esc);
    assert_eq!(overlays_drawn(&application), (false, false));
}

/// A choice in a browser the picker opened is done with both: the Landing
/// opens on the directory chosen, resolved as the picker's own choice is,
/// and neither overlay stands over it.
#[test]
fn a_choice_in_a_browser_opened_from_the_picker_closes_both() {
    let here = workspace(&["work", "here"]);
    let alpha = here.join("alpha");
    let mut application = connected_application(&here);
    open_picker_with(&mut application, narrowable_sessions());
    type_terminal_text(&mut application, "eng");
    let (listing_id, _) = expect_directory_listing(ctrl_o(&mut application));
    application
        .handle_event(ApplicationEvent::DirectoryListed {
            listing_id,
            result: Ok(DirectoryListing {
                root: here.clone(),
                parent: here.parent().map(Path::to_owned),
                source_control: DirectorySourceControl::Plain,
                children: vec![ChildDirectory {
                    name: "alpha".to_owned(),
                    path: alpha.clone(),
                    source_control: DirectorySourceControl::Plain,
                    hidden: false,
                }],
            }),
        })
        .expect("list the browser's root");
    press(&mut application, KeyCode::Down);

    let choice = choose_unanswered(&mut application);

    assert!(
        matches!(
            choice,
            ApplicationTransition::DetachSessionAndResolveWorkspace { .. }
        ),
        "choosing opens the Landing and asks the Server where it is: {choice:?}"
    );
    let (_, surface, _, request) =
        workspace_resolution(&choice).expect("choosing asks for a Workspace resolution");
    assert_eq!(surface, WorkspaceResolutionSurface::WorkspacePicker);
    assert_eq!(request.path, alpha);
    assert_eq!(
        overlays_drawn(&application),
        (false, false),
        "neither the browser nor the picker beneath it is left standing"
    );
}

/// The footer names Ctrl+O beside the keys it already named: in words where
/// the box holds them, and by the chord alone where it does not.
#[test]
fn the_footer_names_ctrl_o() {
    let mut application = connected_application(&workspace(&["work", "here"]));
    open_picker_with(&mut application, narrowable_sessions());

    let wide = rendered_application_rows_at(&application, 80, 15);
    let footer = &wide[rendered_row(&wide, "Esc close")];
    assert!(footer.contains("Ctrl+O browse"), "{footer}");

    let narrow = rendered_application_rows_at(&application, 43, 10);
    let footer = &narrow[rendered_row(&narrow, "^E")];
    assert!(footer.contains("^O"), "{footer}");
}

/// A left press on the Browse row opens the Directory Browser exactly as
/// Enter on it does.
#[test]
fn a_left_press_on_the_browse_row_opens_the_browser_exactly_as_enter_does() {
    for frame in PICKER_FRAMES {
        let here = workspace(&["work", "here"]);
        let picking = || {
            let mut application = connected_application(&here);
            open_picker_with(&mut application, narrowable_sessions());
            application
        };

        let mut keyed = picking();
        press(&mut keyed, KeyCode::Up);
        let entered = choose_unanswered(&mut keyed);
        let mut pointed = picking();
        let pressed = press_row(&mut pointed, frame, MouseButton::Left, BROWSE_ROW);

        let (_, request) = expect_directory_listing(pressed.clone());
        assert_eq!(request.path, here, "{frame:?}");
        assert_eq!(
            pressed, entered,
            "{frame:?}: the press asks what Enter asks"
        );
        assert_eq!(
            rendered_application_rows_at(&pointed, frame.0, frame.1),
            rendered_application_rows_at(&keyed, frame.0, frame.1),
            "{frame:?}: and the same browser stands in the picker's place"
        );
    }
}

/// Choosing a Workspace waits out an Agent Selection update still on its way,
/// since it opens the Landing; the Browse row only opens the browser, so
/// Enter on it opens the browser at once, as Ctrl+O does.
#[test]
fn enter_on_the_browse_row_does_not_wait_out_a_pending_agent_selection() {
    let here = workspace(&["work", "here"]);
    let mut application = application_awaiting_agent_selection(&here);
    open_picker_with(&mut application, narrowable_sessions());
    press(&mut application, KeyCode::Up);

    let (_, request) = expect_directory_listing(choose_unanswered(&mut application));

    assert_eq!(request.path, here);
    assert_eq!(overlays_drawn(&application), (false, true));
}

/// The Browse row stands for no Workspace, so nothing acts on it as on one: a
/// right press on it opens no menu, and Ctrl+E on it describes nothing.
#[test]
fn the_browse_row_has_no_menu_and_no_description_to_edit() {
    for frame in PICKER_FRAMES {
        let mut application = connected_application(&workspace(&["work", "here"]));
        open_picker_with(&mut application, narrowable_sessions());
        let before = rendered_application_rows_at(&application, frame.0, frame.1);

        assert_eq!(
            press_row(&mut application, frame, MouseButton::Right, BROWSE_ROW),
            ApplicationTransition::Continue,
            "{frame:?}"
        );
        assert_eq!(
            rendered_application_rows_at(&application, frame.0, frame.1),
            before,
            "{frame:?}: no menu opens"
        );
    }

    let mut application = connected_application(&workspace(&["work", "here"]));
    open_picker_with(&mut application, narrowable_sessions());
    press(&mut application, KeyCode::Up);
    let before = rendered_application_rows(&application);
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('e'),
            KeyModifiers::CONTROL,
        )))
        .expect("press Ctrl+E");
    assert_eq!(
        rendered_application_rows(&application),
        before,
        "no Description editor opens"
    );
}

/// The browser reads the Outlook's Server, so once that Remote stops
/// answering, Ctrl+O, Enter on the Browse row, and a press on it are refused
/// as any other start is: nothing is asked, and the picker stands as the
/// reader left it, saying why inside it — where they are looking, since it
/// covers the Landing, which goes on saying it once the picker is put away.
#[test]
fn opening_the_browser_from_the_picker_is_refused_for_an_unreachable_remote() {
    type WayIn = fn(&mut Application) -> ApplicationTransition;
    let ways_in: [(&str, &str, bool, WayIn); 3] = [
        ("Ctrl+O under a query", "atl", false, ctrl_o),
        ("Enter on the Browse row", "", true, choose_unanswered),
        ("a press on the Browse row", "", true, |application| {
            press_row(application, (80, 15), MouseButton::Left, BROWSE_ROW)
        }),
    ];
    for (way, query, on_browse_row, open) in ways_in {
        let mut application = application_looking_at_studio();
        open_picker_with(
            &mut application,
            vec![rooted(
                "Remote work",
                &crate::support::named_workspace_path("atlas"),
                30,
            )],
        );
        studio_stops_answering(&mut application, 1, Duration::from_secs(5));
        type_terminal_text(&mut application, query);
        if on_browse_row {
            press(&mut application, KeyCode::Up);
        }
        let left_on = (picker_query(&application), selected_row(&application));

        assert_eq!(
            open(&mut application),
            ApplicationTransition::Continue,
            "{way}: nothing is asked"
        );

        assert_eq!(
            overlays_drawn(&application),
            (true, false),
            "{way}: no browser opens, and the picker stands"
        );
        let lines = picker_lines(&application);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("studio is unreachable")),
            "{way}: the refusal is said inside the picker: {lines:?}"
        );
        assert_eq!(
            (picker_query(&application), selected_row(&application)),
            left_on,
            "{way}: the query and the row the reader was on are as they left them"
        );
        press(&mut application, KeyCode::Esc);
        let landing = rendered_application_rows_at(&application, 120, 20).join("\n");
        assert!(
            landing.contains("studio is unreachable"),
            "{way}: the Landing goes on saying it: {landing}"
        );
    }
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
    suru::paths::canonical(workspace).expect("canonicalize the Workspace fixture")
}

/// A listed Session rooted at the Workspace named, last active when the test
/// says — which is what a catalog spanning several Workspaces is made of, and
/// what the picker's ordering is derived from.
fn rooted(title: &str, workspace: &Path, updated_at: u64) -> SessionListItem {
    SessionListItem::Readable(Box::new(SessionSummary {
        checkout_state: None,
        session: Session {
            checkout: None,
            context_fill: None,
            id: SessionId::new(),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.to_owned(),
            },
            workspace: Workspace::directory(workspace.to_owned()),
            agent_selection: None,
            agent_selection_availability: ModelAvailability::Available,
            approval_posture: None,
            status: SessionStatus::Idle,
            working_since: None,
            monitoring_since: None,
            parent: None,
            begun_by: None,
        },
        title: title.to_owned(),
        icon: None,
        settled_at: None,
        standing_inputs: Default::default(),
        total_usage: None,
        own_cost: None,
        remote_subsessions: Vec::new(),
        created_at: SessionTimestamp(1),
        updated_at: SessionTimestamp(updated_at),
    }))
}

/// Connects to a Server naming `directory` as its Sidekick Workspace.
fn name_sidekick_workspace(application: &mut Application, directory: &Path) {
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
            ready_health(fixture_instance_id(), 42).with_workspace_paths(
                suru::protocol::WorkspacePaths::default().with_sidekick_workspace(directory),
            ),
        )))
        .expect("connect to a Server naming its Sidekick Workspace");
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

/// Presses `button` on the first cell of `needle` as the frame of `size`
/// draws it, so the press lands where the reader would point.
fn press_row(
    application: &mut Application,
    size: (u16, u16),
    button: MouseButton,
    needle: &str,
) -> ApplicationTransition {
    let buffer = rendered_application_buffer(application, size.0, size.1);
    let (column, row) = text_position(&buffer, needle);
    super::support::click_mouse(
        application,
        MouseEvent {
            kind: MouseEventKind::Down(button),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        },
    )
    .expect("press over the Workspace Picker")
}

/// An Application with a Session open in `workspace` whose Agent Selection is
/// changing to a Model chosen in the Model picker, with its Server yet to
/// answer: the update is still on its way.
fn application_awaiting_agent_selection(workspace: &Path) -> Application {
    let codex = ProviderId::new("codex");
    let mut application = Application::new(workspace, Default::default());
    application
        .handle_event(ApplicationEvent::SessionAttached(
            selected_session_snapshot(
                SessionId::new(),
                workspace,
                AgentSelection {
                    provider: codex.clone(),
                    model: ModelId::new("old"),
                    options: vec![],
                },
            ),
        ))
        .expect("attach a Session");
    let ApplicationTransition::ListModels(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::ModelList,
        )))
        .expect("open the Model picker")
    else {
        panic!("the Model picker asks for its catalog");
    };
    application
        .handle_event(ApplicationEvent::ModelsListed {
            request,
            catalog: ModelCatalog {
                providers: vec![ProviderModelCatalog {
                    display_name: "Codex".to_owned(),
                    provider: codex,
                    models: vec![
                        model_descriptor(
                            "codex",
                            "old",
                            "Old Model",
                            true,
                            ModelAvailability::Available,
                        ),
                        model_descriptor(
                            "codex",
                            "new",
                            "New Model",
                            false,
                            ModelAvailability::Available,
                        ),
                    ],
                    status: ProviderCatalogStatus::Fresh,
                }],
            },
        })
        .expect("load the Model catalog");
    type_terminal_text(&mut application, "New Model");
    let ApplicationTransition::UpdateAgentSelection { .. } = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("choose the Model")
    else {
        panic!("choosing a Model for the open Session updates its Agent Selection");
    };
    application
}

fn press(application: &mut Application, code: KeyCode) {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
        .expect("drive the Workspace Picker");
}

/// The picker's Workspace rows, taken from the frame between the search line
/// and the footer and trimmed of the box that draws them, leaving out the
/// Browse row above them.
fn picker_rows(application: &Application) -> Vec<String> {
    picker_rows_at(application, 80, 15)
}

fn picker_rows_at(application: &Application, width: u16, height: u16) -> Vec<String> {
    picker_lines_at(application, width, height)
        .into_iter()
        .filter(|row| row.trim_start_matches(['›', ' ']) != BROWSE_ROW)
        .collect()
}

/// Every line the picker draws between its search line and its footer, the
/// Browse row among them, trimmed of the box that draws them.
fn picker_lines(application: &Application) -> Vec<String> {
    picker_lines_at(application, 80, 15)
}

fn picker_lines_at(application: &Application, width: u16, height: u16) -> Vec<String> {
    let rows = rendered_application_rows_at(application, width, height);
    let search = rendered_row(&rows, "Search:");
    let footer = rows
        .iter()
        .position(|row| row.contains("Esc"))
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
/// draws in front of it — `Browse` where they are on the Browse row.
fn selected_row(application: &Application) -> String {
    let rows = picker_lines(application);
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

/// The name of the Workspace the reader is on in a frame of `size`, read off
/// the marker the frame draws in front of it — `Browse` where they are on the
/// Browse row.
fn selected_row_at(application: &Application, size: (u16, u16)) -> String {
    let rows = picker_lines_at(application, size.0, size.1);
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

/// Enter on the row the reader is on, which is how a Workspace is chosen,
/// followed by its Server's answer. Answers what the choice itself asked for.
fn choose(application: &mut Application) -> ApplicationTransition {
    let transition = choose_unanswered(application);
    answer_workspace_resolution(application, transition.clone());
    transition
}

/// Enter on the row the reader is on, with its Server yet to answer — as a
/// slow one would be — answering what the choice asked for.
fn choose_unanswered(application: &mut Application) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("choose the Workspace the reader is on")
}

/// Ctrl+O, which opens the Directory Browser from anywhere in the picker.
fn ctrl_o(application: &mut Application) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('o'),
            KeyModifiers::CONTROL,
        )))
        .expect("press Ctrl+O")
}

/// The listing a Directory Browser opening asks the Outlook's Server for.
fn expect_directory_listing(
    transition: ApplicationTransition,
) -> (DirectoryListingId, ListDirectoryRequest) {
    let ApplicationTransition::ListDirectory {
        listing_id,
        request,
        ..
    } = transition
    else {
        panic!("the browser asks the Server for a directory's children, not {transition:?}");
    };
    (listing_id, request)
}

/// Whether a frame draws the Workspace Picker, and whether it draws the
/// Directory Browser.
fn overlays_drawn(application: &Application) -> (bool, bool) {
    let frame = rendered_application_rows(application).join("\n");
    (
        frame.contains(" Workspaces "),
        frame.contains(" Directory Browser "),
    )
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
                execution_directory: Some(suru::protocol::ExecutionDirectory {
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

/// The line beneath the Landing's composer, which names the Workspace the
/// Landing stands in by the end of its path.
fn landing_location(application: &Application) -> String {
    let screen = rendered_application_rows_at(application, 120, 20);
    let composer_bottom = screen
        .iter()
        .position(|row| row.contains('└'))
        .unwrap_or_else(|| panic!("the Landing draws its composer: {screen:#?}"));
    screen[composer_bottom + 1].trim().to_owned()
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
    let mut application = Application::new(workspace, Default::default());
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
        execution_directory: suru::protocol::ExecutionDirectory {
            path: workspace.to_owned(),
        },
    };
    application
        .handle_event(ApplicationEvent::SkillsListed {
            request: request.clone(),
            catalog: SkillCatalog {
                provider: request.provider.clone(),
                execution_directory: request.execution_directory.clone(),
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

#[test]
fn an_open_sessions_execution_context_is_independent_of_grouping_and_landing() {
    use suru::protocol::{Activity, ActivityStatus, FileChange, PromptId};
    let root = workspace_dir();
    let landing_root = workspace_in(root.path(), "landing");
    let landing = workspace_in(&landing_root, "subdirectory");
    let grouping = workspace_in(root.path(), "repository");
    let linked = workspace_in(&grouping, "linked");
    let execution = workspace_in(&linked, "nested");
    let mut application = application_choosing_skills(&landing);
    let mut snapshot = crate::support::failed_session_snapshot(
        SessionId::new(),
        PromptId::new(),
        "Create a file",
        &execution,
    );
    snapshot.session.workspace.path = grouping;
    snapshot.session.status = SessionStatus::Active;
    snapshot.turns[0].status = suru::protocol::TurnStatus::Active;
    snapshot.turns[0].settled_at = None;
    snapshot.session.agent_selection = Some(AgentSelection {
        provider: ProviderId::new("codex"),
        model: ModelId::new("gpt-fixture"),
        options: Vec::new(),
    });
    snapshot.activities[0] = Activity::FileChange {
        id: snapshot.activities[0].id(),
        turn_id: snapshot.turns[0].id,
        status: ActivityStatus::Completed,
        changes: vec![FileChange::Add {
            path: execution.join("created.rs"),
        }],
    };
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .unwrap();
    let transcript = rendered_application_rows(&application).join("\n");
    assert!(transcript.contains("Created created.rs"), "{transcript}");

    let request = SkillCatalogRequest {
        provider: ProviderId::new("codex"),
        execution_directory: suru::protocol::ExecutionDirectory { path: execution },
    };
    application
        .handle_event(ApplicationEvent::SkillsListed {
            request: request.clone(),
            catalog: SkillCatalog {
                provider: request.provider.clone(),
                execution_directory: request.execution_directory.clone(),
                skills: Vec::new(),
                capabilities: SkillCatalogCapabilities {
                    max_distinct_invocations: None,
                    supported_deliveries: vec![SkillPromptDelivery::Steer],
                },
                status: SkillCatalogStatus::Stale {
                    message: "Refresh needed".to_owned(),
                },
            },
        })
        .unwrap();
    assert_eq!(
        application
            .handle_event(ApplicationEvent::Command(CommandId::InsertText(
                "$".to_owned()
            )))
            .unwrap(),
        ApplicationTransition::RefreshSkills(request)
    );
    press(&mut application, KeyCode::Esc);
    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::SessionNew,
        )))
        .expect("leave the Session for the Landing");
    type_terminal_text(&mut application, "Initial Prompt");
    let ApplicationTransition::CreateSession(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit the initial Prompt")
    else {
        panic!("a Landing submission creates a Session");
    };
    assert_eq!(
        request.execution_directory.path, landing,
        "the Landing kept its own Execution Directory while the open Session worked in another"
    );
}

/// Server discovery owns grouping; the Client retains the precise launch
/// directory and selects a Repository by identity even after its label changes.
#[test]
fn repository_rows_deduplicate_by_metadata_identity_and_preserve_execution_context() {
    use suru::protocol::{
        Repository, RepositoryId, RepositoryLocation, ResolvedWorkspace, SourceControlAvailability,
        SourceControlCapabilities,
    };
    let temporary = tempfile::tempdir().unwrap();
    let root = suru::paths::canonical(temporary.path()).unwrap();
    let main = root.join("main");
    let linked = root.join("linked");
    let metadata = root.join("metadata");
    std::fs::create_dir(&main).unwrap();
    std::fs::create_dir(&linked).unwrap();
    let execution = linked.join("nested");
    std::fs::create_dir(&execution).unwrap();
    let repository = Repository {
        id: RepositoryId::from_metadata("git", &metadata),
        system: "git".to_owned(),
        metadata_directory: metadata.clone(),
        location: RepositoryLocation::UnknownMain,
        availability: SourceControlAvailability::Available,
        capabilities: SourceControlCapabilities::discovery_only(),
    };
    let unknown = Workspace {
        id: repository.id.workspace_id(),
        path: metadata,
        repository: Some(Box::new(repository.clone())),
        source_control: SourceControlAvailability::Available,
        icon: None,
        description: None,
    };
    let mut known = unknown.clone();
    known.path = main.clone();
    known.repository.as_mut().unwrap().location = RepositoryLocation::Main { root: main.clone() };
    let mut application = Application::new(&execution, Default::default());
    let ApplicationTransition::ResolveWorkspace {
        outlook,
        surface,
        request_id,
        request,
    } = application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
            ready_health(fixture_instance_id(), 42_424),
        )))
        .unwrap()
    else {
        panic!("launch context resolves on the owning Server")
    };
    assert_eq!(request.path, execution);
    application
        .handle_event(ApplicationEvent::WorkspaceResolved {
            outlook,
            surface,
            request_id,
            result: Ok(ResolvedWorkspace {
                execution_status: suru::protocol::ExecutionDirectoryStatus::Available,
                workspace: unknown.clone(),
                execution_directory: Some(suru::protocol::ExecutionDirectory {
                    path: execution.clone(),
                }),
                checkout: None,
                checkouts: vec![],
            }),
        })
        .unwrap();
    let mut first = rooted("Linked Session", &execution, 20);
    let SessionListItem::Readable(summary) = &mut first else {
        unreachable!()
    };
    summary.session.workspace = unknown.clone();
    open_picker_with(&mut application, vec![first.clone()]);
    let rows = picker_rows_at(&application, 140, 18);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].contains("main checkout unknown"));
    press(&mut application, KeyCode::Esc);
    let mut second = rooted("Main Session", &main, 30);
    let SessionListItem::Readable(summary) = &mut second else {
        unreachable!()
    };
    summary.session.workspace = known.clone();
    open_picker_with(&mut application, vec![second, first]);
    let rows = picker_rows_at(&application, 140, 18);
    assert_eq!(rows.len(), 1, "known and unknown main are one Repository");
    assert!(rows[0].contains("[current]"));
    let ApplicationTransition::DetachSessionAndResolveWorkspace { request, .. } = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .unwrap()
    else {
        panic!("selection resolves on the owning Server")
    };
    assert_eq!(request.workspace_id, Some(unknown.id));
    assert_eq!(request.path, main);
    // A fresh Landing lets go of the choice before it is answered, and merely
    // receiving newer grouping labels did not change execution context.
    application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::SessionNew,
        )))
        .unwrap();
    let ApplicationTransition::ResolveWorkspace { request, .. } = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::WorktreeList,
        )))
        .unwrap()
    else {
        panic!("worktree navigation resolves the current execution context")
    };
    assert_eq!(
        request.remembered_execution_directory.unwrap().path,
        execution
    );
}

/// The Landing of a bare Repository at `root`, which has metadata but no
/// working copy for a Session to execute in.
fn bare_repository_landing(root: &Path) -> Application {
    use suru::protocol::{
        Repository, RepositoryId, RepositoryLocation, ResolvedWorkspace, SourceControlAvailability,
        SourceControlCapabilities,
    };
    let root = root.to_owned();
    let repository = Repository {
        id: RepositoryId::from_metadata("git", &root),
        system: "git".to_owned(),
        metadata_directory: root.clone(),
        location: RepositoryLocation::Bare { root: root.clone() },
        availability: SourceControlAvailability::Available,
        capabilities: SourceControlCapabilities::discovery_only(),
    };
    let workspace = Workspace {
        id: repository.id.workspace_id(),
        path: root.clone(),
        repository: Some(Box::new(repository)),
        source_control: SourceControlAvailability::Available,
        icon: None,
        description: None,
    };
    let mut application = Application::new(&root, Default::default());
    let ApplicationTransition::ResolveWorkspace {
        outlook,
        surface,
        request_id,
        ..
    } = application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
            ready_health(fixture_instance_id(), 42_424),
        )))
        .unwrap()
    else {
        panic!("resolve launch")
    };
    application
        .handle_event(ApplicationEvent::WorkspaceResolved {
            outlook,
            surface,
            request_id,
            result: Ok(ResolvedWorkspace {
                execution_status: suru::protocol::ExecutionDirectoryStatus::RequiresWorkingCopy,
                workspace,
                execution_directory: None,
                checkout: None,
                checkouts: vec![],
            }),
        })
        .unwrap();
    application
}

#[test]
fn bare_repository_landing_requires_working_copy_before_creating_session() {
    let temporary = tempfile::tempdir().unwrap();
    let root = suru::paths::canonical(temporary.path()).unwrap();
    let mut application = bare_repository_landing(&root);
    assert!(
        rendered_application_rows_at(&application, 160, 25)
            .join("\n")
            .contains("Choose a working copy")
    );
    type_terminal_text(&mut application, "Keep this draft");
    let transition = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .unwrap();
    assert_eq!(transition, ApplicationTransition::Continue);
    let rendered = rendered_application_rows_at(&application, 160, 25).join("\n");
    assert!(rendered.contains("Keep this draft"));
    assert!(rendered.contains("working copy"));
}

/// The Landing's line is only as wide as its composer, whatever the terminal.
/// A path too long for it gives up its front, as a Workspace Picker row's
/// does, so the line still ends on the Repository's own name and on what the
/// reader has to do next.
#[test]
fn a_long_workspace_path_gives_up_its_front_to_what_the_landing_asks() {
    let temporary = tempfile::tempdir().unwrap();
    let nested = temporary
        .path()
        .join("a-directory-name-long-enough-to-crowd-the-landing")
        .join("repository.git");
    std::fs::create_dir_all(&nested).unwrap();
    let root = suru::paths::canonical(nested).unwrap();
    let application = bare_repository_landing(&root);

    let line = rendered_application_rows_at(&application, 160, 25)
        .into_iter()
        .find(|row| row.contains("repository.git"))
        .expect("the Landing names the Repository");
    assert!(line.contains("…"), "the path gives up its front: {line}");
    assert!(
        line.contains("repository.git · Choose a working copy to start a Session"),
        "the path keeps its end, and what to do comes whole after it: {line}"
    );
}
