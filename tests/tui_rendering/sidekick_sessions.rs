//! The Sessions Section: where a Sidekick's Session heads the tree, the
//! Aside's Section answers for everything that Sidekick has a hand in. It is
//! headed **Sessions**, and beneath the Sidekick's own Subagents it lists its
//! Subsessions and the other Sessions it acted on alike, working ones first
//! and then the one it acted on most recently, each in three lines — Marker
//! and Title; its Workspace; the Model and the time — pressed, focused and
//! scrolled as one, with its own Subagents beneath it. A Subsession's tree is
//! its Sidekick's; a Session only acted on heads its own. The Subagent Picker
//! offers the working ones beside the Sidekick's Subagents, and stops one by
//! interrupting it.

use crate::support::{
    click_mouse, connected_application, deliver_settings, failed_session_snapshot, invoke,
    rendered_application_buffer, rendered_application_rows_at, workspace_dir,
};
use crossterm::event::{
    Event as InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::style::Color;
use suru::{
    managed_client::SubagentTreeEvent,
    protocol::{
        ActivityStatus, EffectiveSettings, ModelId, Outlook, PromptId, SessionId, SessionReference,
        SessionTimestamp, SidebarVisibility, SubagentTreeChange, SubagentTreeEntry,
        SubagentTreeRevision, SubagentTreeSession, SubagentTreeSnapshot, SubagentTreeTopLevel,
    },
    tui::{Application, ApplicationEvent, ApplicationTransition, SemanticCommandId},
};

/// Wide enough for the main view and the Aside at its launch width.
const WIDTH: u16 = 120;
const HEIGHT: u16 = 30;
/// The launch width of the Aside, rule included.
const ASIDE_WIDTH: u16 = 32;
/// The glyph a Workspace with no Icon is named beside.
const FOLDER: char = '\u{ea83}';
/// The glyph of the Icon Catalog's `md-bug`.
const BUG: char = '\u{f00e4}';

/// The fixture: a Sidekick's Session with a Subagent of its own, a
/// Subsession it began and settled, a Session it acted on that works with a
/// Subagent of its own, and another it acted on that was stopped.
struct Sidekick {
    top: SessionId,
    survey: SessionId,
    subsession: SessionId,
    build: SessionId,
    probe: SessionId,
    docs: SessionId,
    root: std::path::PathBuf,
}

impl Sidekick {
    fn new(workspace: &std::path::Path) -> Self {
        Self {
            top: SessionId::new(),
            survey: SessionId::new(),
            subsession: SessionId::new(),
            build: SessionId::new(),
            probe: SessionId::new(),
            docs: SessionId::new(),
            root: workspace.to_owned(),
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
                title: "Plan the week".to_owned(),
                working_since: None,
                monitoring_since: None,
                needs_intervention: false,
                sidekick: true,
            },
            subagents: vec![
                subagent(
                    self.survey,
                    self.top,
                    ("Explore", "Survey the Sessions"),
                    ActivityStatus::Completed,
                    Some(12_000),
                ),
                subagent(
                    self.probe,
                    self.build,
                    ("Probe", "Probe the build"),
                    ActivityStatus::Active,
                    None,
                ),
            ],
            sessions: vec![
                self.session(
                    self.subsession,
                    "Fix the flaky login test",
                    "auth",
                    ActivityStatus::Completed,
                    3_000,
                ),
                self.session(
                    self.docs,
                    "Tidy the docs",
                    "docs",
                    ActivityStatus::Interrupted,
                    2_000,
                ),
                self.session(
                    self.build,
                    "Untangle the build",
                    "build",
                    ActivityStatus::Active,
                    1_000,
                ),
            ],
        }
    }

    /// An entry for a Session beneath the Sidekick, in the Workspace named
    /// `workspace` beneath the fixture's root, the Subsession alone begun by
    /// it.
    fn session(
        &self,
        session_id: SessionId,
        title: &str,
        workspace: &str,
        status: ActivityStatus,
        acted_at: u64,
    ) -> SubagentTreeSession {
        let working = status == ActivityStatus::Active;
        SubagentTreeSession {
            unconfirmed: false,
            session_id,
            origin: None,
            unanswered: false,
            title: title.to_owned(),
            subsession: session_id == self.subsession,
            workspace_path: self.root.join(workspace),
            workspace_icon: None,
            model: Some(ModelId::new(if workspace == "auth" {
                "sonnet"
            } else {
                "gpt-5.5"
            })),
            status: Some(status),
            worked_ms: (!working).then_some(if workspace == "auth" { 45_000 } else { 3_000 }),
            // Unknown, so a working entry's time is left blank.
            working_since: None,
            monitoring_since: None,
            needs_intervention: false,
            acted_at: SessionTimestamp(acted_at),
        }
    }
}

fn subagent(
    session_id: SessionId,
    parent_session_id: SessionId,
    (name, title): (&str, &str),
    status: ActivityStatus,
    worked_ms: Option<u64>,
) -> SubagentTreeEntry {
    SubagentTreeEntry {
        origin: None,
        session_id,
        parent_session_id,
        spawn_order: 0,
        name: name.to_owned(),
        title: title.to_owned(),
        model: None,
        status,
        worked_ms,
        working_since: None,
        monitoring_since: None,
        needs_intervention: false,
    }
}

