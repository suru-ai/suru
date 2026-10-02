//! Hiding Subsessions (`sidekick.hideSubsessions`): what the Sidebar, its
//! search, and the Session picker list with the Setting off and on, and what a
//! Sidekick's row carries for the Subsessions it hides.

use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::support::{
    SIDEBAR_WIDE, connected_application, connected_application_homed, deliver_settings,
    drawn_in_sidebar, failed_session_snapshot, listed_session, rendered_application_buffer,
    rendered_application_rows_at, sidebar_column, text_on, type_terminal_text, workspace_dir,
};
use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Color;
use suru::{
    managed_client::{ManagedEvent, SubagentTreeEvent},
    protocol::{
        ActivityStatus, AsideSettings, AsideVisibility, Author, EffectiveSettings,
        LatestTurnStatus, Outlook, PromptId, QuestionnaireId, Remote, RemoteStatus, SessionDeleted,
        SessionId, SessionListItem, SessionReference, SessionRevision, SessionStatus,
        SessionTimestamp, SidebarScope, SidebarSettings, SidebarVisibility, SidekickSettings,
        SubagentTreeEntry, SubagentTreeRevision, SubagentTreeSession, SubagentTreeSnapshot,
        SubagentTreeTopLevel, TurnStatus, UnreadableSessionSummary,
    },
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, SemanticCommandId,
        SessionListRequest, SessionListScope, SessionListSurface,
    },
};

/// Tall enough that every row these listings hold is drawn.
const TALL: u16 = 40;

/// The Sidebar's own columns, which is where its highlight is read.
const SIDEBAR_COLUMNS: std::ops::Range<u16> = 0..31;

/// One Sidekick's Session and a Subsession it began, rooted where the
/// fixture says: the Sidekick in a Sidekick Workspace of its own, the
/// Subsession in the Repository it was begun to work in.
struct Sidekick {
    session: SessionId,
    subsession: SessionId,
}

impl Sidekick {
    fn new() -> Self {
        Self {
            session: SessionId::new(),
            subsession: SessionId::new(),
        }
    }

    /// The Sidekick's own Session, listed in the Sidekick Workspace beneath
    /// `root`.
    fn own(&self, root: &Path) -> SessionListItem {
        listed(
            self.session,
            "Sidekick at work",
            &sidekick_workspace(root),
            2,
        )
    }

    /// The Subsession it began, listed in the Repository beneath `root`.
    fn began(&self, root: &Path) -> SessionListItem {
        begun_by(
            listed(self.subsession, "Fixing the parser", &repository(root), 3),
            self.session,
        )
    }
}

fn sidekick_workspace(root: &Path) -> std::path::PathBuf {
    root.join("sidekick")
}

fn repository(root: &Path) -> std::path::PathBuf {
    root.join("repository")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("read the clock")
        .as_millis()
        .try_into()
        .expect("the clock fits a Session timestamp")
}

fn minutes_ago(minutes: u64) -> u64 {
    now().saturating_sub(minutes * 60 * 1_000)
}

fn days_ago(days: u64) -> u64 {
    minutes_ago(days * 24 * 60)
}

/// A readable Session last active a moment ago.
fn listed(
    session_id: SessionId,
    title: &str,
    workspace: &Path,
    created_at: u64,
) -> SessionListItem {
    listed_session(session_id, title, workspace, created_at, now())
}

fn edit(
    session: SessionListItem,
    change: impl FnOnce(&mut suru::protocol::SessionSummary),
) -> SessionListItem {
    let SessionListItem::Readable(mut summary) = session else {
        unreachable!("the fixture builds a readable Session");
    };
    change(&mut summary);
    SessionListItem::Readable(summary)
}

/// The Session as a Subsession the Sidekick of `sidekick` began.
fn begun_by(session: SessionListItem, sidekick: SessionId) -> SessionListItem {
    edit(session, |summary| {
        summary.session.begun_by = Some(Author::Sidekick {
            session_id: sidekick,
            title: "Sidekick at work".to_owned(),
        });
    })
}

fn working(session: SessionListItem, since: u64) -> SessionListItem {
    edit(session, |summary| {
        summary.session.status = SessionStatus::Active;
        summary.session.working_since = Some(SessionTimestamp(since));
    })
}

fn monitoring(session: SessionListItem, since: u64) -> SessionListItem {
    edit(session, |summary| {
        summary.session.monitoring_since = Some(SessionTimestamp(since));
    })
}

/// Working, and owing a Questionnaire the reader has yet to answer.
fn owing_an_answer(session: SessionListItem) -> SessionListItem {
    edit(working(session, minutes_ago(5)), |summary| {
        summary.standing_inputs.pending_questionnaires = vec![QuestionnaireId::new()];
        summary.standing_inputs.pending_questionnaires_revision = SessionRevision(3);
    })
}

