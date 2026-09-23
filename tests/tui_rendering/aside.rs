//! The Aside: the right-hand column answering for the open Session, and its
//! one Section today, the Subagents Section — the tree the open Session
//! belongs to, fed by the per-tree subscription.

use crate::support::{
    click_mouse, connected_application, deliver_settings, enter_session, failed_session_snapshot,
    invoke, key, rendered_application_buffer, rendered_application_rows_at, type_terminal_text,
    workspace_dir,
};
use crossterm::event::{
    Event as InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::style::Color;
use suru::{
    managed_client::SubagentTreeEvent,
    protocol::{
        Activity, ActivityStatus, AsideSettings, AsideVisibility, EffectiveSettings, Outlook,
        PromptId, SessionId, SessionReference, SessionSnapshot, SessionStatus, SessionTimestamp,
        SidebarVisibility, SubagentTreeChange, SubagentTreeEntry, SubagentTreeRevision,
        SubagentTreeSnapshot, SubagentTreeTopLevel, TurnStatus,
    },
    tui::{Application, ApplicationEvent, ApplicationTransition, SemanticCommandId},
};

/// Wide enough for the main view and the Aside at its launch width.
const WIDTH: u16 = 120;
const HEIGHT: u16 = 24;
/// The launch width of the Aside, rule included.
const ASIDE_WIDTH: u16 = 32;

/// The tree fixture's Sessions: a top-level Session with two Subagents of its
/// own, the first of which spawned one more.
struct Tree {
    top: SessionId,
    explore: SessionId,
    review: SessionId,
    plan: SessionId,
}

impl Tree {
    fn new() -> Self {
        Self {
            top: SessionId::new(),
            explore: SessionId::new(),
            review: SessionId::new(),
            plan: SessionId::new(),
        }
    }

    fn snapshot(&self) -> SubagentTreeSnapshot {
        SubagentTreeSnapshot {
            revision: SubagentTreeRevision::INITIAL,
            top_level: SubagentTreeTopLevel {
                session_id: self.top,
                title: "Map every seam".to_owned(),
                working_since: None,
                needs_intervention: false,
            },
            subagents: vec![
                entry(
                    self.explore,
                    self.top,
                    0,
                    ("Explore", "Map the provider seams"),
                    ActivityStatus::Completed,
                    Some(12_000),
                ),
                entry(
                    self.review,
                    self.explore,
                    0,
                    ("Review", "Check the mapped seams"),
                    ActivityStatus::Active,
                    None,
                ),
                entry(
                    self.plan,
                    self.top,
                    1,
                    ("Plan", "Weigh the options"),
                    ActivityStatus::Completed,
                    None,
                ),
            ],
        }
    }
}

fn entry(
    session_id: SessionId,
    parent_session_id: SessionId,
    spawn_order: u32,
    (name, title): (&str, &str),
    status: ActivityStatus,
    duration_ms: Option<u64>,
) -> SubagentTreeEntry {
    SubagentTreeEntry {
        session_id,
        parent_session_id,
        spawn_order,
        name: name.to_owned(),
        title: title.to_owned(),
        status,
        duration_ms,
        // Unknown unless a test says when the work began.
        started_at: None,
        needs_intervention: false,
    }
}

fn local(session_id: SessionId) -> SessionReference {
    SessionReference::new(Outlook::Local, session_id)
}

/// A client whose first Settings snapshot has shown the Aside, with the
/// Sidebar kept off the frame so the Aside's columns are the only ones taken
/// from the main view.
fn client(workspace: &std::path::Path) -> Application {
    let mut application = connected_application(workspace);
    let mut settings = EffectiveSettings::default();
    settings.sidebar.initial_visibility = SidebarVisibility::Hidden;
    deliver_settings(&mut application, settings);
    application
}

/// Opens `session_id`, parented where `parent` says, as the ordinary attach
/// route lands it.
fn open(
    application: &mut Application,
    workspace: &std::path::Path,
    session_id: SessionId,
    parent: Option<SessionId>,
) {
    let mut snapshot = failed_session_snapshot(session_id, PromptId::new(), "Open work", workspace);
    snapshot.session.parent = parent;
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("open a Session");
}

fn deliver_tree(application: &mut Application, through: SessionId, event: SubagentTreeEvent) {
    deliver_tree_through(application, local(through), event);
}

fn deliver_tree_through(
    application: &mut Application,
    through: SessionReference,
    event: SubagentTreeEvent,
) {
    application
        .handle_event(ApplicationEvent::SubagentTree { through, event })
        .expect("take a Subagent tree event");
}

/// The Aside's own content columns of each rendered row: past its rule and
/// the column of padding inside it.
fn aside_rows(application: &Application, width: u16) -> Vec<String> {
    rendered_application_rows_at(application, width, HEIGHT)
        .iter()
        .map(|row| {
            row.chars()
                .skip(usize::from(width - ASIDE_WIDTH + 2))
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

fn aside_text(application: &Application) -> String {
    aside_rows(application, WIDTH).join("\n")
}

/// The screen position of an Aside row's text.
fn aside_row_position(application: &Application, needle: &str) -> (u16, u16) {
    let rows = aside_rows(application, WIDTH);
    let row = rows
        .iter()
        .position(|row| row.contains(needle))
        .unwrap_or_else(|| panic!("the Aside draws {needle:?}: {rows:#?}"));
    (
        WIDTH - ASIDE_WIDTH + 4,
        u16::try_from(row).expect("row fits"),
    )
}

fn click(application: &mut Application, (column, row): (u16, u16)) -> ApplicationTransition {
    click_mouse(
        application,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        },
    )
    .expect("click the Aside")
}

fn press_leader_chord(application: &mut Application, key: char) {
    for key in [
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE),
    ] {
        application
            .handle_terminal_event(InputEvent::Key(key))
            .expect("press a Leader chord");
    }
}

/// A client with the fixture tree's top-level Session open and its tree in
/// hand.
fn tree_open_at_top(workspace: &std::path::Path) -> (Application, Tree) {
    let tree = Tree::new();
    let mut application = client(workspace);
    open(&mut application, workspace, tree.top, None);
    deliver_tree(
        &mut application,
        tree.top,
        SubagentTreeEvent::Snapshot(tree.snapshot()),
    );
    (application, tree)
}

#[test]
fn the_aside_stands_beside_an_open_session_and_is_absent_on_the_landing() {
    let workspace = workspace_dir();
    let mut application = client(workspace.path());
    let landing = rendered_application_rows_at(&application, WIDTH, HEIGHT).join("\n");
    assert!(
        !landing.contains("Subagents"),
        "nothing is open on the Landing, so the Aside is absent: {landing}"
    );

    enter_session(&mut application, workspace.path());
    let rows = aside_rows(&application, WIDTH);
    assert_eq!(
        rows[0], "Subagents",
        "an open Session has the Aside beside it, headed by its one Section: {rows:#?}"
    );
    let frame = rendered_application_rows_at(&application, WIDTH, HEIGHT);
    assert_eq!(
        frame[0].chars().nth(usize::from(WIDTH - ASIDE_WIDTH)),
        Some('│'),
        "the Aside's rule stands down its left, facing the main view"
    );
}

#[test]
fn entries_run_depth_first_with_guides_marker_name_title_and_a_final_or_blank_duration() {
    let workspace = workspace_dir();
    let (application, _) = tree_open_at_top(workspace.path());

    let rows = aside_rows(&application, WIDTH);
    assert_eq!(
        rows[..5],
        [
            "Subagents 3",
            "Map every seam",
            "├ ✓ Explore Map the prov… 12s",
            "│ └ ⠋ Review Check the mappe…",
            "└ ✓ Plan Weigh the options",
        ],
        "the top-level Session heads the tree, each Subagent follows its spawner in \
         spawn order, and a settle with no known duration leaves the time blank: {rows:#?}"
    );
}

#[test]
fn the_title_truncates_before_the_name() {
    let workspace = workspace_dir();
    let tree = Tree::new();
    let mut application = client(workspace.path());
    open(&mut application, workspace.path(), tree.top, None);
    let mut snapshot = tree.snapshot();
    snapshot.subagents.truncate(1);
    snapshot.subagents[0].name = "Cartographer".to_owned();
    snapshot.subagents[0].title = "Chart every Provider seam the orchestration crosses".to_owned();
    deliver_tree(
        &mut application,
        tree.top,
        SubagentTreeEvent::Snapshot(snapshot),
    );

    let rows = aside_rows(&application, WIDTH);
    assert_eq!(
        rows[2], "└ ✓ Cartographer Chart e… 12s",
        "the name stands whole while the Title gives way: {rows:#?}"
    );
}

#[test]
fn a_session_with_no_subagents_lists_only_its_own_entry() {
    let workspace = workspace_dir();
    let tree = Tree::new();
    let mut application = client(workspace.path());
    open(&mut application, workspace.path(), tree.top, None);
    let mut snapshot = tree.snapshot();
    snapshot.subagents.clear();
    deliver_tree(
        &mut application,
        tree.top,
        SubagentTreeEvent::Snapshot(snapshot),
    );

    let rows = aside_rows(&application, WIDTH);
    assert_eq!(rows[..3], ["Subagents 0", "Map every seam", ""]);
}

#[test]
fn the_open_entry_is_highlighted_by_its_title_alone() {
    let workspace = workspace_dir();
    let (mut application, tree) = tree_open_at_top(workspace.path());
    let title_cell = |application: &Application, needle: &str| {
        let (column, row) = aside_row_position(application, needle);
        let rows = aside_rows(application, WIDTH);
        let offset = rows[usize::from(row)]
            .find(needle)
            .expect("the entry draws its Title");
        let offset = rows[usize::from(row)][..offset].chars().count();
        let buffer = rendered_application_buffer(application, WIDTH, HEIGHT);
        buffer[(column - 2 + u16::try_from(offset).expect("fits"), row)].clone()
    };

    assert_eq!(title_cell(&application, "Map every seam").fg, Color::Cyan);
    assert_ne!(
        title_cell(&application, "Weigh the options").fg,
        Color::Cyan,
        "only the open entry is accented"
    );

    open(
        &mut application,
        workspace.path(),
        tree.plan,
        Some(tree.top),
    );
    assert_eq!(
        title_cell(&application, "Weigh the options").fg,
        Color::Cyan,
        "the highlight moves with the open Session"
    );
    assert_ne!(title_cell(&application, "Map every seam").fg, Color::Cyan);
    let (column, row) = aside_row_position(&application, "Weigh the options");
    let buffer = rendered_application_buffer(&application, WIDTH, HEIGHT);
    assert_ne!(
        buffer[(column - 2, row)].bg,
        Color::Cyan,
        "the highlight repaints nothing but the Title"
    );
}

#[test]
fn a_settle_never_moves_an_entry() {
    let workspace = workspace_dir();
    let (mut application, tree) = tree_open_at_top(workspace.path());
    deliver_tree(
        &mut application,
        tree.top,
        SubagentTreeEvent::Changed(SubagentTreeChange::SubagentSettled {
            session_id: tree.review,
            status: ActivityStatus::Failed,
            duration_ms: Some(3_000),
        }),
    );

    let rows = aside_rows(&application, WIDTH);
    assert_eq!(
        rows[2..5],
        [
            "├ ✓ Explore Map the prov… 12s",
            "│ └ × Review Check the ma… 3s",
            "└ ✓ Plan Weigh the options",
        ],
        "the settled entry wears its outcome and final time where it stood: {rows:#?}"
    );
}

#[test]
fn a_late_spawn_stands_beneath_its_spawner() {
    let workspace = workspace_dir();
    let (mut application, tree) = tree_open_at_top(workspace.path());
    let probe = SessionId::new();
    deliver_tree(
        &mut application,
        tree.top,
        SubagentTreeEvent::Changed(SubagentTreeChange::SubagentSpawned {
            entry: entry(
                probe,
                tree.explore,
                1,
                ("Probe", "Probe a seam"),
                ActivityStatus::Active,
                None,
            ),
        }),
    );

    let rows = aside_rows(&application, WIDTH);
    assert_eq!(rows[0], "Subagents 4");
    assert_eq!(
        rows[3..6],
        [
            "│ ├ ⠋ Review Check the mappe…",
            "│ └ ⠋ Probe Probe a seam",
            "└ ✓ Plan Weigh the options",
        ],
        "{rows:#?}"
    );
}

#[test]
fn clicking_an_entry_attaches_its_session_and_the_open_entry_does_nothing() {
    let workspace = workspace_dir();
    let (mut application, tree) = tree_open_at_top(workspace.path());

    let open_entry = aside_row_position(&application, "Map every seam");
    assert_eq!(
        click(&mut application, open_entry),
        ApplicationTransition::Continue,
        "choosing the open entry does nothing"
    );
    let review = aside_row_position(&application, "Review");
    assert_eq!(
        click(&mut application, review),
        ApplicationTransition::AttachSession(local(tree.review)),
        "a nested Subagent's entry opens its Session through the ordinary attach route"
    );
}

#[test]
fn from_a_nested_subagent_the_top_level_entry_attaches_the_top_level_session() {
    let workspace = workspace_dir();
    let (mut application, tree) = tree_open_at_top(workspace.path());
    open(
        &mut application,
        workspace.path(),
        tree.review,
        Some(tree.explore),
    );

    let top = aside_row_position(&application, "Map every seam");
    assert_eq!(
        click(&mut application, top),
        ApplicationTransition::AttachSession(local(tree.top))
    );
    let review = aside_row_position(&application, "Review");
    assert_eq!(
        click(&mut application, review),
        ApplicationTransition::Continue,
        "the open entry is the nested Subagent now"
    );
}

#[test]
fn switching_within_a_tree_keeps_the_section_and_another_tree_does_not_borrow_it() {
    let workspace = workspace_dir();
    let (mut application, tree) = tree_open_at_top(workspace.path());
    let before = aside_rows(&application, WIDTH);

    open(
        &mut application,
        workspace.path(),
        tree.explore,
        Some(tree.top),
    );
    assert_eq!(
        aside_rows(&application, WIDTH)[..5],
        before[..5],
        "moving to a Session of the same tree leaves the Section standing"
    );

    // A change still arriving through the subscription the tree came from is
    // taken, wherever in the tree the reader now stands.
    deliver_tree(
        &mut application,
        tree.top,
        SubagentTreeEvent::Changed(SubagentTreeChange::TopLevelRetitled {
            title: "Map every Provider seam".to_owned(),
        }),
    );
    assert_eq!(
        aside_rows(&application, WIDTH)[1],
        "Map every Provider seam"
    );

    let elsewhere = SessionId::new();
    open(&mut application, workspace.path(), elsewhere, None);
    let rows = aside_rows(&application, WIDTH);
    assert_eq!(
        rows[..2],
        ["Subagents", ""],
        "another tree's Session is not answered with this tree: {rows:#?}"
    );
    deliver_tree(
        &mut application,
        tree.top,
        SubagentTreeEvent::Changed(SubagentTreeChange::TopLevelRetitled {
            title: "Late".to_owned(),
        }),
    );
    assert!(
        !aside_text(&application).contains("Late"),
        "a late change from the tree left behind is dropped"
    );
}

#[test]
fn ctrl_x_a_and_slash_aside_follow_the_three_way_rule() {
    let workspace = workspace_dir();
    let (mut application, _) = tree_open_at_top(workspace.path());
    let rule = |application: &Application| {
        rendered_application_buffer(application, WIDTH, HEIGHT)[(WIDTH - ASIDE_WIDTH, 0)].fg
    };
    let resting = rule(&application);

    press_leader_chord(&mut application, 'a');
    assert!(
        aside_text(&application).contains("Subagents"),
        "a shown Aside without the keys stays shown"
    );
    assert_ne!(rule(&application), resting, "and takes the keys");
    type_terminal_text(&mut application, "zq");
    assert!(
        !rendered_application_rows_at(&application, WIDTH, HEIGHT)
            .join("\n")
            .contains("zq"),
        "text typed while the Aside holds the keys reaches no composer"
    );

    press_leader_chord(&mut application, 'a');
    assert!(
        !aside_text(&application).contains("Subagents"),
        "an Aside holding the keys hides"
    );

    press_leader_chord(&mut application, 'a');
    assert!(
        aside_text(&application).contains("Subagents"),
        "a hidden Aside shows"
    );
    assert_ne!(rule(&application), resting, "and takes the keys with it");

    key(&mut application, KeyCode::Esc);
    assert!(aside_text(&application).contains("Subagents"));
    assert_eq!(
        rule(&application),
        resting,
        "Esc hands the keys back and leaves the Aside shown"
    );

    type_terminal_text(&mut application, "/aside");
    key(&mut application, KeyCode::Enter);
    assert!(aside_text(&application).contains("Subagents"));
    assert_ne!(
        rule(&application),
        resting,
        "/aside takes the keys for a shown Aside, as Ctrl+X A does"
    );
    assert_eq!(
        invoke(&mut application, SemanticCommandId::AsideToggle),
        ApplicationTransition::Continue
    );
    assert!(
        !aside_text(&application).contains("Subagents"),
        "and the same command hides it while it holds them"
    );
}

#[test]
fn the_aside_and_the_sidebar_never_both_hold_the_keys() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    deliver_settings(&mut application, EffectiveSettings::default());
    enter_session(&mut application, workspace.path());
    let width = 140;
    let aside_rule = |application: &Application| {
        rendered_application_buffer(application, width, HEIGHT)[(width - ASIDE_WIDTH, 0)].fg
    };
    let resting = aside_rule(&application);

    invoke(&mut application, SemanticCommandId::AsideToggle);
    assert_ne!(
        aside_rule(&application),
        resting,
        "the Aside takes the keys"
    );

    // Ctrl+B from inside the Aside hands them to the Sidebar.
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('b'),
            KeyModifiers::CONTROL,
        )))
        .expect("press Ctrl+B");
    assert_eq!(
        aside_rule(&application),
        resting,
        "the Sidebar taking the keys takes them from the Aside"
    );

    // Ctrl+X A from inside the Sidebar takes them back.
    press_leader_chord(&mut application, 'a');
    assert_ne!(aside_rule(&application), resting);
    type_terminal_text(&mut application, "zq");
    assert!(
        !rendered_application_rows_at(&application, width, HEIGHT)
            .join("\n")
            .contains("zq"),
        "the Sidebar let the keys go: typing searches nothing there"
    );
}