fn local(session_id: SessionId) -> SessionReference {
    SessionReference::new(Outlook::Local, session_id)
}

/// A client whose first Settings snapshot has shown the Aside, with the
/// Sidebar kept off the frame, and Icons drawn or not as `show_icons` says.
fn client(workspace: &std::path::Path, show_icons: bool) -> Application {
    let mut application = connected_application(workspace);
    let mut settings = EffectiveSettings::default();
    settings.sidebar.initial_visibility = SidebarVisibility::Hidden;
    settings.appearance.show_icons = show_icons;
    deliver_settings(&mut application, settings);
    application
}

/// Opens the top-level Session `session_id` as the ordinary attach route
/// lands it.
fn open(application: &mut Application, workspace: &std::path::Path, session_id: SessionId) {
    let snapshot = failed_session_snapshot(session_id, PromptId::new(), "Open work", workspace);
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("open a Session");
}

fn deliver(application: &mut Application, through: SessionId, event: SubagentTreeEvent) {
    application
        .handle_event(ApplicationEvent::SubagentTree {
            through: local(through),
            event,
        })
        .expect("take a Subagent tree event");
}

fn change(application: &mut Application, through: SessionId, change: SubagentTreeChange) {
    deliver(application, through, SubagentTreeEvent::Changed(change));
}

/// A client with the Sidekick's Session open and its tree in hand.
fn sidekick_open(show_icons: bool) -> (Application, Sidekick, crate::support::WorkspaceDir) {
    let workspace = workspace_dir();
    let sidekick = Sidekick::new(workspace.path());
    let mut application = client(workspace.path(), show_icons);
    open(&mut application, workspace.path(), sidekick.top);
    deliver(
        &mut application,
        sidekick.top,
        SubagentTreeEvent::Snapshot(sidekick.snapshot()),
    );
    (application, sidekick, workspace)
}