/// Its latest Turn settled as `status` a moment ago, and no Client has viewed
/// it since.
fn settled_as(session: SessionListItem, status: TurnStatus) -> SessionListItem {
    edit(session, |summary| {
        summary.standing_inputs.latest_turn = Some(LatestTurnStatus {
            status,
            settled_at: Some(SessionTimestamp(minutes_ago(1))),
        });
    })
}

/// Set aside by the reader.
fn set_aside(session: SessionListItem, at: u64) -> SessionListItem {
    edit(session, |summary| {
        summary.settled_at = Some(SessionTimestamp(at))
    })
}

/// Last active `at`, which is what settles a Session left alone long enough.
fn last_active(session: SessionListItem, at: u64) -> SessionListItem {
    edit(session, |summary| {
        summary.created_at = SessionTimestamp(at.saturating_sub(1_000));
        summary.updated_at = SessionTimestamp(at);
    })
}

/// The Settings a reader with the Sidebar on screen leaves, hiding Subsessions
/// or not.
fn hiding(hide_subsessions: bool) -> EffectiveSettings {
    EffectiveSettings {
        sidebar: SidebarSettings {
            initial_visibility: SidebarVisibility::Shown,
            ..SidebarSettings::default()
        },
        aside: AsideSettings {
            initial_visibility: AsideVisibility::Hidden,
            ..AsideSettings::default()
        },
        sidekick: SidekickSettings { hide_subsessions },
        ..EffectiveSettings::default()
    }
}

/// A connected client whose Sidebar has taken `settings` and been answered
/// with `sessions`.
fn sidebar_under(
    workspace: &Path,
    settings: EffectiveSettings,
    sessions: Vec<SessionListItem>,
) -> Application {
    let mut application = connected_application(workspace);
    let ApplicationTransition::ListSessions(request) = deliver_settings(&mut application, settings)
    else {
        panic!("a Sidebar coming into view asks for its Sessions");
    };
    assert_eq!(request.surface(), SessionListSurface::Sidebar);
    application
        .handle_event(ApplicationEvent::SessionsListed { request, sessions })
        .expect("hydrate the Sidebar");
    application
}

fn sidebar_hiding(
    workspace: &Path,
    hide_subsessions: bool,
    sessions: Vec<SessionListItem>,
) -> Application {
    sidebar_under(workspace, hiding(hide_subsessions), sessions)
}

/// The Sidebar's own lines, top to bottom, with nothing beside them.
fn sidebar_lines(application: &Application) -> Vec<String> {
    rendered_application_rows_at(application, SIDEBAR_WIDE, TALL)
        .iter()
        .map(|row| sidebar_column(row))
        .collect()
}

fn listed_in_sidebar(application: &Application, title: &str) -> bool {
    drawn_in_sidebar(
        &rendered_application_rows_at(application, SIDEBAR_WIDE, TALL),
        title,
    )
}

/// What the right slot of the active row titled `title` says: the end of the
/// line above its Title, where the slot stands.
fn slot_of(application: &Application, title: &str) -> String {
    let lines = sidebar_lines(application);
    let at = lines
        .iter()
        .position(|line| line.contains(title))
        .unwrap_or_else(|| panic!("the Sidebar drew no row titled {title:?}: {lines:?}"));
    lines[at - 1].clone()
}

/// Whether the row titled `title` stands on the settled shelf, below the
/// divider closing the active list.
fn stands_settled(application: &Application, title: &str) -> bool {
    let lines = sidebar_lines(application);
    let at = lines
        .iter()
        .position(|line| line.contains(title))
        .unwrap_or_else(|| panic!("the Sidebar drew no row titled {title:?}: {lines:?}"));
    lines
        .iter()
        .position(|line| line.starts_with("Settled"))
        .is_some_and(|divider| divider < at)
}

/// The Sidebar row standing for the Session the main view has open.
fn open_sidebar_text(application: &Application) -> String {
    let buffer = rendered_application_buffer(application, SIDEBAR_WIDE, TALL);
    (0..TALL)
        .map(|row| {
            SIDEBAR_COLUMNS
                .filter_map(|column| buffer.cell((column, row)))
                .filter(|cell| cell.fg == Color::Cyan)
                .map(|cell| cell.symbol())
                .collect::<String>()
        })
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The Sidebar row the reader is on while they are driving the Sidebar.
fn focused_sidebar_text(application: &Application) -> String {
    text_on(
        application,
        Color::Blue,
        (SIDEBAR_WIDE, TALL),
        SIDEBAR_COLUMNS,
    )
}

fn press(application: &mut Application, code: KeyCode, modifiers: KeyModifiers) {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(code, modifiers)))
        .expect("press a key");
}