#[test]
fn the_aside_is_squeezed_out_before_the_sidebar() {
    let workspace = workspace_dir();
    let mut application = connected_application(workspace.path());
    deliver_settings(&mut application, EffectiveSettings::default());
    enter_session(&mut application, workspace.path());

    let both = rendered_application_rows_at(&application, 118, HEIGHT);
    assert_eq!(both[0].chars().nth(118 - 32), Some('│'), "{both:#?}");
    assert!(both.join("\n").contains("Subagents"));

    let squeezed = rendered_application_rows_at(&application, 109, HEIGHT);
    assert!(
        !squeezed.join("\n").contains("Subagents"),
        "a terminal too narrow for all three drops the Aside first"
    );
    assert_eq!(
        squeezed[0].chars().nth(31),
        Some('│'),
        "and keeps the Sidebar at its width"
    );

    assert!(
        rendered_application_rows_at(&application, 118, HEIGHT)
            .join("\n")
            .contains("Subagents"),
        "growing the terminal brings the Aside back"
    );
}

#[test]
fn the_edge_drags_and_a_double_click_resets_the_width() {
    use crate::selection::mouse;

    let workspace = workspace_dir();
    let (mut application, _) = tree_open_at_top(workspace.path());
    let rule_column = |application: &Application| {
        rendered_application_rows_at(application, WIDTH, HEIGHT)[0]
            .chars()
            .position(|character| character == '│')
    };
    assert_eq!(rule_column(&application), Some(usize::from(WIDTH - 32)));
    rendered_application_buffer(&application, WIDTH, HEIGHT);

    mouse(
        &mut application,
        MouseEventKind::Down(MouseButton::Left),
        (WIDTH - 32, 10),
    );
    assert_eq!(
        mouse(
            &mut application,
            MouseEventKind::Drag(MouseButton::Left),
            (WIDTH - 40, 10),
        ),
        ApplicationTransition::Continue,
        "a pointer resize is view state"
    );
    let held = rendered_application_buffer(&application, WIDTH, HEIGHT);
    assert_eq!(held[(WIDTH - 40, 0)].fg, Color::Cyan, "the held edge");
    mouse(
        &mut application,
        MouseEventKind::Up(MouseButton::Left),
        (WIDTH - 40, 10),
    );
    assert_eq!(rule_column(&application), Some(usize::from(WIDTH - 40)));

    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        mouse(&mut application, kind, (WIDTH - 40, 10));
    }
    assert_eq!(
        rule_column(&application),
        Some(usize::from(WIDTH - 32)),
        "a double-click on the edge returns the Aside to its launch width"
    );

    invoke(
        &mut application,
        SemanticCommandId::AsideWidthSet { columns: 50 },
    );
    assert_eq!(rule_column(&application), Some(usize::from(WIDTH - 50)));
    invoke(&mut application, SemanticCommandId::AsideWidthReset);
    assert_eq!(rule_column(&application), Some(usize::from(WIDTH - 32)));
}