/// The Aside's own content columns of each rendered row, at an Aside of
/// `aside_width` columns.
fn aside_rows_at(application: &Application, aside_width: u16) -> Vec<String> {
    rendered_application_rows_at(application, WIDTH, HEIGHT)
        .iter()
        .map(|row| {
            row.chars()
                .skip(usize::from(WIDTH - aside_width + 2))
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

fn aside_rows(application: &Application) -> Vec<String> {
    aside_rows_at(application, ASIDE_WIDTH)
}

/// The screen position of the Aside row whose text holds `needle`.
fn aside_row_position(application: &Application, needle: &str) -> (u16, u16) {
    let rows = aside_rows(application);
    let row = rows
        .iter()
        .position(|row| row.contains(needle))
        .unwrap_or_else(|| panic!("the Aside draws {needle:?}: {rows:#?}"));
    (
        WIDTH - ASIDE_WIDTH + 4,
        u16::try_from(row).expect("row fits"),
    )
}

/// Presses the Aside row whose text holds `needle`.
fn click_on(application: &mut Application, needle: &str) -> ApplicationTransition {
    let position = aside_row_position(application, needle);
    click(application, position)
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

fn press(application: &mut Application, code: KeyCode) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
        .expect("press a key")
}

/// The Aside rows painted with row focus.
fn focused_aside_rows(application: &Application) -> Vec<String> {
    let buffer = rendered_application_buffer(application, WIDTH, HEIGHT);
    let rows = aside_rows(application);
    (0..HEIGHT)
        .filter(|row| buffer[(WIDTH - ASIDE_WIDTH + 2, *row)].bg == Color::Blue)
        .map(|row| rows[usize::from(row)].clone())
        .collect()
}

#[test]
fn a_sidekicks_tree_is_headed_sessions_and_lists_them_working_first_then_by_its_latest_act() {
    let (application, _, _workspace) = sidekick_open(false);

    let rows = aside_rows(&application);
    assert_eq!(
        rows[..16],
        [
            "Sessions 5 (2 active)",
            "Plan the week",
            "├ ✓ Survey the Sessions",
            "│   Explore               12s",
            "├ ⠋ Untangle the build",
            "│ │ build",
            "│ │ gpt-5.5",
            "│ └ ⠋ Probe the build",
            "│     Probe",
            "├ ✓ Fix the flaky login test",
            "│   auth",
            "│   sonnet                45s",
            "└ × Tidy the docs",
            "    docs",
            "    gpt-5.5 · Stopped      3s",
            "",
        ],
        "headed Sessions and counted the usual way, the Sidekick's own Subagent \
         first, then the Session that works, then the rest by the Sidekick's latest \
         act — the Subsession it began drawn as the Sessions it only acted on are — \
         each in three lines with its own Subagents beneath it: {rows:#?}"
    );
}

#[test]
fn a_sidekick_that_has_a_hand_in_nothing_yet_is_headed_sessions_all_the_same() {
    let workspace = workspace_dir();
    let sidekick = Sidekick::new(workspace.path());
    let mut application = client(workspace.path(), false);
    open(&mut application, workspace.path(), sidekick.top);
    let mut snapshot = sidekick.snapshot();
    snapshot.subagents.clear();
    snapshot.sessions.clear();
    deliver(
        &mut application,
        sidekick.top,
        SubagentTreeEvent::Snapshot(snapshot),
    );

    assert_eq!(
        aside_rows(&application)[..3],
        ["Sessions 0", "Plan the week", ""]
    );
}

#[test]
fn each_workspace_is_named_beside_its_icon_where_icons_are_drawn() {
    let workspace = workspace_dir();
    let sidekick = Sidekick::new(workspace.path());
    let mut application = client(workspace.path(), true);
    open(&mut application, workspace.path(), sidekick.top);
    let mut snapshot = sidekick.snapshot();
    snapshot.sessions[0].workspace_icon = Some("md-bug".to_owned());
    deliver(
        &mut application,
        sidekick.top,
        SubagentTreeEvent::Snapshot(snapshot),
    );

    let rows = aside_rows(&application);
    assert!(
        rows.contains(&format!("│   {BUG} auth")),
        "a Workspace with an Icon is named beside it: {rows:#?}"
    );
    assert!(
        rows.contains(&format!("    {FOLDER} docs")),
        "and one without beside the plain folder: {rows:#?}"
    );
}

#[test]
fn an_act_moves_its_session_up_and_settled_work_moves_down_but_stays_drawn_as_settled() {
    let (mut application, sidekick, _workspace) = sidekick_open(false);

    let acted_again = sidekick.session(
        sidekick.docs,
        "Tidy the docs",
        "docs",
        ActivityStatus::Interrupted,
        4_000,
    );
    change(
        &mut application,
        sidekick.top,
        SubagentTreeChange::SessionChanged { entry: acted_again },
    );
    let rows = aside_rows(&application);
    assert!(
        rows.iter().position(|row| row.contains("Tidy the docs"))
            < rows
                .iter()
                .position(|row| row.contains("Fix the flaky login test")),
        "the Session acted on most recently stands ahead of the others not working: {rows:#?}"
    );

    // The working Session settles, and so does its Subagent.
    let mut settled = sidekick.session(
        sidekick.build,
        "Untangle the build",
        "build",
        ActivityStatus::Failed,
        1_000,
    );
    settled.worked_ms = Some(61_000);
    change(
        &mut application,
        sidekick.top,
        SubagentTreeChange::SessionChanged { entry: settled },
    );
    change(
        &mut application,
        sidekick.top,
        SubagentTreeChange::SubagentWorkingChanged {
            session_id: sidekick.probe,
            status: ActivityStatus::Completed,
            worked_ms: Some(2_000),
            working_since: None,
            monitoring_since: None,
        },
    );
    let rows = aside_rows(&application);
    assert_eq!(
        rows[..16],
        [
            "Sessions 5",
            "Plan the week",
            "├ ✓ Survey the Sessions",
            "│   Explore               12s",
            "├ × Tidy the docs",
            "│   docs",
            "│   gpt-5.5 · Stopped      3s",
            "├ ✓ Fix the flaky login test",
            "│   auth",
            "│   sonnet                45s",
            "└ × Untangle the build",
            "  │ build",
            "  │ gpt-5.5 · Failed    1m 1s",
            "  └ ✓ Probe the build",
            "      Probe                2s",
            "",
        ],
        "settled, it falls in by its latest act and stays, wearing how it settled: \
         {rows:#?}"
    );

    // A Session deleted leaves the tree with its Subagents; nothing else
    // takes an entry away.
    change(
        &mut application,
        sidekick.top,
        SubagentTreeChange::SessionLeft {
            session_id: sidekick.build,
            origin: None,
        },
    );
    let rows = aside_rows(&application);
    assert_eq!(rows[0], "Sessions 3");
    assert!(
        !rows
            .iter()
            .any(|row| row.contains("Untangle the build") || row.contains("Probe")),
        "{rows:#?}"
    );
}

/// The three-line entry of the fixture's stopped Session, with its Title,
/// Workspace and Model as given, alone beneath a Sidekick's Session in an
/// Aside at its launch width.
fn entry_lines(
    title: &str,
    workspace_name: &str,
    model: Option<&str>,
    show_icons: bool,
) -> Vec<String> {
    let workspace = workspace_dir();
    let sidekick = Sidekick::new(workspace.path());
    let mut application = client(workspace.path(), show_icons);
    open(&mut application, workspace.path(), sidekick.top);
    let mut snapshot = sidekick.snapshot();
    snapshot.subagents.clear();
    let mut entry = sidekick.session(
        sidekick.docs,
        title,
        workspace_name,
        ActivityStatus::Interrupted,
        2_000,
    );
    entry.model = model.map(ModelId::new);
    snapshot.sessions = vec![entry];
    deliver(
        &mut application,
        sidekick.top,
        SubagentTreeEvent::Snapshot(snapshot),
    );
    aside_rows(&application)[2..5].to_vec()
}

#[test]
fn where_a_line_runs_short_the_title_and_workspace_are_cut_and_the_model_gives_way_to_the_slot() {
    assert_eq!(
        entry_lines("Tidy the docs", "docs", Some("gpt-5.5"), false),
        [
            "└ × Tidy the docs",
            "    docs",
            "    gpt-5.5 · Stopped      3s"
        ],
        "everything fits whole"
    );
    assert_eq!(
        entry_lines(
            "Tidy every page of the developer documentation",
            "documentation-of-every-service",
            Some("gpt-5.5"),
            false,
        ),
        [
            "└ × Tidy every page of the d…",
            "    documentation-of-every-s…",
            "    gpt-5.5 · Stopped      3s",
        ],
        "a long Title and Workspace are each cut short with an ellipsis"
    );
    assert_eq!(
        entry_lines(
            "Tidy the docs",
            "documentation-of-every-service",
            Some("gpt-5.5"),
            true,
        )[1],
        format!("    {FOLDER} documentation-of-every…"),
        "where Icons are drawn the Workspace keeps its Icon and its name gives way"
    );
    assert_eq!(
        entry_lines(
            "Tidy the docs",
            "docs",
            Some("claude-sonnet-4-5-with-a-long-suffix"),
            false,
        )[2],
        "    claude-sonn… · Stopped 3s",
        "a Model too long for the line gives way, never the outcome or the time"
    );
    assert_eq!(
        entry_lines("Tidy the docs", "docs", None, false)[2],
        "    Stopped                3s",
        "with no Model, the outcome stands alone beside the time"
    );
}

#[test]
fn every_line_of_an_entry_opens_its_session_and_the_open_entry_opens_nothing() {
    let (mut application, sidekick, _workspace) = sidekick_open(false);

    for line in ["Fix the flaky login test", "│   auth", "sonnet"] {
        assert_eq!(
            click_on(&mut application, line),
            ApplicationTransition::ViewAndAttachSession(local(sidekick.subsession)),
            "pressing {line:?} opens the Session the whole entry stands for"
        );
    }
    assert_eq!(
        click_on(&mut application, "gpt-5.5 · Stopped"),
        ApplicationTransition::ViewAndAttachSession(local(sidekick.docs)),
        "a Session only acted on opens the same way"
    );
    assert_eq!(
        click_on(&mut application, "Probe the build"),
        ApplicationTransition::AttachSession(local(sidekick.probe)),
        "its Subagent still opens as a Subagent does"
    );
    assert_eq!(
        click_on(&mut application, "Plan the week"),
        ApplicationTransition::Continue,
        "the open Sidekick's own entry opens nothing"
    );
}

#[test]
fn row_focus_walks_an_entry_as_one_and_enter_opens_it() {
    let (mut application, sidekick, _workspace) = sidekick_open(false);
    invoke(&mut application, SemanticCommandId::AsideToggle);
    for _ in 0..2 {
        press(&mut application, KeyCode::Down);
    }
    assert_eq!(
        focused_aside_rows(&application),
        ["├ ⠋ Untangle the build", "│ │ build", "│ │ gpt-5.5"],
        "focus paints the whole entry"
    );
    press(&mut application, KeyCode::Down);
    press(&mut application, KeyCode::Down);
    assert_eq!(
        focused_aside_rows(&application),
        [
            "├ ✓ Fix the flaky login test",
            "│   auth",
            "│   sonnet                45s"
        ],
        "a step walks past a nested Subagent onto the next whole entry"
    );
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::ViewAndAttachSession(local(sidekick.subsession))
    );
}

#[test]
fn a_short_aside_scrolls_three_line_entries_whole() {
    let workspace = workspace_dir();
    let sidekick = Sidekick::new(workspace.path());
    let mut application = client(workspace.path(), false);
    open(&mut application, workspace.path(), sidekick.top);
    let mut snapshot = sidekick.snapshot();
    // Enough Sessions beneath the Sidekick to outgrow the column.
    snapshot.sessions = (0..12)
        .map(|index| {
            let mut entry = sidekick.session(
                SessionId::new(),
                &format!("Task {index:02}"),
                "docs",
                ActivityStatus::Completed,
                10_000 - index,
            );
            entry.worked_ms = None;
            entry
        })
        .collect();
    snapshot.subagents.clear();
    let last = snapshot.sessions[11].session_id;
    deliver(
        &mut application,
        sidekick.top,
        SubagentTreeEvent::Snapshot(snapshot),
    );
    invoke(&mut application, SemanticCommandId::AsideToggle);
    for _ in 0..12 {
        press(&mut application, KeyCode::Down);
    }
    let rows = aside_rows(&application);
    assert_eq!(
        focused_aside_rows(&application),
        ["└ ✓ Task 11", "    docs", "    gpt-5.5"],
        "the last entry is in view, whole: {rows:#?}"
    );
    let body = rows[1..]
        .iter()
        .take_while(|row| !row.is_empty())
        .collect::<Vec<_>>();
    assert!(
        body.first().is_some_and(|row| row.contains("Task")),
        "the window begins on an entry's first line, never partway through one: {body:#?}"
    );
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::ViewAndAttachSession(local(last))
    );
}

#[test]
fn opening_a_subsession_keeps_the_sidekicks_tree_and_a_session_only_acted_on_shows_its_own() {
    let (mut application, sidekick, workspace) = sidekick_open(false);
    let before = aside_rows(&application);

    open(&mut application, workspace.path(), sidekick.subsession);
    assert_eq!(
        aside_rows(&application),
        before,
        "a Subsession's tree is its Sidekick's, so the Section stands as it was"
    );

    open(&mut application, workspace.path(), sidekick.docs);
    assert_eq!(
        aside_rows(&application)[..2],
        ["Subagents", ""],
        "a Session only acted on is not answered with its Sidekick's tree"
    );
    deliver(
        &mut application,
        sidekick.docs,
        SubagentTreeEvent::Snapshot(SubagentTreeSnapshot {
            revision: SubagentTreeRevision::INITIAL,
            top_level: SubagentTreeTopLevel {
                own_working_since: None,
                status: None,
                worked_ms: None,
                session_id: sidekick.docs,
                title: "Tidy the docs".to_owned(),
                working_since: None,
                monitoring_since: None,
                needs_intervention: false,
                sidekick: false,
            },
            subagents: Vec::new(),
            sessions: Vec::new(),
        }),
    );
    assert_eq!(
        aside_rows(&application)[..3],
        ["Subagents 0", "Tidy the docs", ""],
        "it heads its own tree, with no Sidekick above it"
    );
}

#[test]
fn deleting_the_sidekick_leaves_an_open_subsession_showing_its_own_tree() {
    let (mut application, sidekick, workspace) = sidekick_open(false);
    // Opened from the Sidekick's Session, the Subsession is answered with the
    // tree followed through it.
    open(&mut application, workspace.path(), sidekick.subsession);
    assert_eq!(aside_rows(&application)[0], "Sessions 5 (2 active)");

    // The Sidekick's Session is deleted elsewhere, and its tree with it.
    deliver(&mut application, sidekick.top, SubagentTreeEvent::Deleted);
    // The Subsession is left standing, so its own tree is asked for through
    // it.
    deliver(
        &mut application,
        sidekick.subsession,
        SubagentTreeEvent::Snapshot(SubagentTreeSnapshot {
            revision: SubagentTreeRevision::INITIAL,
            top_level: SubagentTreeTopLevel {
                own_working_since: None,
                status: None,
                worked_ms: None,
                session_id: sidekick.subsession,
                title: "Fix the flaky login test".to_owned(),
                working_since: None,
                monitoring_since: None,
                needs_intervention: false,
                sidekick: false,
            },
            subagents: Vec::new(),
            sessions: Vec::new(),
        }),
    );
    assert_eq!(
        aside_rows(&application)[..3],
        ["Subagents 0", "Fix the flaky login test", ""],
        "the Subsession heads its own tree once its Sidekick's Session is gone"
    );
}

#[test]
fn deleting_the_sidekick_while_it_is_open_leaves_nothing_to_show() {
    let (mut application, sidekick, _workspace) = sidekick_open(false);
    deliver(&mut application, sidekick.top, SubagentTreeEvent::Deleted);
    assert_eq!(
        aside_rows(&application)[..2],
        ["Subagents", ""],
        "a deleted tree is dropped without complaint"
    );
}

/// A client whose Aside is `aside_width` columns wide, rule included, with
/// the Sidebar kept off the frame.
fn narrow_client(workspace: &std::path::Path, aside_width: u16) -> Application {
    let mut application = connected_application(workspace);
    let mut settings = EffectiveSettings::default();
    settings.sidebar.initial_visibility = SidebarVisibility::Hidden;
    settings.aside.initial_width = u64::from(aside_width);
    deliver_settings(&mut application, settings);
    application
}

#[test]
fn at_the_narrowest_aside_the_slot_stands_whole_and_what_precedes_it_gives_way() {
    /// The narrowest an Aside is drawn, rule included.
    const NARROWEST: u16 = 24;
    let workspace = workspace_dir();
    let sidekick = Sidekick::new(workspace.path());
    let mut application = narrow_client(workspace.path(), NARROWEST);
    open(&mut application, workspace.path(), sidekick.top);
    let mut snapshot = sidekick.snapshot();
    // The working Session and its Subagent each wait on an Intervention of
    // their own, beneath entries that follow them.
    for session in &mut snapshot.sessions {
        if session.session_id == sidekick.build {
            session.needs_intervention = true;
        }
    }
    snapshot.subagents[1].needs_intervention = true;
    let nested = SessionId::new();
    snapshot.subagents.push(SubagentTreeEntry {
        needs_intervention: true,
        ..subagent(
            nested,
            sidekick.probe,
            ("Review", "Review the probe"),
            ActivityStatus::Active,
            None,
        )
    });
    deliver(
        &mut application,
        sidekick.top,
        SubagentTreeEvent::Snapshot(snapshot),
    );

    let rows = aside_rows_at(&application, NARROWEST);
    let content = usize::from(NARROWEST - 3);
    for row in &rows {
        assert!(
            row.chars().count() <= content,
            "no line runs past the Aside's {content} columns: {row:?} in {rows:#?}"
        );
    }
    let interventions = rows
        .iter()
        .filter(|row| row.ends_with(" Needs Intervention"))
        .count();
    assert_eq!(
        interventions, 3,
        "the Session's third line, its Subagent's second and the nested Subagent's second \
         each keep the whole slot: {rows:#?}"
    );
    let entry = rows
        .iter()
        .position(|row| row.contains("Untangle"))
        .expect("the working Session's entry is drawn");
    assert_eq!(
        rows[entry + 2],
        "│  Needs Intervention",
        "the Model gives way, then the guides, never the slot: {rows:#?}"
    );
}

/// Opens the Subagent Picker with Down, as a reader in the composer does.
fn open_picker(application: &mut Application) -> String {
    press(application, KeyCode::Down);
    rendered_application_rows_at(application, WIDTH, HEIGHT).join("\n")
}

#[test]
fn the_picker_offers_the_working_sessions_and_stopping_one_interrupts_it() {
    let (mut application, sidekick, _workspace) = sidekick_open(false);

    let text = open_picker(&mut application);
    assert!(
        text.contains("└ ⠋ build · gpt-5.5: Untangle the build"),
        "the working Session the Sidekick acted on is offered: {text}"
    );
    assert!(
        !text.contains(": Fix the flaky login test") && !text.contains(": Tidy the docs"),
        "settled Sessions are not the picker's concern: {text}"
    );
    assert!(
        text.contains("Enter open · x stop · Esc close"),
        "it may be stopped from its row: {text}"
    );
    assert_eq!(
        press(&mut application, KeyCode::Char('x')),
        ApplicationTransition::InterruptSession {
            session: local(sidekick.build)
        },
        "stopping it interrupts it as the user stops any Session of theirs"
    );
    assert_eq!(
        press(&mut application, KeyCode::Enter),
        ApplicationTransition::ViewAndAttachSession(local(sidekick.build)),
        "and choosing it opens it as the Session it is"
    );
}

#[test]
fn the_picker_keeps_a_session_while_its_subagents_work_and_closes_when_nothing_does() {
    let (mut application, sidekick, workspace) = sidekick_open(false);
    open_picker(&mut application);
    let mut settled = sidekick.session(
        sidekick.build,
        "Untangle the build",
        "build",
        ActivityStatus::Completed,
        1_000,
    );
    settled.worked_ms = Some(5_000);
    change(
        &mut application,
        sidekick.top,
        SubagentTreeChange::SessionChanged { entry: settled },
    );
    let text = rendered_application_rows_at(&application, WIDTH, HEIGHT).join("\n");
    assert!(
        text.contains("└ ⠋ build · gpt-5.5: Untangle the build"),
        "a Session whose own Turn settled still works while its Subagent does: {text}"
    );
    assert_eq!(
        press(&mut application, KeyCode::Char('x')),
        ApplicationTransition::InterruptSession {
            session: local(sidekick.build)
        },
        "and may still be stopped"
    );
    let rows = aside_rows(&application);
    assert!(
        rows.contains(&"├ ✓ Untangle the build".to_owned()),
        "while its entry wears the Marker its own Turn settled with: {rows:#?}"
    );

    change(
        &mut application,
        sidekick.top,
        SubagentTreeChange::SubagentWorkingChanged {
            session_id: sidekick.probe,
            status: ActivityStatus::Completed,
            worked_ms: Some(2_000),
            working_since: None,
            monitoring_since: None,
        },
    );
    let text = rendered_application_rows_at(&application, WIDTH, HEIGHT).join("\n");
    assert!(
        !text.contains(": Untangle the build"),
        "the picker closes once nothing in it works: {text}"
    );

    // From a Subsession, the Sidekick's Sessions are not its own to offer.
    let (mut application, sidekick, _workspace) = sidekick_open(false);
    open(&mut application, workspace.path(), sidekick.subsession);
    let text = open_picker(&mut application);
    assert!(
        !text.contains(": Untangle the build"),
        "only the Sidekick's own Session offers them: {text}"
    );
}

/// The Remote the fixture's Sessions on a Remote live on.
const REMOTE: &str = "workstation";

/// A Session the Sidekick acted on at [`REMOTE`], titled `title`, working in
/// the Workspace named `workspace` there.
fn remote_entry(session_id: SessionId, title: &str, workspace: &str) -> SubagentTreeSession {
    SubagentTreeSession {
        unconfirmed: false,
        session_id,
        origin: Some(REMOTE.to_owned()),
        unanswered: false,
        title: title.to_owned(),
        subsession: false,
        workspace_path: std::path::PathBuf::from("/srv").join(workspace),
        workspace_icon: None,
        model: Some(ModelId::new("sonnet")),
        status: Some(ActivityStatus::Completed),
        worked_ms: None,
        working_since: None,
        monitoring_since: None,
        needs_intervention: false,
        acted_at: SessionTimestamp(4_000),
    }
}

/// A client with the Sidekick's Session open and its tree in hand, listing
/// `entry` alone beneath it.
fn sidekick_listing(entry: SubagentTreeSession, show_icons: bool) -> (Application, Sidekick) {
    let workspace = workspace_dir();
    let sidekick = Sidekick::new(workspace.path());
    let mut application = client(workspace.path(), show_icons);
    open(&mut application, workspace.path(), sidekick.top);
    let mut snapshot = sidekick.snapshot();
    snapshot.subagents.clear();
    snapshot.sessions = vec![entry];
    deliver(
        &mut application,
        sidekick.top,
        SubagentTreeEvent::Snapshot(snapshot),
    );
    (application, sidekick)
}

/// The three-line entry of a Session at [`REMOTE`] working in the Workspace
/// named `workspace`, alone beneath a Sidekick's Session in an Aside at its
/// launch width.
fn remote_entry_lines(workspace: &str, show_icons: bool) -> Vec<String> {
    let (application, _) = sidekick_listing(
        remote_entry(SessionId::new(), "Bind the ledger", workspace),
        show_icons,
    );
    aside_rows(&application)[2..5].to_vec()
}

#[test]
fn a_session_on_a_remote_names_its_remote_first_which_gives_way_before_its_workspace() {
    assert_eq!(
        remote_entry_lines("ledger", false),
        [
            "└ ✓ Bind the ledger",
            "    workstation · ledger",
            "    sonnet"
        ],
        "the Remote's name leads the line where the Workspace is named"
    );
    assert_eq!(
        remote_entry_lines("ledger", true)[1],
        format!("    workstation · {FOLDER} ledger"),
        "ahead of the Workspace's Icon"
    );
    assert_eq!(
        remote_entry_lines("ledger-of-account", false)[1],
        "    work… · ledger-of-account",
        "where the line runs short the Remote's name is cut first"
    );
    assert_eq!(
        remote_entry_lines("ledger-of-the-account", false)[1],
        "    ledger-of-the-account",
        "then left out, with what parts it from the Workspace"
    );
    assert_eq!(
        remote_entry_lines("documentation-of-every-service", false)[1],
        "    documentation-of-every-s…",
        "and only then is the Workspace's name cut"
    );
}

#[test]
fn a_session_whose_remote_does_not_answer_keeps_what_named_it_and_says_it_does_not_answer() {
    let unanswered = SubagentTreeSession {
        unanswered: true,
        model: None,
        status: None,
        ..remote_entry(SessionId::new(), "Bind the ledger", "ledger")
    };
    let (application, _) = sidekick_listing(unanswered.clone(), true);
    let rows = aside_rows(&application);
    assert_eq!(rows[0], "Sessions 1", "it is listed still");
    assert_eq!(
        rows[2..5],
        [
            "└ Bind the ledger".to_owned(),
            format!("    workstation · {FOLDER} ledger"),
            "    not answering".to_owned(),
        ],
        "its last Title, Workspace and Remote, without a Marker, Model or time"
    );
    let buffer = rendered_application_buffer(&application, WIDTH, HEIGHT);
    let column = WIDTH - ASIDE_WIDTH + 2;
    assert_eq!(
        (buffer[(column + 2, 2)].symbol(), buffer[(column + 2, 2)].fg),
        ("B", buffer[(column + 4, 3)].fg),
        "its Title drawn dimmed, as its Workspace is"
    );

    let never_said = SubagentTreeSession {
        title: String::new(),
        workspace_path: std::path::PathBuf::new(),
        ..unanswered
    };
    let (application, _) = sidekick_listing(never_said, true);
    assert_eq!(
        aside_rows(&application)[2..5],
        ["└ not answering", "    workstation", "    not answering"],
        "one whose Remote never said what it is, is named by its Remote alone"
    );
}

#[test]
fn a_session_the_sidekicks_act_on_is_not_yet_confirmed_stands_as_one_not_answering_does() {
    let unconfirmed = SubagentTreeSession {
        unconfirmed: true,
        model: None,
        status: None,
        ..remote_entry(SessionId::new(), "Bind the ledger", "ledger")
    };
    let (application, _) = sidekick_listing(unconfirmed, false);
    assert_eq!(
        aside_rows(&application)[2..5],
        [
            "└ Bind the ledger",
            "    workstation · ledger",
            "    not confirmed"
        ],
        "named by what it asked for, saying it is not yet confirmed in place of its Model and time"
    );
}

#[test]
fn opening_a_session_on_a_remote_turns_the_outlook_toward_that_remote() {
    let ledger = SessionId::new();
    let (mut application, _) =
        sidekick_listing(remote_entry(ledger, "Bind the ledger", "ledger"), false);
    let opened = SessionReference::new(Outlook::Remote(REMOTE.to_owned()), ledger);
    for line in ["Bind the ledger", "workstation · ledger"] {
        let (mut application, _) =
            sidekick_listing(remote_entry(ledger, "Bind the ledger", "ledger"), false);
        assert!(
            matches!(
                click_on(&mut application, line),
                ApplicationTransition::TurnOutlookAndViewAndAttach { session, .. }
                    if session == opened
            ),
            "pressing {line:?} opens the Session on its Remote, turning the Outlook there"
        );
    }
    invoke(&mut application, SemanticCommandId::AsideToggle);
    press(&mut application, KeyCode::Down);
    assert!(
        matches!(
            press(&mut application, KeyCode::Enter),
            ApplicationTransition::TurnOutlookAndViewAndAttach { session, .. } if session == opened
        ),
        "and so does Enter on its entry"
    );
}

/// The Session on [`REMOTE`] that `session_id` names, as this client reaches
/// it.
fn on_remote(session_id: SessionId) -> SessionReference {
    SessionReference::new(Outlook::Remote(REMOTE.to_owned()), session_id)
}

/// A Subagent of a Session on [`REMOTE`], spawned by `parent` there.
fn remote_subagent(session_id: SessionId, parent: SessionId, title: &str) -> SubagentTreeEntry {
    SubagentTreeEntry {
        origin: Some(REMOTE.to_owned()),
        ..subagent(
            session_id,
            parent,
            ("Explore", title),
            ActivityStatus::Completed,
            Some(4_000),
        )
    }
}

#[test]
fn opening_a_subsession_begun_on_a_remote_keeps_the_sidekicks_tree_here() {
    let ledger = SessionId::new();
    let begun = SubagentTreeSession {
        subsession: true,
        ..remote_entry(ledger, "Bind the ledger", "ledger")
    };
    let (mut application, sidekick) = sidekick_listing(begun, false);
    let before = aside_rows(&application);
    assert!(matches!(
        click_on(&mut application, "Bind the ledger"),
        ApplicationTransition::TurnOutlookAndViewAndAttach { session, .. }
            if session == on_remote(ledger)
    ));
    let workspace = workspace_dir();
    open(&mut application, workspace.path(), ledger);
    assert_eq!(
        aside_rows(&application)[..5],
        before[..5],
        "a Subsession's tree is its Sidekick's, wherever it was begun"
    );
    change(
        &mut application,
        sidekick.top,
        SubagentTreeChange::TopLevelRetitled {
            title: "Plan the month".to_owned(),
        },
    );
    assert_eq!(
        aside_rows(&application)[1],
        "Plan the month",
        "and it is still followed through this client's own Server"
    );

    let (mut application, _) =
        sidekick_listing(remote_entry(ledger, "Bind the ledger", "ledger"), false);
    click_on(&mut application, "Bind the ledger");
    open(&mut application, workspace.path(), ledger);
    assert_eq!(
        aside_rows(&application)[..2],
        ["Subagents", ""],
        "a Session only acted on there heads its own tree there"
    );
}

#[test]
fn a_remote_sessions_subagents_stand_beneath_it_alone_and_open_on_its_remote() {
    let shared = SessionId::new();
    let explore = SessionId::new();
    let (mut application, sidekick) = sidekick_listing(
        SubagentTreeSession {
            worked_ms: Some(2_000),
            ..remote_entry(shared, "Bind the ledger", "ledger")
        },
        false,
    );
    // A Session of this Server's sharing the Remote's Session's identity,
    // acted on before it.
    let workspace = workspace_dir();
    let mine = Sidekick::new(workspace.path()).session(
        shared,
        "Audit the ledger",
        "audit",
        ActivityStatus::Completed,
        1_000,
    );
    change(
        &mut application,
        sidekick.top,
        SubagentTreeChange::SessionChanged { entry: mine },
    );
    change(
        &mut application,
        sidekick.top,
        SubagentTreeChange::SubagentSpawned {
            entry: remote_subagent(explore, shared, "Read the old entries"),
        },
    );
    let rows = aside_rows(&application);
    assert_eq!(
        rows[2..10],
        [
            "├ ✓ Bind the ledger",
            "│ │ workstation · ledger",
            "│ │ sonnet                 2s",
            "│ └ ✓ Read the old entries",
            "│     Explore              4s",
            "└ ✓ Audit the ledger",
            "    audit",
            "    gpt-5.5                3s",
        ],
        "the Remote's Subagent stands beneath its Remote's Session alone, which says its \
         Model and time"
    );
    assert!(
        matches!(
            click_on(&mut application, "Read the old entries"),
            ApplicationTransition::TurnOutlookAndViewAndAttach { session, .. }
                if session == on_remote(explore)
        ),
        "and opens on that Remote, turning the Outlook there"
    );
}

#[test]
fn the_picker_offers_a_working_session_on_a_remote_and_stops_it_there() {
    let ledger = SessionId::new();
    let (mut application, _) = sidekick_listing(
        SubagentTreeSession {
            status: Some(ActivityStatus::Active),
            ..remote_entry(ledger, "Bind the ledger", "ledger")
        },
        false,
    );
    let text = open_picker(&mut application);
    assert!(
        text.contains("└ ⠋ workstation · ledger · sonnet: Bind the ledger"),
        "it is offered, named by its Remote: {text}"
    );
    assert!(
        text.contains("Enter open · x stop · Esc close"),
        "and may be stopped from its row: {text}"
    );
    assert_eq!(
        press(&mut application, KeyCode::Char('x')),
        ApplicationTransition::InterruptSession {
            session: on_remote(ledger)
        },
        "stopping it interrupts it on its Remote"
    );
    assert!(
        matches!(
            press(&mut application, KeyCode::Enter),
            ApplicationTransition::TurnOutlookAndViewAndAttach { session, .. }
                if session == on_remote(ledger)
        ),
        "and choosing it opens it there"
    );
}