/// Brings the reader into a Sidebar the Setting revealed.
fn enter_sidebar(application: &mut Application) {
    press(application, KeyCode::Char('b'), KeyModifiers::CONTROL);
}

/// Puts `session_id` in the main view, as opening it from anywhere does.
fn open_session(application: &mut Application, workspace: &Path, session_id: SessionId) {
    application
        .handle_event(ApplicationEvent::SessionAttached(failed_session_snapshot(
            session_id,
            PromptId::new(),
            "Initial Prompt",
            workspace,
        )))
        .expect("open a Session in the main view");
}

#[test]
fn subsessions_are_listed_like_any_session_until_the_reader_hides_them() {
    let workspace = workspace_dir();
    let root = workspace.path();
    let sidekick = Sidekick::new();
    let sessions = vec![
        sidekick.own(root),
        working(sidekick.began(root), minutes_ago(5)),
        // A Session the Sidekick only sent a Prompt: nothing about it names
        // the Sidekick as the one that began it.
        working(
            listed(SessionId::new(), "Acted on only", &repository(root), 1),
            minutes_ago(9),
        ),
    ];
    let mut application = sidebar_hiding(root, false, sessions);

    assert!(
        !EffectiveSettings::default().sidekick.hide_subsessions,
        "Subsessions are listed unless the reader says otherwise"
    );
    assert!(listed_in_sidebar(&application, "Fixing the parser"));
    assert!(
        !slot_of(&application, "Sidekick at work").contains("Working"),
        "with the Setting off nothing rolls up: {:?}",
        slot_of(&application, "Sidekick at work")
    );

    deliver_settings(&mut application, hiding(true));
    assert!(
        !listed_in_sidebar(&application, "Fixing the parser"),
        "the Setting takes the Subsession off the Sidebar the moment it lands: {:?}",
        sidebar_lines(&application)
    );
    assert!(
        slot_of(&application, "Sidekick at work").ends_with("Working 5m"),
        "and its Sidekick's row carries its Standing: {:?}",
        slot_of(&application, "Sidekick at work")
    );
    assert!(
        listed_in_sidebar(&application, "Acted on only"),
        "a Session a Sidekick only acted on is never hidden"
    );
    assert!(
        slot_of(&application, "Acted on only").ends_with("Working 9m"),
        "and it keeps its Standing as its own"
    );

    deliver_settings(&mut application, hiding(false));
    assert!(
        listed_in_sidebar(&application, "Fixing the parser"),
        "turning it off lists the Subsession again at once"
    );
    assert!(!slot_of(&application, "Sidekick at work").contains("Working"));
}

/// The Sidekick's row presents the highest-precedence Standing among itself
/// and the Subsessions it hides, an owed Intervention above all.
#[test]
fn a_sidekicks_row_carries_the_highest_standing_among_itself_and_the_subsessions_it_hides() {
    type Reading = fn(SessionListItem) -> SessionListItem;
    let idle: Reading = |session| session;
    let cases: [(&str, Reading, Reading, &str); 8] = [
        (
            "an owed Intervention",
            idle,
            owing_an_answer,
            "Needs Intervention",
        ),
        (
            "work",
            idle,
            |session| working(session, minutes_ago(5)),
            "Working 5m",
        ),
        (
            "a failure",
            idle,
            |session| settled_as(session, TurnStatus::Failed),
            "Failed",
        ),
        (
            "a Watch",
            idle,
            |session| monitoring(session, minutes_ago(7)),
            "Monitoring 7m",
        ),
        (
            "an outcome",
            idle,
            |session| settled_as(session, TurnStatus::Completed),
            "Done",
        ),
        (
            "an owed Intervention over the Sidekick's own work",
            |session| working(session, minutes_ago(2)),
            owing_an_answer,
            "Needs Intervention",
        ),
        (
            "work over the Sidekick's own failure",
            |session| settled_as(session, TurnStatus::Failed),
            |session| working(session, minutes_ago(5)),
            "Working 5m",
        ),
        (
            "the Sidekick's own Intervention over an outcome",
            owing_an_answer,
            |session| settled_as(session, TurnStatus::Completed),
            "Needs Intervention",
        ),
    ];
    for (case, sidekicks, subsessions, slot) in cases {
        let workspace = workspace_dir();
        let root = workspace.path();
        let sidekick = Sidekick::new();
        let application = sidebar_hiding(
            root,
            true,
            vec![
                sidekicks(sidekick.own(root)),
                subsessions(sidekick.began(root)),
            ],
        );

        assert!(
            !listed_in_sidebar(&application, "Fixing the parser"),
            "{case}: the Subsession is hidden"
        );
        assert!(
            slot_of(&application, "Sidekick at work").ends_with(slot),
            "{case}: the Sidekick's row reads {slot:?}, not {:?}",
            slot_of(&application, "Sidekick at work")
        );
    }
}