/// A top-level Session whose active Turn spawned one working Subagent.
fn parent_with_working_subagent(workspace: &std::path::Path) -> (SessionSnapshot, SessionId) {
    let mut snapshot = failed_session_snapshot(
        SessionId::new(),
        PromptId::new(),
        "Delegate the mapping",
        workspace,
    );
    let turn_id = snapshot.turns[0].id;
    snapshot.session.status = SessionStatus::Active;
    snapshot.turns[0].status = TurnStatus::Active;
    snapshot.session.working_since = Some(SessionTimestamp::now());
    let child = SessionId::new();
    let activity_id = snapshot.activities[0].id();
    snapshot.activities[0] = Activity::Subagent {
        id: activity_id,
        turn_id,
        status: ActivityStatus::Active,
        name: "Explore".to_owned(),
        description: "Map the provider seams".to_owned(),
        model: None,
        session_id: child,
        duration_ms: None,
    };
    (snapshot, child)
}

#[test]
fn the_subagent_picker_and_esc_to_parent_are_unchanged_beside_the_aside() {
    let workspace = workspace_dir();
    let mut application = client(workspace.path());
    let (parent, child) = parent_with_working_subagent(workspace.path());
    let parent_id = parent.session.id;
    application
        .handle_event(ApplicationEvent::SessionAttached(parent))
        .expect("open the parent");
    assert!(aside_text(&application).contains("Subagents"));

    key(&mut application, KeyCode::Down);
    assert_eq!(
        key(&mut application, KeyCode::Enter),
        ApplicationTransition::AttachSession(local(child)),
        "Down still docks the Subagent Picker and Enter opens its entry"
    );

    open(&mut application, workspace.path(), child, Some(parent_id));
    assert_eq!(
        key(&mut application, KeyCode::Esc),
        ApplicationTransition::ViewAndAttachSession(local(parent_id)),
        "Esc in a Subagent's Session still returns to its parent"
    );
}

