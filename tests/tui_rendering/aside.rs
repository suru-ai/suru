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
        Activity, ActivityStatus, AsideSettings, AsideVisibility, EffectiveSettings, ModelId,
        Outlook, PromptId, SessionId, SessionReference, SessionSnapshot, SessionStatus,
        SessionTimestamp, SidebarVisibility, SubagentTreeChange, SubagentTreeEntry,
        SubagentTreeRevision, SubagentTreeSnapshot, SubagentTreeTopLevel, TurnStatus,
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
                own_working_since: None,
                status: None,
                worked_ms: None,
                session_id: self.top,
                title: "Map every seam".to_owned(),
                working_since: None,
                monitoring_since: None,
                needs_intervention: false,
                sidekick: false,
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
            sessions: Vec::new(),
        }
    }
}

fn entry(
    session_id: SessionId,
    parent_session_id: SessionId,
    spawn_order: u32,
    (name, title): (&str, &str),
    status: ActivityStatus,
    worked_ms: Option<u64>,
) -> SubagentTreeEntry {
    SubagentTreeEntry {
        unanswered: false,
        origin: None,
        session_id,
        parent_session_id,
        spawn_order,
        name: name.to_owned(),
        title: title.to_owned(),
        model: None,
        status,
        worked_ms,
        // Unknown unless a test says when the working Turn began.
        working_since: None,
        monitoring_since: None,
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
fn entries_run_depth_first_over_two_lines_with_guides_marker_title_name_and_a_final_or_blank_duration()
 {
    let workspace = workspace_dir();
    let (application, _) = tree_open_at_top(workspace.path());

    let rows = aside_rows(&application, WIDTH);
    assert_eq!(
        rows[..8],
        [
            "Subagents 3 (1 active)",
            "Map every seam",
            "├ ✓ Map the provider seams",
            "│ │ Explore               12s",
            "│ └ ⠋ Check the mapped seams",
            "│     Review",
            "└ ✓ Weigh the options",
            "    Plan",
        ],
        "the top-level Session heads the tree; each Subagent follows its spawner, the \
         branch still working ahead of the settled one, over two lines, its Marker and Title first and its name and time \
         beneath, the guides running on through the second line to the entries it \
         spawned; and a settle with no known duration leaves the time blank: {rows:#?}"
    );
}

/// The foreground `needle` is drawn in on the Aside row at `row`.
fn aside_colour(application: &Application, row: usize, needle: &str) -> Option<Color> {
    let buffer = rendered_application_buffer(application, WIDTH, HEIGHT);
    let line = aside_rows(application, WIDTH)[row].clone();
    let column = line.find(needle)?;
    let x = WIDTH - ASIDE_WIDTH
        + 2
        + u16::try_from(line[..column].chars().count()).expect("column fits");
    buffer
        .cell((x, u16::try_from(row).expect("row fits")))?
        .fg
        .into()
}

#[test]
fn the_header_counts_the_working_subagents_at_every_depth_in_the_working_colour() {
    let workspace = workspace_dir();
    let (mut application, tree) = tree_open_at_top(workspace.path());
    let rows = aside_rows(&application, WIDTH);
    assert_eq!(
        rows[0], "Subagents 3 (1 active)",
        "the nested working Subagent is counted beside the total: {rows:#?}"
    );
    let marker_row = rows
        .iter()
        .position(|row| row.contains("Check the mapped seams"))
        .expect("the working entry is drawn");
    assert_eq!(
        aside_colour(&application, 0, "(1 active)"),
        aside_colour(&application, marker_row, "⠋"),
        "the working count wears the working Marker's colour"
    );

    change(
        &mut application,
        tree.top,
        SubagentTreeChange::SubagentWorkingChanged {
            session_id: tree.review,
            status: ActivityStatus::Completed,
            worked_ms: Some(3_000),
            working_since: None,
            monitoring_since: None,
        },
    );
    assert_eq!(
        aside_rows(&application, WIDTH)[0],
        "Subagents 3",
        "with nothing working the header says the total alone"
    );
}

#[test]
fn the_title_has_the_first_line_and_the_name_and_time_the_second() {
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
        rows[2..4],
        [
            "└ ✓ Chart every Provider sea…",
            "    Cartographer          12s",
        ],
        "the Title has the whole of its line, and the name stands whole beneath it \
         beside the time: {rows:#?}"
    );
}

#[test]
fn a_subagents_confirmed_model_follows_its_name_and_an_unconfirmed_one_is_left_unsaid() {
    let workspace = workspace_dir();
    let tree = Tree::new();
    let mut application = client(workspace.path());
    open(&mut application, workspace.path(), tree.top, None);
    let mut snapshot = tree.snapshot();
    snapshot.subagents[0].model = Some(ModelId::new("sonnet"));
    snapshot.subagents[2].status = ActivityStatus::Failed;
    snapshot.subagents[2].model = Some(ModelId::new("opus"));
    deliver_tree(
        &mut application,
        tree.top,
        SubagentTreeEvent::Snapshot(snapshot),
    );

    let rows = aside_rows(&application, WIDTH);
    assert_eq!(
        rows[2..8],
        [
            "├ ✓ Map the provider seams",
            "│ │ Explore · sonnet      12s",
            "│ └ ⠋ Check the mapped seams",
            "│     Review",
            "└ × Weigh the options",
            "    Plan · opus · Failed",
        ],
        "the Model the Provider confirmed follows the name, before any outcome word, and \
         a Subagent whose Model is not yet known says its name alone: {rows:#?}"
    );
}

#[test]
fn a_model_confirmed_after_the_snapshot_updates_its_entry_in_place() {
    let workspace = workspace_dir();
    let (mut application, tree) = tree_open_at_top(workspace.path());
    let before = aside_rows(&application, WIDTH);
    assert_eq!(
        before[4..6],
        ["│ └ ⠋ Check the mapped seams", "│     Review"]
    );

    change(
        &mut application,
        tree.top,
        SubagentTreeChange::SubagentModelChanged {
            session_id: tree.review,
            model: ModelId::new("haiku"),
        },
    );

    let mut expected = before;
    expected[5] = "│     Review · haiku".to_owned();
    assert_eq!(
        aside_rows(&application, WIDTH),
        expected,
        "the confirmed Model joins the entry's second line where it stands, and nothing \
         else moves"
    );
}

/// A brokered Subagent — its row in the delegating Transcript the Broker's —
/// reaches the Section as the protocol reports one: spawned with no Model
/// known, told the Model its own Provider confirmed once that Provider takes
/// its first Turn, and settled. At each step its entry reads as a native
/// Subagent's does.
#[test]
fn a_brokered_subagents_entry_shows_the_model_its_provider_confirmed_and_its_outcome() {
    let workspace = workspace_dir();
    let mut application = client(workspace.path());
    let (mut parent, child) = parent_with_working_subagent(workspace.path());
    let Activity::Subagent {
        name,
        description,
        brokered,
        ..
    } = &mut parent.activities[0]
    else {
        unreachable!("the fixture's row is a Subagent's")
    };
    *name = "Scout".to_owned();
    *description = "Survey the Claude seam".to_owned();
    *brokered = true;
    let top = parent.session.id;
    application
        .handle_event(ApplicationEvent::SessionAttached(parent))
        .expect("open the delegating Session");
    deliver_tree(
        &mut application,
        top,
        SubagentTreeEvent::Snapshot(SubagentTreeSnapshot {
            revision: SubagentTreeRevision::INITIAL,
            top_level: SubagentTreeTopLevel {
                own_working_since: None,
                status: None,
                worked_ms: None,
                session_id: top,
                title: "Delegate the mapping".to_owned(),
                working_since: None,
                monitoring_since: None,
                needs_intervention: false,
                sidekick: false,
            },
            subagents: Vec::new(),
            sessions: Vec::new(),
        }),
    );

    change(
        &mut application,
        top,
        SubagentTreeChange::SubagentSpawned {
            entry: entry(
                child,
                top,
                0,
                ("Scout", "Survey the Claude seam"),
                ActivityStatus::Active,
                None,
            ),
        },
    );
    assert_eq!(
        entry_lines(&application, "Scout"),
        ["└ ⠋ Survey the Claude seam", "    Scout"],
        "until its Provider confirms a Model, the entry says its name alone"
    );

    change(
        &mut application,
        top,
        SubagentTreeChange::SubagentModelChanged {
            session_id: child,
            model: ModelId::new("gpt-5"),
        },
    );
    assert_eq!(
        entry_lines(&application, "Scout"),
        ["└ ⠋ Survey the Claude seam", "    Scout · gpt-5"],
        "the Model its own Provider confirmed follows its name"
    );

    change(
        &mut application,
        top,
        SubagentTreeChange::SubagentWorkingChanged {
            session_id: child,
            status: ActivityStatus::Failed,
            worked_ms: Some(3_000),
            working_since: None,
            monitoring_since: None,
        },
    );
    assert_eq!(
        entry_lines(&application, "Scout"),
        [
            "└ × Survey the Claude seam",
            "    Scout · gpt-5 · Failed 3s"
        ],
        "once settled it wears its outcome, named after the Model, and its final time"
    );
}

#[test]
fn at_the_launch_width_the_name_is_left_out_before_the_model_is_cut() {
    let workspace = workspace_dir();
    let tree = Tree::new();
    let detail_line = |name: &str, model: Option<&str>| {
        let mut application = client(workspace.path());
        open(&mut application, workspace.path(), tree.top, None);
        let mut snapshot = tree.snapshot();
        snapshot.subagents.truncate(1);
        snapshot.subagents[0].name = name.to_owned();
        snapshot.subagents[0].model = model.map(ModelId::new);
        deliver_tree(
            &mut application,
            tree.top,
            SubagentTreeEvent::Snapshot(snapshot),
        );
        aside_rows(&application, WIDTH)[3].clone()
    };

    assert_eq!(
        detail_line("Cartographer", Some("sonnet")),
        "    Cartographer · sonnet 12s",
        "a name and Model that just fit beside the time are both drawn whole"
    );
    assert_eq!(
        detail_line("Cartographer", Some("claude-sonnet-4-5")),
        "    claude-sonnet-4-5     12s",
        "where both do not fit, the name is left out and the Model stands whole"
    );
    assert_eq!(
        detail_line(
            "Cartographer",
            Some("claude-sonnet-4-5-with-an-astronomically-long-suffix")
        ),
        "    claude-sonnet-4-5-wi… 12s",
        "a Model too long for the line alone gives way to the time"
    );
    assert_eq!(
        detail_line("Cartographer-of-every-seam", None),
        "    Cartographer-of-ever… 12s",
        "with no Model known, a name too long for the line gives way to the time as ever"
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
fn the_last_settle_in_a_branch_moves_it_behind_newer_settled_siblings() {
    let workspace = workspace_dir();
    let (mut application, tree) = tree_open_at_top(workspace.path());
    deliver_tree(
        &mut application,
        tree.top,
        SubagentTreeEvent::Changed(SubagentTreeChange::SubagentWorkingChanged {
            session_id: tree.review,
            status: ActivityStatus::Failed,
            worked_ms: Some(3_000),
            working_since: None,
            monitoring_since: None,
        }),
    );

    let rows = aside_rows(&application, WIDTH);
    assert_eq!(
        rows[2..8],
        [
            "├ ✓ Weigh the options",
            "│   Plan",
            "└ ✓ Map the provider seams",
            "  │ Explore               12s",
            "  └ × Check the mapped seams",
            "      Review · Failed      3s",
        ],
        "with nothing in it working, the branch falls in behind its newer settled \
         sibling, and the settled entry wears its outcome — named beside its name, \
         since a stop wears the same glyph — and its final time: {rows:#?}"
    );
}

#[test]
fn a_late_spawn_stands_beneath_its_spawner_ahead_of_older_siblings() {
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
    assert_eq!(rows[0], "Subagents 4 (2 active)");
    assert_eq!(
        rows[4..10],
        [
            "│ ├ ⠋ Probe a seam",
            "│ │   Probe",
            "│ └ ⠋ Check the mapped seams",
            "│     Review",
            "└ ✓ Weigh the options",
            "    Plan",
        ],
        "working alike, the newer spawn stands ahead of its older sibling: {rows:#?}"
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
        brokered: false,
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
        ["Subagents 3 (1 active)", "Map every seam"],
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

/// The two lines of the Subagent entry named `name`: its Marker and Title
/// line, and the name-and-time line beneath it.
fn entry_lines(application: &Application, name: &str) -> [String; 2] {
    let rows = aside_rows(application, WIDTH);
    let detail = rows
        .iter()
        .position(|row| row.contains(name))
        .unwrap_or_else(|| panic!("the Aside draws {name:?}: {rows:#?}"));
    [rows[detail - 1].clone(), rows[detail].clone()]
}

/// The colour "Needs Intervention" is drawn in on the Aside row at `row`.
fn needs_intervention_colour(application: &Application, row: usize) -> Option<Color> {
    aside_colour(application, row, "Needs Intervention")
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
    snapshot.subagents[1].working_since = Some(before_start(5_000));
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
        SubagentTreeChange::SubagentWorkingChanged {
            session_id: tree.review,
            status: ActivityStatus::Completed,
            worked_ms: Some(95_000),
            working_since: None,
            monitoring_since: None,
        },
    );
    let settled = entry_lines(&application, "Review");
    assert!(
        settled[1].ends_with(" 1m 35s") && settled[0].contains('✓'),
        "a settle stands the entry at the duration its work took: {settled:?}"
    );
    advance_session_clock(&now, 60_000);
    assert_eq!(
        entry_lines(&application, "Review"),
        settled,
        "and the time no longer moves"
    );
    assert!(
        !application.wants_spinner(),
        "nothing in the Aside is live any more"
    );
}

/// A settled Subagent a resume — or a Continuation its own work began —
/// sets Working again counts up from what its earlier Turns worked, and once
/// that Turn settles stands at the time all of them took, the change arriving
/// live with the Section open.
#[test]
fn a_settled_entry_working_again_counts_up_from_its_earlier_turns_and_stands_at_their_sum() {
    let workspace = workspace_dir();
    let (mut application, now) = session_clocked_client(workspace.path());
    let tree = Tree::new();
    open(&mut application, workspace.path(), tree.top, None);
    let mut snapshot = tree.snapshot();
    // Nothing else in the tree is live, so what ticks is the resumed entry.
    snapshot.subagents[1].status = ActivityStatus::Completed;
    snapshot.subagents[1].worked_ms = Some(3_000);
    deliver_tree(
        &mut application,
        tree.top,
        SubagentTreeEvent::Snapshot(snapshot),
    );
    assert_eq!(
        entry_lines(&application, "Explore"),
        [
            "└ ✓ Map the provider seams",
            "  │ Explore               12s"
        ],
        "settled, the older branch stands after its newer sibling"
    );
    assert!(!application.wants_spinner());

    change(
        &mut application,
        tree.top,
        SubagentTreeChange::SubagentWorkingChanged {
            session_id: tree.explore,
            status: ActivityStatus::Active,
            worked_ms: Some(12_000),
            // Begun at the moment the clock now reads.
            working_since: Some(SessionTimestamp(CLOCK_START)),
            monitoring_since: None,
        },
    );
    let resumed = entry_lines(&application, "Explore");
    assert!(
        resumed[0].starts_with("├ ⠋ ") && resumed[1].ends_with(" 12s"),
        "working again, the entry moves back ahead of its settled sibling, wears the \
         Working Marker, and counts up from its earlier Turns' time: {resumed:?}"
    );
    assert!(
        application.wants_spinner(),
        "and keeps the run loop ticking while it works"
    );
    advance_session_clock(&now, 5_000);
    assert!(aside_row(&application, "Explore").ends_with(" 17s"));
    advance_session_clock(&now, 60_000);
    assert!(
        aside_row(&application, "Explore").ends_with(" 1m"),
        "read the way a working entry always reads: {:?}",
        aside_row(&application, "Explore")
    );

    change(
        &mut application,
        tree.top,
        SubagentTreeChange::SubagentWorkingChanged {
            session_id: tree.explore,
            status: ActivityStatus::Failed,
            worked_ms: Some(77_000),
            working_since: None,
            monitoring_since: None,
        },
    );
    let settled = entry_lines(&application, "Explore");
    assert_eq!(
        settled,
        [
            "└ × Map the provider seams",
            "  │ Explore · Failed   1m 17s"
        ],
        "settled again, it falls back behind its newer sibling and wears the latest \
         Turn's outcome over all its Turns' time"
    );
    advance_session_clock(&now, 60_000);
    assert_eq!(
        entry_lines(&application, "Explore"),
        settled,
        "and the time no longer moves"
    );
    assert!(!application.wants_spinner());
}

#[test]
fn a_settled_entry_whose_end_went_unlearned_leaves_its_time_blank() {
    let workspace = workspace_dir();
    let (mut application, tree) = tree_open_at_top(workspace.path());
    change(
        &mut application,
        tree.top,
        SubagentTreeChange::SubagentWorkingChanged {
            session_id: tree.review,
            status: ActivityStatus::Failed,
            worked_ms: None,
            working_since: None,
            monitoring_since: None,
        },
    );

    assert_eq!(
        entry_lines(&application, "Review"),
        ["  └ × Check the mapped seams", "      Review · Failed"],
        "no time stands beside it, the Marker saying enough"
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
            own_working_since: None,
            status: None,
            worked_ms: None,
            working_since: None,
            monitoring_since: None,
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
            own_working_since: None,
            status: None,
            worked_ms: None,
            // Begun at the moment the clock now reads.
            working_since: Some(SessionTimestamp(CLOCK_START + 3_000)),
            monitoring_since: None,
        },
    );
    advance_session_clock(&now, 2_000);
    let resumed = aside_rows(&application, WIDTH)[1].clone();
    assert!(
        resumed.starts_with("⠋ Map every seam") && resumed.ends_with(" 2s"),
        "and come back, counting afresh, when it Works again: {resumed:?}"
    );
}

/// A settled Subagent whose Watches outlive it keeps the Marker it settled
/// with and says **monitoring** where its time would stand, while the
/// top-level Session Monitoring through it wears the Working Marker and the
/// time since Monitoring began — both clearing once the Watches settle.
#[test]
fn a_monitoring_subagent_says_so_in_its_times_place_and_the_top_level_counts_its_monitoring() {
    let workspace = workspace_dir();
    let (mut application, now) = session_clocked_client(workspace.path());
    let tree = Tree::new();
    open(&mut application, workspace.path(), tree.top, None);
    let mut snapshot = tree.snapshot();
    // Nothing in the tree is Working: the settled Explore left a Watch running.
    snapshot.subagents[1].status = ActivityStatus::Completed;
    snapshot.subagents[1].worked_ms = Some(3_000);
    snapshot.subagents[0].monitoring_since = Some(before_start(20_000));
    snapshot.top_level.monitoring_since = Some(before_start(20_000));
    deliver_tree(
        &mut application,
        tree.top,
        SubagentTreeEvent::Snapshot(snapshot),
    );

    let explore = entry_lines(&application, "Explore");
    assert!(
        explore[0].starts_with("└ ✓ ") && explore[1].ends_with(" monitoring"),
        "the settled Marker stays, and monitoring stands in its time's place: {explore:?}"
    );
    assert!(
        !aside_row(&application, "Plan").contains("monitoring"),
        "only the Subagent whose Session is Monitoring says so"
    );
    let top_level = aside_rows(&application, WIDTH)[1].clone();
    assert!(
        top_level.starts_with("⠋ Map every seam") && top_level.ends_with(" 20s"),
        "the top-level entry shows Monitoring the way it shows Working: {top_level:?}"
    );
    advance_session_clock(&now, 5_000);
    assert!(aside_rows(&application, WIDTH)[1].ends_with(" 25s"));

    change(
        &mut application,
        tree.top,
        SubagentTreeChange::SubagentWorkingChanged {
            session_id: tree.explore,
            status: ActivityStatus::Completed,
            worked_ms: Some(12_000),
            working_since: None,
            monitoring_since: None,
        },
    );
    change(
        &mut application,
        tree.top,
        SubagentTreeChange::TopLevelWorkingChanged {
            own_working_since: None,
            status: None,
            worked_ms: None,
            working_since: None,
            monitoring_since: None,
        },
    );
    assert!(
        aside_row(&application, "Explore").ends_with(" 12s"),
        "once its Watches settle the entry stands at its time again"
    );
    assert_eq!(aside_rows(&application, WIDTH)[1], "Map every seam");
    assert!(!application.wants_spinner());
}

#[test]
fn needs_intervention_stands_in_the_warning_colour_on_the_owning_entry_only() {
    let workspace = workspace_dir();
    let (mut application, _now) = session_clocked_client(workspace.path());
    let tree = Tree::new();
    open(&mut application, workspace.path(), tree.top, None);
    let mut snapshot = tree.snapshot();
    snapshot.top_level.working_since = Some(before_start(30_000));
    snapshot.subagents[1].working_since = Some(before_start(5_000));
    snapshot.subagents[1].needs_intervention = true;
    deliver_tree(
        &mut application,
        tree.top,
        SubagentTreeEvent::Snapshot(snapshot),
    );

    let rows = aside_rows(&application, WIDTH);
    assert!(
        rows[4].starts_with("│ └ ⠋ ") && rows[5] == "│     Rev… Needs Intervention",
        "the nested Subagent whose own Session waits says so in its time's place, its \
         name giving way: {rows:#?}"
    );
    assert_eq!(
        needs_intervention_colour(&application, 5),
        Some(Color::Yellow)
    );
    assert!(
        rows[3].ends_with(" 12s") && rows[1].ends_with(" 30s"),
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
        answered[5].contains("Review") && answered[5].ends_with(" 5s"),
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

// Row focus and scrolling: the Aside driven by the keys, and a tree taller
// than its column.

/// The Aside rows painted with row focus: the focus block's background at the
/// Aside's first content column.
fn focused_aside_rows(application: &Application) -> Vec<String> {
    let buffer = rendered_application_buffer(application, WIDTH, HEIGHT);
    let rows = aside_rows(application, WIDTH);
    (0..HEIGHT)
        .filter(|row| buffer[(WIDTH - ASIDE_WIDTH + 2, *row)].bg == Color::Blue)
        .map(|row| rows[usize::from(row)].trim_end().to_owned())
        .collect()
}

fn press(
    application: &mut Application,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(code, modifiers)))
        .expect("press a key")
}

/// The fixture tree open at `open`, with the Aside holding the keys.
fn driving_the_aside(
    open_at: impl FnOnce(&Tree) -> (SessionId, Option<SessionId>),
) -> (Application, Tree, crate::support::WorkspaceDir) {
    let workspace = workspace_dir();
    let (mut application, tree) = tree_open_at_top(workspace.path());
    let (session, parent) = open_at(&tree);
    if session != tree.top {
        open(&mut application, workspace.path(), session, parent);
    }
    invoke(&mut application, SemanticCommandId::AsideToggle);
    (application, tree, workspace)
}

#[test]
fn row_focus_begins_on_the_open_entry_and_walks_with_the_arrows_and_ctrl_p_n() {
    let (mut application, _, _workspace) =
        driving_the_aside(|tree| (tree.review, Some(tree.explore)));
    assert_eq!(
        focused_aside_rows(&application),
        ["│ └ ⠋ Check the mapped seams", "│     Review"],
        "focus begins on the open Session's entry, and paints both its lines"
    );

    press(&mut application, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(
        focused_aside_rows(&application),
        ["└ ✓ Weigh the options", "    Plan"],
        "a step walks a whole entry"
    );
    press(&mut application, KeyCode::Char('n'), KeyModifiers::CONTROL);
    assert_eq!(
        focused_aside_rows(&application),
        ["Map every seam"],
        "Ctrl+N walks on, wrapping past the end"
    );
    press(&mut application, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(
        focused_aside_rows(&application),
        ["└ ✓ Weigh the options", "    Plan"],
        "Up wraps back past the top"
    );
    press(&mut application, KeyCode::Char('p'), KeyModifiers::CONTROL);
    assert_eq!(
        focused_aside_rows(&application),
        ["│ └ ⠋ Check the mapped seams", "│     Review"]
    );
}

#[test]
fn enter_opens_the_focused_entry_and_does_nothing_on_the_open_one() {
    let (mut application, tree, _workspace) = driving_the_aside(|tree| (tree.top, None));
    assert_eq!(
        press(&mut application, KeyCode::Enter, KeyModifiers::NONE),
        ApplicationTransition::Continue,
        "the open Session's own entry opens nothing"
    );
    press(&mut application, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(
        press(&mut application, KeyCode::Enter, KeyModifiers::NONE),
        ApplicationTransition::AttachSession(local(tree.explore)),
        "Enter opens the focused entry through the ordinary attach route"
    );
}

#[test]
fn esc_hands_the_keys_back_and_focus_begins_again_on_the_open_entry() {
    let (mut application, _, _workspace) = driving_the_aside(|tree| (tree.top, None));
    press(&mut application, KeyCode::Down, KeyModifiers::NONE);
    assert_eq!(
        focused_aside_rows(&application),
        [
            "├ ✓ Map the provider seams",
            "│ │ Explore               12s"
        ]
    );

    press(&mut application, KeyCode::Esc, KeyModifiers::NONE);
    assert!(
        focused_aside_rows(&application).is_empty(),
        "focus goes with the keys"
    );
    type_terminal_text(&mut application, "zq");
    assert!(
        rendered_application_rows_at(&application, WIDTH, HEIGHT)
            .join("\n")
            .contains("zq"),
        "and the composer has them back"
    );

    invoke(&mut application, SemanticCommandId::AsideToggle);
    assert_eq!(
        focused_aside_rows(&application),
        ["Map every seam"],
        "taking the keys again begins on the open entry, not where focus was left"
    );
}

#[test]
fn focus_follows_its_entry_through_spawns_settles_and_a_fresh_tree() {
    let (mut application, tree, _workspace) = driving_the_aside(|tree| (tree.top, None));
    press(&mut application, KeyCode::Up, KeyModifiers::NONE);
    assert_eq!(
        focused_aside_rows(&application),
        ["└ ✓ Weigh the options", "    Plan"]
    );

    let before = SessionId::new();
    deliver_tree(
        &mut application,
        tree.top,
        SubagentTreeEvent::Changed(SubagentTreeChange::SubagentSpawned {
            entry: entry(
                before,
                tree.explore,
                1,
                ("Probe", "Probe a seam"),
                ActivityStatus::Active,
                None,
            ),
        }),
    );
    assert_eq!(
        focused_aside_rows(&application),
        ["└ ✓ Weigh the options", "    Plan"],
        "a spawn drawn above the focused entry does not move focus off it"
    );
    deliver_tree(
        &mut application,
        tree.top,
        SubagentTreeEvent::Changed(SubagentTreeChange::SubagentSpawned {
            entry: entry(
                SessionId::new(),
                tree.plan,
                0,
                ("Check", "Check the plan"),
                ActivityStatus::Active,
                None,
            ),
        }),
    );
    deliver_tree(
        &mut application,
        tree.top,
        SubagentTreeEvent::Changed(SubagentTreeChange::SubagentWorkingChanged {
            session_id: tree.plan,
            status: ActivityStatus::Failed,
            worked_ms: Some(1_000),
            working_since: None,
            monitoring_since: None,
        }),
    );
    assert_eq!(
        focused_aside_rows(&application),
        ["├ × Weigh the options", "│ │ Plan · Failed          1s"],
        "nor does a spawn below it, which moves its now working branch ahead of the \
         older one, or its own settle, whose spawn now hangs from the rule its second \
         line carries"
    );

    // A fresh tree without the focused entry hands focus to the nearest
    // entry left.
    let mut fresh = tree.snapshot();
    fresh
        .subagents
        .retain(|entry| entry.session_id != tree.plan);
    deliver_tree(
        &mut application,
        tree.top,
        SubagentTreeEvent::Snapshot(fresh),
    );
    assert_eq!(
        focused_aside_rows(&application),
        [
            "└ ✓ Map the provider seams",
            "  │ Explore               12s"
        ],
        "the entry now standing where Plan stood"
    );
}

#[test]
fn focus_is_painted_only_while_the_aside_holds_the_keys() {
    let workspace = workspace_dir();
    let (mut application, _) = tree_open_at_top(workspace.path());
    assert!(
        focused_aside_rows(&application).is_empty(),
        "an Aside without the keys paints no focus"
    );

    invoke(&mut application, SemanticCommandId::AsideToggle);
    assert_eq!(focused_aside_rows(&application), ["Map every seam"]);

    invoke(&mut application, SemanticCommandId::SidebarToggle);
    assert!(
        focused_aside_rows(&application).is_empty(),
        "the Sidebar taking the keys takes them, and the paint, from the Aside"
    );
    invoke(&mut application, SemanticCommandId::AsideToggle);
    assert_eq!(
        focused_aside_rows(&application),
        ["Map every seam"],
        "and the Aside takes them back"
    );

    invoke(&mut application, SemanticCommandId::SettingsOpen);
    assert!(
        focused_aside_rows(&application).is_empty(),
        "an overlay outranks the Aside, so its focus is not painted beneath it"
    );
}

#[test]
fn clicking_an_entry_opens_it_without_raising_row_focus() {
    let workspace = workspace_dir();
    let (mut application, tree) = tree_open_at_top(workspace.path());
    let plan = aside_row_position(&application, "Plan");
    assert_eq!(
        click(&mut application, plan),
        ApplicationTransition::AttachSession(local(tree.plan))
    );
    assert!(focused_aside_rows(&application).is_empty());
    type_terminal_text(&mut application, "zq");
    assert!(
        rendered_application_rows_at(&application, WIDTH, HEIGHT)
            .join("\n")
            .contains("zq"),
        "the keys stay with the composer"
    );
}

/// A top-level Session with forty settled Subagents of its own, far more
/// than the column holds. The newest spawn is drawn first, so the Tasks are
/// numbered, and `children` listed, in the order they are drawn.
fn tall_tree(top: SessionId) -> (SubagentTreeSnapshot, Vec<SessionId>) {
    let children = (0..40).map(|_| SessionId::new()).collect::<Vec<_>>();
    let snapshot = SubagentTreeSnapshot {
        revision: SubagentTreeRevision::INITIAL,
        top_level: SubagentTreeTopLevel {
            own_working_since: None,
            status: None,
            worked_ms: None,
            session_id: top,
            title: "Map every seam".to_owned(),
            working_since: None,
            monitoring_since: None,
            needs_intervention: false,
            sidekick: false,
        },
        subagents: children
            .iter()
            .enumerate()
            .map(|(order, child)| {
                entry(
                    *child,
                    top,
                    u32::try_from(39 - order).expect("fits"),
                    ("Agent", &format!("Task {order:02}")),
                    ActivityStatus::Completed,
                    None,
                )
            })
            .collect(),
        sessions: Vec::new(),
    };
    (snapshot, children)
}

/// The lines the Aside's window shows, below its header.
fn window(application: &Application) -> Vec<String> {
    aside_rows(application, WIDTH)[1..]
        .iter()
        .filter(|row| !row.is_empty())
        .cloned()
        .collect()
}

/// The first and last lines the Aside's window shows, below its header.
fn window_edges(application: &Application) -> (String, String) {
    let body = window(application);
    (
        body.first().expect("a first row").clone(),
        body.last().expect("a last row").clone(),
    )
}

#[test]
fn a_tall_tree_scrolls_to_keep_two_entries_beyond_the_open_then_the_focused_entry() {
    let workspace = workspace_dir();
    let top = SessionId::new();
    let mut application = client(workspace.path());
    let (snapshot, children) = tall_tree(top);
    open(&mut application, workspace.path(), children[30], Some(top));
    // The subscription is asked through the Session the reader opened.
    deliver_tree(
        &mut application,
        children[30],
        SubagentTreeEvent::Snapshot(snapshot),
    );
    assert!(
        aside_text(&application).contains("Task 30"),
        "{:#?}",
        aside_rows(&application, WIDTH)
    );
    assert_eq!(
        window_edges(&application).1,
        "│   Agent",
        "the window scrolls whole entries, and never cuts a row: {:#?}",
        window(&application)
    );
    assert!(
        window(&application).contains(&"├ ✓ Task 32".to_owned())
            && !window(&application).contains(&"├ ✓ Task 33".to_owned()),
        "just far enough to show the open entry and the two beneath it: {:#?}",
        window(&application)
    );

    invoke(&mut application, SemanticCommandId::AsideToggle);
    for _ in 0..5 {
        press(&mut application, KeyCode::Down, KeyModifiers::NONE);
    }
    assert_eq!(
        focused_aside_rows(&application),
        ["├ ✓ Task 35", "│   Agent"]
    );
    assert!(
        window(&application).contains(&"├ ✓ Task 37".to_owned())
            && !window(&application).contains(&"├ ✓ Task 38".to_owned()),
        "the window follows row focus, two entries ahead of it: {:#?}",
        window(&application)
    );
    let walked = window(&application);
    for _ in 0..2 {
        press(&mut application, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(
            window(&application),
            walked,
            "walking back up leaves the window standing while two entries show above focus"
        );
    }
    for _ in 0..34 {
        press(&mut application, KeyCode::Up, KeyModifiers::NONE);
    }
    assert_eq!(focused_aside_rows(&application), ["Map every seam"]);
    assert_eq!(window_edges(&application).0, "Map every seam");
}

fn wheel(application: &mut Application, kind: MouseEventKind) {
    application
        .handle_terminal_event(InputEvent::Mouse(MouseEvent {
            kind,
            column: WIDTH - ASIDE_WIDTH + 6,
            row: 5,
            modifiers: KeyModifiers::NONE,
        }))
        .expect("wheel over the Aside");
}

#[test]
fn the_wheel_scrolls_the_aside_without_taking_the_keys() {
    let workspace = workspace_dir();
    let top = SessionId::new();
    let mut application = client(workspace.path());
    open(&mut application, workspace.path(), top, None);
    let (snapshot, _) = tall_tree(top);
    deliver_tree(&mut application, top, SubagentTreeEvent::Snapshot(snapshot));
    assert_eq!(window_edges(&application).0, "Map every seam");

    wheel(&mut application, MouseEventKind::ScrollDown);
    assert_eq!(
        window_edges(&application).0,
        "├ ✓ Task 02",
        "a tick moves the window three entries, however many lines each takes"
    );
    assert_eq!(
        window_edges(&application).0,
        "├ ✓ Task 02",
        "and it holds there, though the open entry is out of view"
    );
    for _ in 0..20 {
        wheel(&mut application, MouseEventKind::ScrollDown);
    }
    let end = window(&application);
    assert_eq!(
        end[end.len() - 2..],
        ["└ ✓ Task 39", "    Agent"],
        "the wheel stops at the end of the tree, the last entry whole: {end:#?}"
    );
    let heads = |line: &str| -> usize {
        let task = line.trim_start_matches("├ ✓ Task ");
        task.parse()
            .unwrap_or_else(|_| panic!("a Task heads the window: {line:?}"))
    };
    let at_end = heads(&window_edges(&application).0);
    wheel(&mut application, MouseEventKind::ScrollUp);
    assert_eq!(
        heads(&window_edges(&application).0),
        at_end - 3,
        "a tick back moves the window three entries up"
    );
    assert!(
        focused_aside_rows(&application).is_empty(),
        "wheeling is looking, not taking the keys"
    );

    invoke(&mut application, SemanticCommandId::AsideToggle);
    assert_eq!(
        window_edges(&application).0,
        "Map every seam",
        "taking the keys brings the focused entry back into view"
    );
}