#[test]
fn a_subsession_whose_sidekicks_session_is_gone_is_listed_regardless() {
    let workspace = workspace_dir();
    let root = workspace.path();
    let sidekick = Sidekick::new();
    let orphan = begun_by(
        listed(
            SessionId::new(),
            "Begun by a deleted Sidekick",
            &repository(root),
            4,
        ),
        SessionId::new(),
    );
    let unreadable_sidekick = SessionId::new();
    let unreachable = begun_by(
        listed(
            SessionId::new(),
            "Begun by an unreadable one",
            &repository(root),
            5,
        ),
        unreadable_sidekick,
    );
    let mut application = sidebar_hiding(
        root,
        true,
        vec![
            sidekick.own(root),
            working(sidekick.began(root), minutes_ago(5)),
            orphan,
            unreachable,
            SessionListItem::Unreadable(UnreadableSessionSummary {
                id: unreadable_sidekick,
                title: "Sidekick nobody can read".to_owned(),
                created_at: SessionTimestamp(6),
                updated_at: SessionTimestamp(now()),
                workspace: None,
            }),
        ],
    );

    assert!(
        listed_in_sidebar(&application, "Begun by a deleted Sidekick"),
        "nothing else would lead to a Subsession whose Sidekick's Session is gone"
    );
    assert!(
        listed_in_sidebar(&application, "Begun by an unreadable one"),
        "nor to one whose Sidekick's Session cannot be opened"
    );
    assert!(!listed_in_sidebar(&application, "Fixing the parser"));

    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::SessionDeleted(
            SessionDeleted {
                session_id: sidekick.session,
            },
        )))
        .expect("take the Sidekick's Session's deletion");
    assert!(
        listed_in_sidebar(&application, "Fixing the parser"),
        "deleting the Sidekick's Session lists its Subsession again: {:?}",
        sidebar_lines(&application)
    );
    assert!(
        slot_of(&application, "Fixing the parser").ends_with("Working 5m"),
        "with its Standing its own again"
    );
}

#[test]
fn the_open_session_highlight_stands_on_the_sidekicks_row_while_its_subsession_is_hidden() {
    let workspace = workspace_dir();
    let root = workspace.path();
    let sidekick = Sidekick::new();
    let mut application = sidebar_hiding(
        root,
        true,
        vec![
            listed(SessionId::new(), "Other work", root, 1),
            sidekick.own(root),
            sidekick.began(root),
        ],
    );

    open_session(&mut application, &repository(root), sidekick.subsession);
    assert_eq!(
        open_sidebar_text(&application),
        "Sidekick at work",
        "the hidden Subsession's highlight stands on the row that carries it"
    );
    enter_sidebar(&mut application);
    assert!(
        focused_sidebar_text(&application).contains("Sidekick at work"),
        "entering the Sidebar begins row focus on that row: {:?}",
        focused_sidebar_text(&application)
    );

    deliver_settings(&mut application, hiding(false));
    assert_eq!(
        open_sidebar_text(&application),
        "Fixing the parser",
        "listed, the Subsession's own row carries its highlight"
    );
}

/// A Subagent of a hidden Subsession answers for the Subsession, as it would
/// for any top-level Session it works beneath, and so for the Sidekick's row
/// carrying it.
#[test]
fn a_subagent_of_a_hidden_subsession_highlights_its_sidekicks_row() {
    let workspace = workspace_dir();
    let root = workspace.path();
    let sidekick = Sidekick::new();
    let subagent = SessionId::new();
    let mut application = sidebar_hiding(
        root,
        true,
        vec![
            listed(SessionId::new(), "Other work", root, 1),
            sidekick.own(root),
            sidekick.began(root),
        ],
    );
    let mut snapshot = failed_session_snapshot(
        subagent,
        PromptId::new(),
        "Delegated work",
        &repository(root),
    );
    snapshot.session.parent = Some(sidekick.subsession);
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot))
        .expect("open the Subsession's Subagent");
    application
        .handle_event(ApplicationEvent::SubagentTree {
            through: SessionReference::new(Outlook::Local, subagent),
            event: SubagentTreeEvent::Snapshot(sidekicks_tree(&sidekick, subagent, root)),
        })
        .expect("take the Sidekick's tree");

    assert_eq!(open_sidebar_text(&application), "Sidekick at work");
}