#[test]
fn on_the_landing_the_toggle_only_shows_and_hides() {
    let workspace = workspace_dir();
    let mut application = client(workspace.path());

    press_leader_chord(&mut application, 'a');
    enter_session(&mut application, workspace.path());
    assert!(
        !aside_text(&application).contains("Subagents"),
        "on the Landing the act hid the Aside the Settings showed"
    );

    let mut application = client(workspace.path());
    press_leader_chord(&mut application, 'a');
    press_leader_chord(&mut application, 'a');
    enter_session(&mut application, workspace.path());
    let rows = rendered_application_rows_at(&application, WIDTH, HEIGHT);
    let resting =
        rendered_application_buffer(&application, WIDTH, HEIGHT)[(WIDTH - ASIDE_WIDTH, 0)].fg;
    assert!(
        aside_text(&application).contains("Subagents"),
        "and showed it again: {rows:#?}"
    );
    type_terminal_text(&mut application, "zq");
    assert!(
        rendered_application_rows_at(&application, WIDTH, HEIGHT)
            .join("\n")
            .contains("zq"),
        "no claim on the keys was left behind to take them once a Session opened"
    );
    press_leader_chord(&mut application, 'a');
    assert!(aside_text(&application).contains("Subagents"));
    assert_ne!(
        rendered_application_buffer(&application, WIDTH, HEIGHT)[(WIDTH - ASIDE_WIDTH, 0)].fg,
        resting,
        "beside a Session the act takes the keys as the three-way rule says"
    );
}

#[test]
fn in_a_subagents_session_the_leader_reaches_the_aside_and_nothing_else() {
    let workspace = workspace_dir();
    let (mut application, tree) = tree_open_at_top(workspace.path());
    open(
        &mut application,
        workspace.path(),
        tree.review,
        Some(tree.explore),
    );
    let before = rendered_application_rows_at(&application, WIDTH, HEIGHT);

    for key in [
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE),
    ] {
        assert_eq!(
            application
                .handle_terminal_event(InputEvent::Key(key))
                .expect("press Ctrl+X M"),
            ApplicationTransition::Continue,
            "the Model Picker is not offered in a Session that is read, never prompted"
        );
    }
    assert_eq!(
        rendered_application_rows_at(&application, WIDTH, HEIGHT),
        before,
        "Ctrl+X M changes nothing in a Subagent's Session"
    );
    assert_eq!(
        key(&mut application, KeyCode::Esc),
        ApplicationTransition::ViewAndAttachSession(local(tree.explore)),
        "and the ended chord leaves Esc returning to the parent"
    );

    open(
        &mut application,
        workspace.path(),
        tree.review,
        Some(tree.explore),
    );
    press_leader_chord(&mut application, 'a');
    press_leader_chord(&mut application, 'a');
    assert!(
        !aside_text(&application).contains("Subagents"),
        "Ctrl+X A takes the keys and then hides the Aside from inside it"
    );
}

// Arriving, failing, deletion and an Unreachable Remote: the Section stays
// trustworthy whatever the per-tree subscription is doing.

/// A client drawing under a presentation clock the test moves by hand, so the
/// Section's 300 ms quiet period passes without being waited out.
fn clocked_client(
    workspace: &std::path::Path,
) -> (
    Application,
    std::sync::Arc<std::sync::Mutex<std::time::Instant>>,
) {
    let now = std::sync::Arc::new(std::sync::Mutex::new(std::time::Instant::now()));
    let clock = std::sync::Arc::clone(&now);
    let mut application = connected_application(workspace)
        .with_presentation_clock(move || *clock.lock().expect("read the presentation clock"));
    let mut settings = EffectiveSettings::default();
    settings.sidebar.initial_visibility = SidebarVisibility::Hidden;
    deliver_settings(&mut application, settings);
    (application, now)
}

fn advance(now: &std::sync::Mutex<std::time::Instant>, milliseconds: u64) {
    *now.lock().expect("advance the presentation clock") +=
        std::time::Duration::from_millis(milliseconds);
}

#[test]
fn a_tree_still_arriving_is_blank_and_then_says_loading() {
    let workspace = workspace_dir();
    let (mut application, now) = clocked_client(workspace.path());
    let tree = Tree::new();
    open(&mut application, workspace.path(), tree.top, None);

    assert_eq!(
        aside_rows(&application, WIDTH)[..2],
        ["Subagents", ""],
        "a tree not yet in hand is drawn blank"
    );
    advance(&now, 299);
    assert_eq!(
        aside_rows(&application, WIDTH)[1],
        "",
        "still quiet at 299 ms"
    );
    advance(&now, 1);
    assert_eq!(
        aside_rows(&application, WIDTH)[1],
        "Loading",
        "Loading once the quiet period has passed"
    );

    deliver_tree(
        &mut application,
        tree.top,
        SubagentTreeEvent::Snapshot(tree.snapshot()),
    );
    assert_eq!(aside_rows(&application, WIDTH)[1], "Map every seam");
}

#[test]
fn a_fast_arrival_never_flashes_loading_and_moving_within_the_tree_never_blanks() {
    let workspace = workspace_dir();
    let (mut application, now) = clocked_client(workspace.path());
    let tree = Tree::new();
    open(&mut application, workspace.path(), tree.top, None);
    aside_rows(&application, WIDTH);
    advance(&now, 200);
    deliver_tree(
        &mut application,
        tree.top,
        SubagentTreeEvent::Snapshot(tree.snapshot()),
    );
    let arrived = aside_rows(&application, WIDTH);
    assert_eq!(arrived[1], "Map every seam");

    advance(&now, 400);
    assert_eq!(
        aside_rows(&application, WIDTH),
        arrived,
        "a tree that arrived inside the quiet period never says Loading"
    );

    open(
        &mut application,
        workspace.path(),
        tree.review,
        Some(tree.explore),
    );
    assert_eq!(aside_rows(&application, WIDTH)[..5], arrived[..5]);
    advance(&now, 400);
    assert_eq!(
        aside_rows(&application, WIDTH)[..5],
        arrived[..5],
        "moving to another Session of the tree neither blanks nor loads it"
    );
}