/// The tree a Client is given through any Session of a Sidekick's: the
/// Sidekick's Session heading it, its Subsession beneath, and one Subagent of
/// that Subsession's.
fn sidekicks_tree(sidekick: &Sidekick, subagent: SessionId, root: &Path) -> SubagentTreeSnapshot {
    SubagentTreeSnapshot {
        revision: SubagentTreeRevision::INITIAL,
        top_level: SubagentTreeTopLevel {
            session_id: sidekick.session,
            title: "Sidekick at work".to_owned(),
            working_since: None,
            monitoring_since: None,
            needs_intervention: false,
            sidekick: true,
        },
        subagents: vec![SubagentTreeEntry {
            session_id: subagent,
            parent_session_id: sidekick.subsession,
            spawn_order: 0,
            name: "Explore".to_owned(),
            title: "Explore the seams".to_owned(),
            model: None,
            status: ActivityStatus::Active,
            worked_ms: Some(0),
            working_since: None,
            monitoring_since: None,
            needs_intervention: false,
        }],
        sessions: vec![SubagentTreeSession {
            session_id: sidekick.subsession,
            title: "Fixing the parser".to_owned(),
            subsession: true,
            workspace_path: repository(root),
            workspace_icon: None,
            model: None,
            status: Some(ActivityStatus::Active),
            worked_ms: Some(0),
            working_since: None,
            monitoring_since: None,
            needs_intervention: false,
            acted_at: SessionTimestamp(3),
        }],
    }
}

#[test]
fn a_search_never_finds_a_subsession_the_reader_hides() {
    let workspace = workspace_dir();
    let root = workspace.path();
    let sidekick = Sidekick::new();
    let sessions = vec![
        sidekick.own(root),
        sidekick.began(root),
        listed(SessionId::new(), "Parser benchmarks", root, 1),
    ];
    let mut application = sidebar_hiding(root, true, sessions.clone());
    enter_sidebar(&mut application);
    type_terminal_text(&mut application, "parser");

    assert!(
        !listed_in_sidebar(&application, "Fixing the parser"),
        "a hidden Subsession is no result, however well its Title matches: {:?}",
        sidebar_lines(&application)
    );
    assert!(
        !listed_in_sidebar(&application, "Sidekick at work"),
        "and its Sidekick's row is found by its own Title alone"
    );
    assert!(listed_in_sidebar(&application, "Parser benchmarks"));

    let mut listing = sidebar_hiding(root, false, sessions);
    enter_sidebar(&mut listing);
    type_terminal_text(&mut listing, "parser");
    assert!(
        listed_in_sidebar(&listing, "Fixing the parser"),
        "listed, the Subsession is found like any other Session"
    );
}

/// A settled Subsession says nothing on its Sidekick's row, as it would say
/// nothing on a settled row of its own — and, hidden, stands on no shelf.
#[test]
fn a_settled_subsession_says_nothing_on_its_sidekicks_row() {
    let workspace = workspace_dir();
    let root = workspace.path();
    let sidekick = Sidekick::new();
    let application = sidebar_hiding(
        root,
        true,
        vec![
            sidekick.own(root),
            set_aside(
                settled_as(sidekick.began(root), TurnStatus::Failed),
                minutes_ago(1),
            ),
        ],
    );

    assert!(
        !slot_of(&application, "Sidekick at work").contains("Failed"),
        "{:?}",
        slot_of(&application, "Sidekick at work")
    );
    assert!(
        !listed_in_sidebar(&application, "Fixing the parser"),
        "a hidden Subsession stands on no shelf"
    );
    assert!(
        !sidebar_lines(&application)
            .iter()
            .any(|line| line.starts_with("Settled")),
        "and opens none: {:?}",
        sidebar_lines(&application)
    );
}

/// A Sidekick's row settles on its own as any Session does, unless a
/// Subsession it hides is still working — work that would keep a row of its
/// own on the active list keeps the row carrying it there. The reader's own
/// say-so still wins.
#[test]
fn a_sidekicks_row_left_alone_stays_active_while_a_subsession_it_hides_works() {
    let workspace = workspace_dir();
    let root = workspace.path();
    let sidekick = Sidekick::new();
    let left_alone = || last_active(sidekick.own(root), days_ago(5));

    let quiet = sidebar_hiding(
        root,
        true,
        vec![
            left_alone(),
            settled_as(sidekick.began(root), TurnStatus::Completed),
        ],
    );
    assert!(
        stands_settled(&quiet, "Sidekick at work"),
        "left alone past the threshold, the Sidekick's row settles: {:?}",
        sidebar_lines(&quiet)
    );

    for (case, subsession) in [
        ("working", working(sidekick.began(root), minutes_ago(5))),
        (
            "Monitoring",
            monitoring(sidekick.began(root), minutes_ago(5)),
        ),
        (
            "owing an Intervention",
            owing_an_answer(sidekick.began(root)),
        ),
    ] {
        let live = sidebar_hiding(root, true, vec![left_alone(), subsession]);
        assert!(
            !stands_settled(&live, "Sidekick at work"),
            "a Subsession {case} keeps its Sidekick's row active: {:?}",
            sidebar_lines(&live)
        );
    }

    let set_aside_by_the_reader = sidebar_hiding(
        root,
        true,
        vec![
            set_aside(sidekick.own(root), minutes_ago(1)),
            working(sidekick.began(root), minutes_ago(5)),
        ],
    );
    assert!(
        stands_settled(&set_aside_by_the_reader, "Sidekick at work"),
        "a row the reader set aside stays where they put it: {:?}",
        sidebar_lines(&set_aside_by_the_reader)
    );
}