#[test]
fn a_tree_that_cannot_be_read_says_so_until_a_session_of_it_is_opened_again() {
    let workspace = workspace_dir();
    let (mut application, now) = clocked_client(workspace.path());
    let tree = Tree::new();
    open(&mut application, workspace.path(), tree.top, None);
    deliver_tree(
        &mut application,
        tree.top,
        SubagentTreeEvent::Failed("Session does not exist on this server instance".to_owned()),
    );

    let rows = aside_rows(&application, WIDTH);
    assert_eq!(
        rows[1..4],
        [
            "Error: Could not load",
            "Subagents: Session does not",
            "exist on this server instance",
        ],
        "the Section says why it has no tree: {rows:#?}"
    );
    let buffer = rendered_application_buffer(&application, WIDTH, HEIGHT);
    assert_eq!(
        buffer[(WIDTH - ASIDE_WIDTH + 2, 1)].fg,
        Color::Red,
        "in the Theme's error colour"
    );
    advance(&now, 400);
    assert_eq!(
        aside_rows(&application, WIDTH),
        rows,
        "the error line stands rather than giving way to Loading"
    );

    // The ended subscription is not asked again while the reader stays put:
    // a snapshot still in flight from it is not taken.
    deliver_tree(
        &mut application,
        tree.top,
        SubagentTreeEvent::Snapshot(tree.snapshot()),
    );
    assert_eq!(aside_rows(&application, WIDTH), rows);

    // Opening a Session of the tree asks for it again.
    open(&mut application, workspace.path(), tree.top, None);
    assert_eq!(
        aside_rows(&application, WIDTH)[1],
        "",
        "the retry begins quiet, as any tree still arriving does"
    );
    deliver_tree(
        &mut application,
        tree.top,
        SubagentTreeEvent::Snapshot(tree.snapshot()),
    );
    assert_eq!(
        aside_rows(&application, WIDTH)[..2],
        ["Subagents 3", "Map every seam"],
        "the retried subscription's tree is taken"
    );
}

#[test]
fn a_tree_lost_while_in_hand_is_retried_from_any_session_of_it() {
    let workspace = workspace_dir();
    let (mut application, tree) = tree_open_at_top(workspace.path());
    deliver_tree(
        &mut application,
        tree.top,
        SubagentTreeEvent::Failed("Remote revoked this Pairing".to_owned()),
    );
    assert!(aside_text(&application).contains("Error: Could not load"));

    open(
        &mut application,
        workspace.path(),
        tree.review,
        Some(tree.explore),
    );
    assert!(
        !aside_text(&application).contains("Error"),
        "opening a Session of the lost tree asks for it again"
    );
    deliver_tree(
        &mut application,
        tree.review,
        SubagentTreeEvent::Snapshot(tree.snapshot()),
    );
    assert_eq!(aside_rows(&application, WIDTH)[1], "Map every seam");
}

#[test]
fn a_deleted_tree_is_dropped_without_an_error_line() {
    let workspace = workspace_dir();
    let (mut application, now) = clocked_client(workspace.path());
    let tree = Tree::new();
    open(&mut application, workspace.path(), tree.top, None);
    deliver_tree(
        &mut application,
        tree.top,
        SubagentTreeEvent::Snapshot(tree.snapshot()),
    );
    deliver_tree(&mut application, tree.top, SubagentTreeEvent::Deleted);

    let rows = aside_rows(&application, WIDTH);
    assert_eq!(rows[..3], ["Subagents", "", ""], "{rows:#?}");
    advance(&now, 400);
    assert!(
        !aside_text(&application).contains("Loading")
            && !aside_text(&application).contains("Error"),
        "a deleted tree neither loads again nor complains"
    );
}

#[test]
fn an_unreachable_remote_leaves_the_last_tree_standing() {
    let mut application = crate::support::application_looking_at_studio();
    let mut settings = EffectiveSettings::default();
    settings.sidebar.initial_visibility = SidebarVisibility::Hidden;
    deliver_settings(&mut application, settings);
    let tree = Tree::new();
    let mut snapshot = failed_session_snapshot(
        tree.top,
        PromptId::new(),
        "Open work",
        std::path::Path::new("."),
    );
    snapshot.session.parent = None;
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("open the Remote Session");
    let studio = Outlook::Remote("studio".to_owned());
    deliver_tree_through(
        &mut application,
        SessionReference::new(studio.clone(), tree.top),
        SubagentTreeEvent::Snapshot(tree.snapshot()),
    );
    let before = aside_rows(&application, WIDTH);
    assert_eq!(before[1], "Map every seam");

    crate::support::studio_stops_answering(
        &mut application,
        1,
        std::time::Duration::from_millis(5),
    );
    crate::support::grace_elapses(&mut application, studio);
    let rows = aside_rows(&application, WIDTH);
    assert!(
        rows[..5] == before[..5],
        "the tree the Remote last gave stands while it is Unreachable: {rows:#?}"
    );
}

// How the Aside begins: its two Settings, adopted once at launch and then the
// reader's to overrule.

fn aside_settings(visibility: AsideVisibility, initial_width: u64) -> AsideSettings {
    AsideSettings {
        initial_visibility: visibility,
        initial_width,
    }
}

/// Delivers a Settings snapshot carrying `aside`, with the Sidebar kept off
/// the frame so the Aside is the only column beside the main view.
fn deliver_aside_settings(application: &mut Application, aside: AsideSettings) {
    let mut settings = EffectiveSettings::default();
    settings.sidebar.initial_visibility = SidebarVisibility::Hidden;
    settings.aside = aside;
    deliver_settings(application, settings);
}

fn launched_with(workspace: &std::path::Path, aside: AsideSettings) -> Application {
    let mut application = connected_application(workspace);
    deliver_aside_settings(&mut application, aside);
    enter_session(&mut application, workspace);
    application
}

/// The column the Aside's rule stands at in a frame this wide, or `None`
/// where the frame draws no Aside.
fn aside_rule_column(application: &Application, width: u16) -> Option<usize> {
    let rows = rendered_application_rows_at(application, width, HEIGHT);
    rows[0]
        .chars()
        .position(|character| character == '│')
        .filter(|_| rows.join("\n").contains("Subagents"))
}

#[test]
fn an_aside_the_setting_hides_is_absent_until_the_reader_asks_for_it() {
    let workspace = workspace_dir();
    let mut application = launched_with(
        workspace.path(),
        aside_settings(AsideVisibility::Hidden, 32),
    );
    assert_eq!(
        aside_rule_column(&application, WIDTH),
        None,
        "the first frame honours the initial-visibility Setting"
    );

    invoke(&mut application, SemanticCommandId::AsideToggle);
    assert_eq!(
        aside_rule_column(&application, WIDTH),
        Some(usize::from(WIDTH - 32)),
        "the toggle overrides the Setting for this run"
    );
}

#[test]
fn the_setting_reveals_the_aside_without_taking_the_keys() {
    let workspace = workspace_dir();
    let mut application =
        launched_with(workspace.path(), aside_settings(AsideVisibility::Shown, 32));
    assert_eq!(
        aside_rule_column(&application, WIDTH),
        Some(usize::from(WIDTH - 32))
    );
    type_terminal_text(&mut application, "zq");
    assert!(
        rendered_application_rows_at(&application, WIDTH, HEIGHT)
            .join("\n")
            .contains("zq"),
        "a reader who has not touched the Aside is still typing into the composer"
    );
}

#[test]
fn a_later_settings_snapshot_leaves_the_readers_own_choice_alone() {
    let workspace = workspace_dir();

    let mut hidden_by_the_reader =
        launched_with(workspace.path(), aside_settings(AsideVisibility::Shown, 32));
    invoke(&mut hidden_by_the_reader, SemanticCommandId::AsideToggle);
    invoke(&mut hidden_by_the_reader, SemanticCommandId::AsideToggle);
    assert_eq!(aside_rule_column(&hidden_by_the_reader, WIDTH), None);
    deliver_aside_settings(
        &mut hidden_by_the_reader,
        aside_settings(AsideVisibility::Shown, 32),
    );
    assert_eq!(
        aside_rule_column(&hidden_by_the_reader, WIDTH),
        None,
        "another Setting's edit does not reopen an Aside the reader hid"
    );

    let mut shown_by_the_reader = launched_with(
        workspace.path(),
        aside_settings(AsideVisibility::Hidden, 32),
    );
    invoke(&mut shown_by_the_reader, SemanticCommandId::AsideToggle);
    deliver_aside_settings(
        &mut shown_by_the_reader,
        aside_settings(AsideVisibility::Hidden, 32),
    );
    assert_eq!(
        aside_rule_column(&shown_by_the_reader, WIDTH),
        Some(usize::from(WIDTH - 32)),
        "nor does it put away an Aside the reader showed"
    );
}

#[test]
fn the_launch_setting_chooses_the_aside_width_clamped_by_both_floors() {
    let workspace = workspace_dir();
    let application = launched_with(workspace.path(), aside_settings(AsideVisibility::Shown, 40));
    assert_eq!(
        aside_rule_column(&application, WIDTH),
        Some(usize::from(WIDTH - 40)),
        "the Aside begins at the configured width"
    );

    let wide = launched_with(workspace.path(), aside_settings(AsideVisibility::Shown, 60));
    assert_eq!(
        aside_rule_column(&wide, 100),
        Some(54),
        "the main view keeps its 54-column floor"
    );
    assert_eq!(
        aside_rule_column(&wide, 78),
        Some(54),
        "the Aside keeps its 24-column floor"
    );
    assert_eq!(
        aside_rule_column(&wide, 140),
        Some(80),
        "drawing narrow never overwrites the configured width"
    );
}

#[test]
fn a_changed_launch_setting_moves_nothing_but_the_width_a_reset_returns_to() {
    let workspace = workspace_dir();
    let mut application =
        launched_with(workspace.path(), aside_settings(AsideVisibility::Shown, 40));
    deliver_aside_settings(&mut application, aside_settings(AsideVisibility::Shown, 50));
    assert_eq!(
        aside_rule_column(&application, WIDTH),
        Some(usize::from(WIDTH - 40)),
        "a later delivery leaves the live width alone"
    );

    invoke(&mut application, SemanticCommandId::AsideWidthReset);
    assert_eq!(
        aside_rule_column(&application, WIDTH),
        Some(usize::from(WIDTH - 50)),
        "a reset returns to the launch width Setting currently delivered"
    );
}

// Live state: how long work has been running, the top-level Session's
// Working, and which Session waits on an Intervention.

/// When the fixture's clock starts, in milliseconds since the Unix epoch.
const CLOCK_START: u64 = 1_800_000_000_000;

/// A client whose Session clock the test moves by hand, so a time can be seen
/// advancing without being waited out.
fn session_clocked_client(
    workspace: &std::path::Path,
) -> (Application, std::sync::Arc<std::sync::Mutex<u64>>) {
    let now = std::sync::Arc::new(std::sync::Mutex::new(CLOCK_START));
    let clock = std::sync::Arc::clone(&now);
    let mut application = connected_application(workspace).with_session_clock(move || {
        SessionTimestamp(*clock.lock().expect("read the Session clock"))
    });
    let mut settings = EffectiveSettings::default();
    settings.sidebar.initial_visibility = SidebarVisibility::Hidden;
    deliver_settings(&mut application, settings);
    (application, now)
}

fn advance_session_clock(now: &std::sync::Mutex<u64>, milliseconds: u64) {
    *now.lock().expect("advance the Session clock") += milliseconds;
}

/// A moment `milliseconds` before the fixture's clock starts.
const fn before_start(milliseconds: u64) -> SessionTimestamp {
    SessionTimestamp(CLOCK_START - milliseconds)
}

/// The Aside row naming `needle`.
fn aside_row(application: &Application, needle: &str) -> String {
    let rows = aside_rows(application, WIDTH);
    rows.iter()
        .find(|row| row.contains(needle))
        .unwrap_or_else(|| panic!("the Aside draws {needle:?}: {rows:#?}"))
        .clone()
}

/// The colour "Needs Intervention" is drawn in on the Aside row at `row`.
fn needs_intervention_colour(application: &Application, row: usize) -> Option<Color> {
    let buffer = rendered_application_buffer(application, WIDTH, HEIGHT);
    let line = aside_rows(application, WIDTH)[row].clone();
    let column = line.find("Needs Intervention")?;
    let x = WIDTH - ASIDE_WIDTH
        + 2
        + u16::try_from(line[..column].chars().count()).expect("column fits");
    buffer
        .cell((x, u16::try_from(row).expect("row fits")))?
        .fg
        .into()
}

fn change(application: &mut Application, through: SessionId, change: SubagentTreeChange) {
    deliver_tree(application, through, SubagentTreeEvent::Changed(change));
}