/// Opening a Sidekick's Session views none of the Subsessions its row
/// carries; opening the Subsession is what clears its outcome.
#[test]
fn only_opening_a_hidden_subsession_clears_its_outcome_from_its_sidekicks_row() {
    let workspace = workspace_dir();
    let root = workspace.path();
    let sidekick = Sidekick::new();
    let mut application = sidebar_hiding(
        root,
        true,
        vec![
            listed(SessionId::new(), "Other work", root, 1),
            sidekick.own(root),
            settled_as(sidekick.began(root), TurnStatus::Completed),
        ],
    );
    assert!(slot_of(&application, "Sidekick at work").ends_with("Done"));

    open_session(
        &mut application,
        &sidekick_workspace(root),
        sidekick.session,
    );
    assert!(
        slot_of(&application, "Sidekick at work").ends_with("Done"),
        "the Subsession is still unseen: {:?}",
        slot_of(&application, "Sidekick at work")
    );

    open_session(&mut application, &repository(root), sidekick.subsession);
    assert!(
        !slot_of(&application, "Sidekick at work").contains("Done"),
        "opened, the Subsession reads as Viewed: {:?}",
        slot_of(&application, "Sidekick at work")
    );
}

/// The roll-up is the row's Standing and nothing more: what a hidden
/// Subsession owes is answered in the Subsession, never in its Sidekick's.
#[test]
fn a_hidden_subsessions_intervention_never_presents_in_its_sidekicks_session() {
    let workspace = workspace_dir();
    let root = workspace.path();
    let sidekick = Sidekick::new();
    let mut application = sidebar_hiding(
        root,
        true,
        vec![sidekick.own(root), owing_an_answer(sidekick.began(root))],
    );
    open_session(
        &mut application,
        &sidekick_workspace(root),
        sidekick.session,
    );

    assert!(slot_of(&application, "Sidekick at work").ends_with("Needs Intervention"));
    type_terminal_text(&mut application, "carry on");
    let screen = rendered_application_rows_at(&application, SIDEBAR_WIDE, TALL).join("\n");
    assert!(
        screen.contains("carry on"),
        "the keys stay with the Sidekick's composer, no Questionnaire standing over it: {screen}"
    );
}

/// A Workspace holding nothing but Subsessions the reader hides is not one
/// the Sidebar has work listed in.
#[test]
fn the_selector_offers_no_workspace_on_account_of_hidden_subsessions_alone() {
    let workspace = workspace_dir();
    let root = workspace.path();
    let sidekick = Sidekick::new();
    let sessions = vec![sidekick.own(root), sidekick.began(root)];
    let offered = |hide: bool| {
        let mut application = sidebar_hiding(root, hide, sessions.clone());
        enter_sidebar(&mut application);
        press(&mut application, KeyCode::Enter, KeyModifiers::NONE);
        sidebar_lines(&application)
    };

    let listing = offered(false);
    assert!(
        listing.iter().any(|line| line.ends_with("repository")),
        "{listing:?}"
    );
    let hidden = offered(true);
    assert!(
        hidden.iter().any(|line| line.ends_with("sidekick")),
        "{hidden:?}"
    );
    assert!(
        !hidden.iter().any(|line| line.ends_with("repository")),
        "{hidden:?}"
    );
}

fn remote(name: &str) -> Remote {
    Remote {
        name: name.to_owned(),
        fingerprint: format!("{name}-fingerprint"),
        addresses: Vec::new(),
        status: RemoteStatus::Available,
    }
}

/// Everywhere, each Subsession is judged on its own Server: a Remote's
/// Subsession is hidden by its own Sidekick there, and an identity that names
/// a Sidekick's Session only on another Server names nothing on its own.
#[test]
fn everywhere_a_subsession_is_hidden_by_its_sidekick_on_its_own_server() {
    let workspace = workspace_dir();
    let root = workspace.path();
    let mut application = connected_application(root);
    let ApplicationTransition::ListEverywhereRemotes(discovery) = deliver_settings(
        &mut application,
        EffectiveSettings {
            sidebar: SidebarSettings {
                initial_visibility: SidebarVisibility::Shown,
                initial_scope: SidebarScope::Everywhere,
                ..SidebarSettings::default()
            },
            sidekick: SidekickSettings {
                hide_subsessions: true,
            },
            ..EffectiveSettings::default()
        },
    ) else {
        panic!("an Everywhere Sidebar starts by discovering its paired Remotes");
    };
    let ApplicationTransition::ReconcileCatalogOrigins { requests, .. } = application
        .handle_event(ApplicationEvent::EverywhereRemotesListed {
            request: discovery,
            remotes: vec![remote("studio")],
        })
        .expect("take the paired Remotes")
    else {
        panic!("Everywhere asks every Origin for its Sessions");
    };
    let local = Sidekick::new();
    let studio = Sidekick::new();
    let studio_root = root.join("studio");
    for request in requests {
        let sessions = match request.outlook() {
            Outlook::Local => vec![local.own(root), local.began(root)],
            Outlook::Remote(name) if name == "studio" => vec![
                edit(studio.own(&studio_root), |summary| {
                    summary.title = "Studio Sidekick".to_owned();
                }),
                edit(
                    working(studio.began(&studio_root), minutes_ago(5)),
                    |summary| {
                        summary.title = "Studio Subsession".to_owned();
                    },
                ),
                begun_by(
                    listed(SessionId::new(), "Named across Servers", &studio_root, 4),
                    local.session,
                ),
            ],
            other => panic!("unexpected listing Origin: {other:?}"),
        };
        application
            .handle_event(ApplicationEvent::SessionsListed { request, sessions })
            .expect("take one Origin's listing");
    }

    assert!(!listed_in_sidebar(&application, "Fixing the parser"));
    assert!(
        !listed_in_sidebar(&application, "Studio Subsession"),
        "a Remote's Subsession is hidden too: {:?}",
        sidebar_lines(&application)
    );
    assert!(
        slot_of(&application, "Studio Sidekick").ends_with("Working 5m"),
        "and its own Sidekick's row there carries its Standing: {:?}",
        slot_of(&application, "Studio Sidekick")
    );
    assert!(
        listed_in_sidebar(&application, "Named across Servers"),
        "a Sidekick's Session on another Server leads to nothing on this one"
    );
}

/// The Session picker, with the Sidebar out of the way.
fn picker_client(workspace: &Path, hide_subsessions: bool) -> Application {
    // Homed at the fixture root, so every path the fixed-width picker draws
    // is short on every platform and leaves the Titles whole.
    let mut application = connected_application_homed(workspace);
    let transition = deliver_settings(
        &mut application,
        EffectiveSettings {
            sidebar: SidebarSettings {
                initial_visibility: SidebarVisibility::Hidden,
                ..SidebarSettings::default()
            },
            sidekick: SidekickSettings { hide_subsessions },
            ..EffectiveSettings::default()
        },
    );
    assert_eq!(transition, ApplicationTransition::Continue);
    application
}

fn open_picker(application: &mut Application) -> SessionListRequest {
    let ApplicationTransition::ListSessions(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::SessionList,
        )))
        .expect("open the Session picker")
    else {
        panic!("the Session picker asks for its Sessions");
    };
    request
}

fn widen_picker(application: &mut Application) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL,
        )))
        .expect("widen the Session picker")
}

fn answer(
    application: &mut Application,
    request: SessionListRequest,
    sessions: Vec<SessionListItem>,
) {
    application
        .handle_event(ApplicationEvent::SessionsListed { request, sessions })
        .expect("answer the Session picker");
}

fn picker_screen(application: &Application) -> String {
    rendered_application_rows_at(application, 160, TALL).join("\n")
}

/// The picker's listing: a Sidekick and the Subsession it began, which owes an
/// answer, beside the reader's own work here and elsewhere and a Subsession
/// whose Sidekick is gone.
fn picker_listing(root: &Path, sidekick: &Sidekick) -> Vec<SessionListItem> {
    vec![
        sidekick.own(root),
        owing_an_answer(edit(sidekick.began(root), |summary| {
            summary.session.workspace = suru::protocol::Workspace::directory(root.to_owned());
        })),
        listed(SessionId::new(), "Own work here", root, 1),
        listed(SessionId::new(), "Own work elsewhere", &repository(root), 1),
        begun_by(
            listed(SessionId::new(), "Orphaned here", root, 4),
            SessionId::new(),
        ),
    ]
}