#[test]
fn a_working_entry_ticks_from_when_its_work_began_and_stands_at_its_final_duration() {
    let workspace = workspace_dir();
    let (mut application, now) = session_clocked_client(workspace.path());
    let tree = Tree::new();
    open(&mut application, workspace.path(), tree.top, None);
    let mut snapshot = tree.snapshot();
    snapshot.subagents[1].started_at = Some(before_start(5_000));
    deliver_tree(
        &mut application,
        tree.top,
        SubagentTreeEvent::Snapshot(snapshot),
    );

    assert!(
        aside_row(&application, "Review").ends_with(" 5s"),
        "a working entry says how long its work has run: {:?}",
        aside_row(&application, "Review")
    );
    assert!(
        application.wants_spinner(),
        "and keeps the run loop ticking while it is on screen"
    );
    advance_session_clock(&now, 7_000);
    assert!(aside_row(&application, "Review").ends_with(" 12s"));
    advance_session_clock(&now, 78_000);
    assert!(
        aside_row(&application, "Review").ends_with(" 1m"),
        "read the way the Sidebar reads a Working duration: {:?}",
        aside_row(&application, "Review")
    );

    change(
        &mut application,
        tree.top,
        SubagentTreeChange::SubagentSettled {
            session_id: tree.review,
            status: ActivityStatus::Completed,
            duration_ms: Some(95_000),
        },
    );
    let settled = aside_row(&application, "Review");
    assert!(
        settled.ends_with(" 1m 35s") && settled.contains('✓'),
        "a settle stands the entry at the duration its work took: {settled:?}"
    );
    advance_session_clock(&now, 60_000);
    assert_eq!(
        aside_row(&application, "Review"),
        settled,
        "and the time no longer moves"
    );
    assert!(
        !application.wants_spinner(),
        "nothing in the Aside is live any more"
    );
}

#[test]
fn the_top_level_entry_wears_the_working_marker_and_time_only_while_working() {
    let workspace = workspace_dir();
    let (mut application, now) = session_clocked_client(workspace.path());
    let tree = Tree::new();
    open(&mut application, workspace.path(), tree.top, None);
    let mut snapshot = tree.snapshot();
    // Nothing else in the tree is live, so what ticks is the top level alone.
    snapshot.subagents[1].status = ActivityStatus::Completed;
    snapshot.top_level.working_since = Some(before_start(42_000));
    deliver_tree(
        &mut application,
        tree.top,
        SubagentTreeEvent::Snapshot(snapshot),
    );

    let working = aside_rows(&application, WIDTH)[1].clone();
    assert!(
        working.starts_with("⠋ Map every seam") && working.ends_with(" 42s"),
        "a Working top-level Session wears the Working Marker and its elapsed time: {working:?}"
    );
    assert!(
        application.wants_spinner(),
        "its Marker spins and its time rises"
    );
    advance_session_clock(&now, 3_000);
    assert!(aside_rows(&application, WIDTH)[1].ends_with(" 45s"));

    change(
        &mut application,
        tree.top,
        SubagentTreeChange::TopLevelWorkingChanged {
            working_since: None,
        },
    );
    assert_eq!(
        aside_rows(&application, WIDTH)[1],
        "Map every seam",
        "both clear once it stops Working"
    );
    assert!(!application.wants_spinner());

    change(
        &mut application,
        tree.top,
        SubagentTreeChange::TopLevelWorkingChanged {
            // Begun at the moment the clock now reads.
            working_since: Some(SessionTimestamp(CLOCK_START + 3_000)),
        },
    );
    advance_session_clock(&now, 2_000);
    let resumed = aside_rows(&application, WIDTH)[1].clone();
    assert!(
        resumed.starts_with("⠋ Map every seam") && resumed.ends_with(" 2s"),
        "and come back, counting afresh, when it Works again: {resumed:?}"
    );
}

#[test]
fn needs_intervention_stands_in_the_warning_colour_on_the_owning_entry_only() {
    let workspace = workspace_dir();
    let (mut application, _now) = session_clocked_client(workspace.path());
    let tree = Tree::new();
    open(&mut application, workspace.path(), tree.top, None);
    let mut snapshot = tree.snapshot();
    snapshot.top_level.working_since = Some(before_start(30_000));
    snapshot.subagents[1].started_at = Some(before_start(5_000));
    snapshot.subagents[1].needs_intervention = true;
    deliver_tree(
        &mut application,
        tree.top,
        SubagentTreeEvent::Snapshot(snapshot),
    );

    let rows = aside_rows(&application, WIDTH);
    assert!(
        rows[3].starts_with("│ └ ⠋ ") && rows[3].ends_with(" Needs Intervention"),
        "the nested Subagent whose own Session waits says so in its time's place: {rows:#?}"
    );
    assert_eq!(
        needs_intervention_colour(&application, 3),
        Some(Color::Yellow)
    );
    assert!(
        rows[2].ends_with(" 12s") && rows[1].ends_with(" 30s"),
        "while neither its spawner nor the top-level Session repeats it: {rows:#?}"
    );

    change(
        &mut application,
        tree.top,
        SubagentTreeChange::NeedsInterventionChanged {
            session_id: tree.review,
            needs_intervention: false,
        },
    );
    let answered = aside_rows(&application, WIDTH);
    assert!(
        answered[3].contains("Review") && answered[3].ends_with(" 5s"),
        "answered, the entry has its time back: {answered:#?}"
    );
    assert!(!answered.join("\n").contains("Needs Intervention"));

    change(
        &mut application,
        tree.top,
        SubagentTreeChange::NeedsInterventionChanged {
            session_id: tree.top,
            needs_intervention: true,
        },
    );
    let asked = aside_rows(&application, WIDTH);
    assert!(
        asked[1].starts_with("⠋ ") && asked[1].ends_with(" Needs Intervention"),
        "the top-level entry says so too, where the Intervention is its own: {asked:#?}"
    );
    assert_eq!(
        needs_intervention_colour(&application, 1),
        Some(Color::Yellow)
    );
    assert_eq!(
        asked
            .iter()
            .filter(|row| row.contains("Needs Intervention"))
            .count(),
        1,
        "and it is the only entry that does"
    );

    change(
        &mut application,
        tree.top,
        SubagentTreeChange::NeedsInterventionChanged {
            session_id: tree.top,
            needs_intervention: false,
        },
    );
    assert!(aside_rows(&application, WIDTH)[1].ends_with(" 30s"));
}