#[test]
fn the_session_picker_leaves_out_hidden_subsessions_at_every_scope() {
    let workspace = workspace_dir();
    let root = workspace.path();
    let sidekick = Sidekick::new();
    let mut application = picker_client(root, true);

    let current = open_picker(&mut application);
    assert_eq!(
        current.scope(),
        &SessionListScope::AllWorkspaces,
        "whether a Subsession here is hidden turns on a Sidekick's Session rooted elsewhere, \
         so the picker asks for the whole Outlook"
    );
    answer(&mut application, current, picker_listing(root, &sidekick));
    let screen = picker_screen(&application);
    assert!(screen.contains("Current Workspace"), "{screen}");
    assert!(
        !screen.contains("Fixing the parser"),
        "the Subsession here is hidden: {screen}"
    );
    assert!(screen.contains("Own work here"), "{screen}");
    assert!(screen.contains("Orphaned here"), "{screen}");
    assert!(
        !screen.contains("Own work elsewhere") && !screen.contains("Sidekick at work"),
        "and the picker narrows to the current Workspace itself: {screen}"
    );

    let ApplicationTransition::ListSessions(all) = widen_picker(&mut application) else {
        panic!("widening asks again");
    };
    answer(&mut application, all, picker_listing(root, &sidekick));
    let screen = picker_screen(&application);
    assert!(screen.contains("All Workspaces"), "{screen}");
    assert!(!screen.contains("Fixing the parser"), "{screen}");
    let sidekicks_row = screen
        .lines()
        .find(|line| line.contains("Sidekick at work"))
        .unwrap_or_else(|| panic!("the Sidekick's row is listed: {screen}"));
    assert!(
        sidekicks_row.contains("1 pending questions") && sidekicks_row.contains("active"),
        "and carries what its hidden Subsession owes: {sidekicks_row}"
    );
    assert!(screen.contains("Own work elsewhere"), "{screen}");

    let ApplicationTransition::ListEverywhereRemotes(discovery) = widen_picker(&mut application)
    else {
        panic!("Everywhere first discovers its Origins");
    };
    let ApplicationTransition::ReconcileCatalogOrigins { mut requests, .. } = application
        .handle_event(ApplicationEvent::EverywhereRemotesListed {
            request: discovery,
            remotes: Vec::new(),
        })
        .expect("discover no Remotes")
    else {
        panic!("Everywhere asks every Origin there is");
    };
    let everywhere = requests.pop().expect("this Server is always one Origin");
    assert!(requests.is_empty(), "and the only one: {requests:?}");
    answer(
        &mut application,
        everywhere,
        picker_listing(root, &sidekick),
    );
    let screen = picker_screen(&application);
    assert!(screen.contains("Everywhere"), "{screen}");
    assert!(!screen.contains("Fixing the parser"), "{screen}");
    assert!(screen.contains("Sidekick at work"), "{screen}");

    type_terminal_text(&mut application, "parser");
    let screen = picker_screen(&application);
    assert!(
        !screen.contains("Fixing the parser") && screen.contains("No Sessions found"),
        "a search never finds a hidden Subsession: {screen}"
    );
}

#[test]
fn the_session_picker_lists_subsessions_until_the_reader_hides_them() {
    let workspace = workspace_dir();
    let root = workspace.path();
    let sidekick = Sidekick::new();
    let mut application = picker_client(root, false);

    let current = open_picker(&mut application);
    assert_eq!(
        current.scope(),
        &SessionListScope::CurrentWorkspace(root.to_owned().into()),
        "with nothing hidden the server narrows to the current Workspace"
    );
    let listing = picker_listing(root, &sidekick)
        .into_iter()
        .filter(|session| {
            session
                .workspace()
                .is_some_and(|rooted| rooted.path == root)
        })
        .collect::<Vec<_>>();
    answer(&mut application, current, listing);
    let screen = picker_screen(&application);
    assert!(screen.contains("Fixing the parser"), "{screen}");

    let ApplicationTransition::ListSessions(again) = deliver_settings(
        &mut application,
        EffectiveSettings {
            sidebar: SidebarSettings {
                initial_visibility: SidebarVisibility::Hidden,
                ..SidebarSettings::default()
            },
            sidekick: SidekickSettings {
                hide_subsessions: true,
            },
            ..EffectiveSettings::default()
        },
    ) else {
        panic!("hiding Subsessions while the picker is open asks for the whole Outlook");
    };
    assert_eq!(again.scope(), &SessionListScope::AllWorkspaces);
    answer(&mut application, again, picker_listing(root, &sidekick));
    let screen = picker_screen(&application);
    assert!(
        !screen.contains("Fixing the parser") && screen.contains("Own work here"),
        "{screen}"
    );
}
