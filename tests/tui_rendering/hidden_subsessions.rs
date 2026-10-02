//! Hiding Subsessions (`sidekick.hideSubsessions`): what the Sidebar, its
//! search, and the Session picker list with the Setting off and on, and what a
//! Sidekick's row carries for the Subsessions it hides.

use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::support::{
    SIDEBAR_WIDE, add_activity, add_request, approval_activity, click_mouse, connected_application,
    connected_application_homed, deliver_settings, drawn_in_sidebar, failed_session_snapshot,
    listed_session, named_workspace_path, navigable_session_snapshot, rendered_application_buffer,
    rendered_application_rows_at, rendered_row, sidebar_column, text_on, text_position,
    type_terminal_text, workspace_dir,
};
use crossterm::event::{
    Event as InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::style::Color;
use suru::{
    managed_client::{ManagedEvent, SessionEvent, SubagentTreeEvent},
    protocol::{
        Activity, ActivityId, ActivityStatus, ApprovalId, ApprovalOutcome, ApprovalSubject,
        AsideSettings, AsideVisibility, Author, Decision, EffectiveSettings, LatestTurnStatus,
        Outlook, PromptId, QuestionnaireId, Remote, RemoteHealth, RemoteStatus, ResolvedWorkspace,
        SessionDeleted, SessionId, SessionListItem, SessionReference, SessionRevision,
        SessionStandingInputsChanged, SessionStatus, SessionTimestamp, SidebarScope,
        SidebarSettings, SidebarVisibility, SidekickSettings, SubagentTreeEntry,
        SubagentTreeRevision, SubagentTreeSession, SubagentTreeSnapshot, SubagentTreeTopLevel,
        TranscriptItem, Turn, TurnId, TurnStatus, UnreadableSessionSummary,
    },
    tui::{
        Application, ApplicationEvent, ApplicationTransition, CommandId, SemanticCommandId,
        SessionListRequest, SessionListScope, SessionListSurface, WorkspaceResolutionSurface,
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

/// Working, and owing an Approval the reader has yet to decide.
fn owing_a_decision(session: SessionListItem) -> SessionListItem {
    edit(working(session, minutes_ago(5)), |summary| {
        summary.standing_inputs.pending_approvals = vec![ApprovalId::new()];
        summary.standing_inputs.pending_approvals_revision = SessionRevision(3);
    })
}

/// Viewed by some Client at `at`.
fn viewed(session: SessionListItem, at: u64) -> SessionListItem {
    edit(session, |summary| {
        summary.standing_inputs.viewed_at = Some(SessionTimestamp(at));
    })
}

/// Rooted at `workspace` rather than where the fixture put it.
fn rooted_at(session: SessionListItem, workspace: &Path) -> SessionListItem {
    edit(session, |summary| {
        summary.session.workspace = suru::protocol::Workspace::directory(workspace.to_owned());
    })
}

/// Titled `title` rather than as the fixture titled it.
fn titled(session: SessionListItem, title: &str) -> SessionListItem {
    edit(session, |summary| summary.title = title.to_owned())
}

/// Made at `created_at`, which is where the active list stands it.
fn made_at(session: SessionListItem, created_at: u64) -> SessionListItem {
    edit(session, |summary| {
        summary.created_at = SessionTimestamp(created_at);
    })
}

/// Last moved at `updated_at`, which is where the picker stands it.
fn moved_at(session: SessionListItem, updated_at: u64) -> SessionListItem {
    edit(session, |summary| {
        summary.updated_at = SessionTimestamp(updated_at);
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
            origin: None,
            unanswered: false,
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

/// A settled Subsession still speaks through its Sidekick's row: hidden, it
/// has no row of its own to say anything on, and an Intervention it owes is
/// never out of sight.
#[test]
fn a_settled_subsession_still_speaks_on_its_sidekicks_row() {
    type Reading = fn(SessionListItem) -> SessionListItem;
    let idle: Reading = |session| session;
    let cases: [(&str, Reading, Reading, &str); 7] = [
        (
            "a Questionnaire",
            idle,
            owing_an_answer,
            "Needs Intervention",
        ),
        ("an Approval", idle, owing_a_decision, "Needs Intervention"),
        (
            "an Approval over the Sidekick's own work",
            |session| working(session, minutes_ago(2)),
            owing_a_decision,
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
                set_aside(subsessions(sidekick.began(root)), minutes_ago(1)),
            ],
        );

        assert!(
            slot_of(&application, "Sidekick at work").ends_with(slot),
            "{case} a settled Subsession owes reads {slot:?}, not {:?}",
            slot_of(&application, "Sidekick at work")
        );
        assert!(
            !listed_in_sidebar(&application, "Fixing the parser")
                && !sidebar_lines(&application)
                    .iter()
                    .any(|line| line.starts_with("Settled")),
            "{case}: hidden, the Subsession stands on no shelf: {:?}",
            sidebar_lines(&application)
        );
    }
}

/// A Sidekick's row stands among the active while a Subsession it hides has
/// something to say — live work, an owed Intervention, or an outcome nobody
/// has Viewed — however its own Session came to settle, the reader's say-so
/// included. The row's place moves; the Session's own settlement does not.
#[test]
fn a_settled_sidekicks_row_stays_active_while_a_subsession_it_hides_has_something_to_say() {
    type Reading = fn(SessionListItem) -> SessionListItem;
    let cases: [(&str, Reading, &str); 6] = [
        (
            "working",
            |session| working(session, minutes_ago(5)),
            "Working 5m",
        ),
        (
            "Monitoring",
            |session| monitoring(session, minutes_ago(7)),
            "Monitoring 7m",
        ),
        ("owing an answer", owing_an_answer, "Needs Intervention"),
        ("owing a Decision", owing_a_decision, "Needs Intervention"),
        (
            "with a failure unseen",
            |session| settled_as(session, TurnStatus::Failed),
            "Failed",
        ),
        (
            "with an outcome unseen",
            |session| settled_as(session, TurnStatus::Completed),
            "Done",
        ),
    ];
    type Settling = fn(SessionListItem) -> SessionListItem;
    let settlings: [(&str, Settling); 2] = [
        ("left alone", |session| last_active(session, days_ago(5))),
        ("set aside by the reader", |session| {
            set_aside(last_active(session, days_ago(5)), days_ago(4))
        }),
    ];
    for (settling, settle) in settlings {
        for (case, subsession, slot) in cases {
            let workspace = workspace_dir();
            let root = workspace.path();
            let sidekick = Sidekick::new();
            // More recent history than the Sidekick's, enough to push it past
            // the settled shelf's opening rows were it standing there.
            let mut sessions = (0..12)
                .map(|index| {
                    set_aside(
                        listed(SessionId::new(), &format!("History {index}"), root, 1),
                        days_ago(1),
                    )
                })
                .collect::<Vec<_>>();
            sessions.extend([settle(sidekick.own(root)), subsession(sidekick.began(root))]);
            let mut application = sidebar_hiding(root, true, sessions);

            assert!(
                !stands_settled(&application, "Sidekick at work"),
                "a Sidekick's row {settling} stands active while its Subsession is {case}: {:?}",
                sidebar_lines(&application)
            );
            assert!(
                slot_of(&application, "Sidekick at work").ends_with(slot),
                "{settling}, {case}: the row reads {slot:?}, not {:?}",
                slot_of(&application, "Sidekick at work")
            );
            assert!(
                menu_on(&mut application, "Sidekick at work").contains("Unsettle"),
                "{settling}, {case}: the Session itself is still settled, so its menu offers \
                 to bring it back"
            );
        }

        let workspace = workspace_dir();
        let root = workspace.path();
        let sidekick = Sidekick::new();
        let quiet = sidebar_hiding(
            root,
            true,
            vec![
                settle(sidekick.own(root)),
                viewed(
                    settled_as(sidekick.began(root), TurnStatus::Completed),
                    now(),
                ),
            ],
        );
        assert!(
            stands_settled(&quiet, "Sidekick at work"),
            "{settling}, with nothing its Subsessions have to say, the row settles: {:?}",
            sidebar_lines(&quiet)
        );
    }
}

/// A Sidekick's Session in its Sidekick's Transcript, with the row that
/// began `subsession` and leads into it.
fn sidekick_with_its_subsession_row(
    sidekick: &Sidekick,
    workspace: &Path,
) -> suru::protocol::SessionSnapshot {
    let mut snapshot = navigable_session_snapshot(sidekick.session, workspace, 1);
    let turn_id = snapshot.turns[0].id;
    let row = ActivityId::new();
    snapshot.activities.push(Activity::Subsession {
        id: row,
        turn_id,
        session_id: sidekick.subsession,
        origin: None,
        title: "Fixing the parser".to_owned(),
        prompt: "Fix the parser".to_owned(),
    });
    snapshot
        .transcript
        .insert(1, TranscriptItem::Activity { activity_id: row });
    snapshot
}

/// Presses the primary pointer button on the first drawn occurrence of
/// `needle`.
fn press_text(application: &mut Application, needle: &str) -> ApplicationTransition {
    let buffer = rendered_application_buffer(application, SIDEBAR_WIDE, TALL);
    let (column, row) = text_position(&buffer, needle);
    click_mouse(
        application,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        },
    )
    .expect("press what is drawn")
}

/// Opens the context menu of the Sidebar row titled `title`, and answers with
/// the frame it stands in.
fn menu_on(application: &mut Application, title: &str) -> String {
    let rows = rendered_application_rows_at(application, SIDEBAR_WIDE, TALL);
    let row = u16::try_from(rendered_row(&rows, title)).expect("the row fits a screen row");
    click_mouse(
        application,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: 4,
            row,
            modifiers: KeyModifiers::NONE,
        },
    )
    .expect("ask for the row's menu");
    rendered_application_rows_at(application, SIDEBAR_WIDE, TALL).join("\n")
}

/// Opening a Sidekick's Session views none of the Subsessions its row
/// carries. Opening the Subsession — here through the row in the Sidekick's
/// Transcript that leads into it — Views it as any Session is Viewed, so its
/// outcome leaves the Sidekick's row at once and for good once the Server's
/// Viewed moment lands.
#[test]
fn opening_a_hidden_subsession_clears_its_outcome_from_its_sidekicks_row() {
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

    application
        .handle_event(ApplicationEvent::SessionAttached(
            sidekick_with_its_subsession_row(&sidekick, &sidekick_workspace(root)),
        ))
        .expect("open the Sidekick's Session");
    assert!(
        slot_of(&application, "Sidekick at work").ends_with("Done"),
        "the Subsession is still unseen: {:?}",
        slot_of(&application, "Sidekick at work")
    );

    assert_eq!(
        press_text(&mut application, "Subsession: Fixing the parser"),
        ApplicationTransition::ViewAndAttachSession(SessionReference::new(
            Outlook::Local,
            sidekick.subsession
        )),
        "the row leads into the Subsession, reporting it Viewed as it opens"
    );
    open_session(&mut application, &repository(root), sidekick.subsession);
    assert!(
        !slot_of(&application, "Sidekick at work").contains("Done"),
        "opened, the Subsession reads as Viewed: {:?}",
        slot_of(&application, "Sidekick at work")
    );

    let SessionListItem::Readable(subsession) = viewed(
        settled_as(sidekick.began(root), TurnStatus::Completed),
        now(),
    ) else {
        unreachable!("the fixture builds a readable Session");
    };
    application
        .handle_event(ApplicationEvent::Managed(
            ManagedEvent::SessionStandingInputsChanged(SessionStandingInputsChanged {
                session_id: sidekick.subsession,
                inputs: subsession.standing_inputs,
            }),
        ))
        .expect("take the Server's Viewed moment");
    open_session(
        &mut application,
        &sidekick_workspace(root),
        sidekick.session,
    );
    assert!(
        !slot_of(&application, "Sidekick at work").contains("Done"),
        "once Viewed, the outcome is gone from the row whichever Session is open: {:?}",
        slot_of(&application, "Sidekick at work")
    );
}

const ARMING: Duration = Duration::from_millis(250);

/// A client whose Intervention panels arm on a clock the test moves by hand,
/// with its Sidebar answered by `sessions` and hiding Subsessions.
fn armed_sidebar(
    workspace: &Path,
    sessions: Vec<SessionListItem>,
) -> (Application, Arc<AtomicU64>) {
    let elapsed = Arc::new(AtomicU64::new(0));
    let observed = Arc::clone(&elapsed);
    let origin = Instant::now();
    let mut application = connected_application(workspace)
        .with_presentation_clock(move || {
            origin + Duration::from_millis(observed.load(Ordering::Relaxed))
        })
        .with_intervention_arming_delay(ARMING);
    let ApplicationTransition::ListSessions(request) =
        deliver_settings(&mut application, hiding(true))
    else {
        panic!("a Sidebar coming into view asks for its Sessions");
    };
    application
        .handle_event(ApplicationEvent::SessionsListed { request, sessions })
        .expect("hydrate the Sidebar");
    (application, elapsed)
}

/// The Subsession's own Session, working a Turn that owes an Approval and a
/// Questionnaire, the Approval first.
fn subsession_owing_both(sidekick: &Sidekick, workspace: &Path) -> suru::protocol::SessionSnapshot {
    let mut snapshot = failed_session_snapshot(
        sidekick.subsession,
        PromptId::new(),
        "Fix the parser",
        workspace,
    );
    let turn_id = TurnId::new();
    let started_at = SessionTimestamp::now();
    snapshot.revision = SessionRevision(2);
    snapshot.session.status = SessionStatus::Active;
    snapshot.session.working_since = Some(started_at);
    snapshot.turns.push(Turn {
        id: turn_id,
        prompt_id: None,
        agent: None,
        status: TurnStatus::Active,
        started_at: Some(started_at),
        settled_at: None,
        last_output_at: None,
        usage: None,
        cost: None,
        cost_basis: None,
        cost_details: None,
        compaction_requested: false,
    });
    let approval = approval_activity(
        turn_id,
        ApprovalSubject::Network {
            host_or_url: "https://parser.example.test".to_owned(),
        },
        None,
        ApprovalOutcome::Pending,
        None,
    );
    let Activity::Approval {
        approval: pending, ..
    } = &approval
    else {
        unreachable!("an Approval Activity carries an Approval")
    };
    snapshot.pending_approvals.push(pending.id);
    snapshot.pending_approvals_revision = snapshot.revision;
    add_activity(&mut snapshot, approval);
    add_request(
        &mut snapshot,
        turn_id,
        "Which grammar should the parser follow?",
    );
    snapshot
}

/// The roll-up is the row's Standing and nothing more: what a hidden
/// Subsession owes is answered in the Subsession. Neither its Approval nor its
/// Questionnaire presents itself in the Sidekick's Session; both present
/// themselves once the reader opens the Subsession that owes them.
#[test]
fn a_hidden_subsessions_interventions_present_only_in_the_subsession() {
    let workspace = workspace_dir();
    let root = workspace.path();
    let sidekick = Sidekick::new();
    let (mut application, clock) = armed_sidebar(
        root,
        vec![
            sidekick.own(root),
            owing_a_decision(owing_an_answer(sidekick.began(root))),
        ],
    );
    let screen = |application: &Application| {
        rendered_application_rows_at(application, SIDEBAR_WIDE, TALL).join("\n")
    };
    open_session(
        &mut application,
        &sidekick_workspace(root),
        sidekick.session,
    );

    assert!(slot_of(&application, "Sidekick at work").ends_with("Needs Intervention"));
    let sidekicks = screen(&application);
    assert!(
        !sidekicks.contains("Approval · Choose Decision")
            && !sidekicks.contains("Which grammar should the parser follow?"),
        "nothing the Subsession owes presents itself in its Sidekick's Session: {sidekicks}"
    );
    type_terminal_text(&mut application, "carry on");
    assert!(
        screen(&application).contains("carry on"),
        "and the keys stay with the Sidekick's composer: {}",
        screen(&application)
    );

    let mut owing = subsession_owing_both(&sidekick, &repository(root));
    application
        .handle_event(ApplicationEvent::SessionAttached(owing.clone()))
        .expect("open the Subsession");
    let inside = screen(&application);
    assert!(
        inside.contains("Approval · Choose Decision") && inside.contains("parser.example.test"),
        "the Subsession's Approval presents itself in its own Session: {inside}"
    );

    clock.fetch_add(
        u64::try_from((ARMING * 2).as_millis()).expect("the advance fits"),
        Ordering::Relaxed,
    );
    press(&mut application, KeyCode::Char('1'), KeyModifiers::NONE);
    owing.revision.0 += 1;
    owing.pending_approvals.clear();
    owing.pending_approvals_revision = owing.revision;
    for activity in &mut owing.activities {
        if let Activity::Approval {
            outcome, decision, ..
        } = activity
        {
            *outcome = ApprovalOutcome::Decided;
            *decision = Some(Decision::Accept);
        }
    }
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(owing)))
        .expect("take the decided Approval");
    assert!(
        screen(&application).contains("Which grammar should the parser follow?"),
        "and its Questionnaire follows: {}",
        screen(&application)
    );

    open_session(
        &mut application,
        &sidekick_workspace(root),
        sidekick.session,
    );
    let back = screen(&application);
    assert!(
        !back.contains("Which grammar should the parser follow?"),
        "back in the Sidekick's Session, the Questionnaire stays with the Subsession: {back}"
    );
}

/// Chooses the selector entry `label`, opening the selector first.
fn choose_workspace(application: &mut Application, label: &str) {
    enter_sidebar(application);
    press(application, KeyCode::Enter, KeyModifiers::NONE);
    let rows = rendered_application_rows_at(application, SIDEBAR_WIDE, TALL);
    let row = rows
        .iter()
        .position(|row| sidebar_column(row) == label)
        .unwrap_or_else(|| panic!("the selector offers no {label:?}: {rows:?}"));
    click_mouse(
        application,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 4,
            row: u16::try_from(row).expect("the entry fits a screen row"),
            modifiers: KeyModifiers::NONE,
        },
    )
    .expect("choose the Workspace");
}

/// The selector offers every Workspace there are Sessions in, a Workspace
/// holding nothing but hidden Subsessions among them; choosing one shows the
/// Sidekick's row that carries them, rather than nothing.
#[test]
fn a_workspace_holding_only_hidden_subsessions_is_offered_and_shows_their_sidekick() {
    let workspace = workspace_dir();
    let root = workspace.path();
    let sidekick = Sidekick::new();
    let mut application = sidebar_hiding(
        root,
        true,
        vec![
            sidekick.own(root),
            working(sidekick.began(root), minutes_ago(5)),
        ],
    );

    choose_workspace(&mut application, "repository");
    assert!(
        listed_in_sidebar(&application, "Sidekick at work"),
        "{:?}",
        sidebar_lines(&application)
    );
    assert!(
        slot_of(&application, "Sidekick at work").ends_with("Working 5m"),
        "{:?}",
        slot_of(&application, "Sidekick at work")
    );
    assert!(!listed_in_sidebar(&application, "Fixing the parser"));
}

/// Narrowed to a Workspace, the Sidebar keeps a Sidekick's row standing for
/// the Subsessions it hides there, though its own Session is rooted
/// elsewhere. There it speaks for those Subsessions alone — neither for its
/// own Session nor for Subsessions rooted elsewhere — answers the highlight
/// for them alone, and opens the Sidekick's Session.
#[test]
fn a_narrowed_sidebar_keeps_the_sidekicks_row_for_the_subsessions_it_hides_there() {
    let workspace = workspace_dir();
    let root = workspace.path();
    let sidekick = Sidekick::new();
    let elsewhere = SessionId::new();
    let mut application = sidebar_hiding(
        root,
        true,
        vec![
            owing_an_answer(sidekick.own(root)),
            settled_as(sidekick.began(root), TurnStatus::Completed),
            begun_by(
                working(
                    listed(elsewhere, "Tidying the docs", &root.join("docs"), 4),
                    minutes_ago(5),
                ),
                sidekick.session,
            ),
            listed(SessionId::new(), "Repository work", &repository(root), 1),
        ],
    );
    assert!(slot_of(&application, "Sidekick at work").ends_with("Needs Intervention"));

    choose_workspace(&mut application, "repository");
    assert!(listed_in_sidebar(&application, "Repository work"));
    assert!(
        !listed_in_sidebar(&application, "Fixing the parser")
            && !listed_in_sidebar(&application, "Tidying the docs"),
        "{:?}",
        sidebar_lines(&application)
    );
    assert!(
        slot_of(&application, "Sidekick at work").ends_with("Done"),
        "the row speaks for the Subsession it hides here alone: {:?}",
        slot_of(&application, "Sidekick at work")
    );

    open_session(&mut application, &repository(root), sidekick.subsession);
    assert_eq!(open_sidebar_text(&application), "Sidekick at work");
    open_session(&mut application, &root.join("docs"), elsewhere);
    assert_eq!(
        open_sidebar_text(&application),
        "",
        "a Subsession hidden elsewhere is not one this Workspace's row answers for"
    );

    let rows = rendered_application_rows_at(&application, SIDEBAR_WIDE, TALL);
    let row = u16::try_from(rendered_row(&rows, "Sidekick at work")).expect("a screen row");
    assert_eq!(
        click_mouse(
            &mut application,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 4,
                row,
                modifiers: KeyModifiers::NONE,
            },
        )
        .expect("press the Sidekick's row"),
        ApplicationTransition::ViewAndAttachSession(SessionReference::new(
            Outlook::Local,
            sidekick.session
        )),
        "the row is the way into the Sidekick's Session, and through it to the Subsession"
    );
}

/// The live-work tick is armed by what a row draws: a Sidekick's row whose
/// Standing hides the work it carries under an owed Intervention draws no
/// duration and wants no tick, while one drawing a hidden Subsession's
/// Working does.
#[test]
fn the_live_work_tick_arms_only_where_a_row_draws_live_work() {
    let workspace = workspace_dir();
    let root = workspace.path();
    let sidekick = Sidekick::new();
    let owing_without_working = edit(sidekick.own(root), |summary| {
        summary.standing_inputs.pending_questionnaires = vec![QuestionnaireId::new()];
        summary.standing_inputs.pending_questionnaires_revision = SessionRevision(3);
    });
    let quiet = sidebar_hiding(
        root,
        true,
        vec![
            owing_without_working,
            working(sidekick.began(root), minutes_ago(5)),
        ],
    );
    assert!(slot_of(&quiet, "Sidekick at work").ends_with("Needs Intervention"));
    assert!(
        !quiet.wants_spinner(),
        "nothing the row draws rises, so nothing wakes the client"
    );

    let live = sidebar_hiding(
        root,
        true,
        vec![
            last_active(sidekick.own(root), days_ago(5)),
            working(sidekick.began(root), minutes_ago(5)),
        ],
    );
    assert!(slot_of(&live, "Sidekick at work").ends_with("Working 5m"));
    assert!(
        live.wants_spinner(),
        "the hidden Subsession's Working rises on its Sidekick's row"
    );
}

/// Hiding the Subsession the keys are on hands them to the Sidekick's row
/// carrying it, and carries the column to that row wherever the reader had
/// scrolled.
#[test]
fn hiding_the_subsession_the_keys_are_on_moves_them_to_its_sidekicks_row_in_view() {
    let workspace = workspace_dir();
    let root = workspace.path();
    let sidekick = Sidekick::new();
    let mut sessions = (0..15)
        .map(|index| {
            listed(
                SessionId::new(),
                &format!("Filler {index}"),
                root,
                50 + index,
            )
        })
        .collect::<Vec<_>>();
    sessions.extend([
        made_at(sidekick.own(root), 100),
        made_at(sidekick.began(root), 1),
    ]);
    let mut application = sidebar_hiding(root, false, sessions);
    enter_sidebar(&mut application);
    press(&mut application, KeyCode::Up, KeyModifiers::NONE);
    assert!(
        focused_sidebar_text(&application).contains("Fixing the parser"),
        "the keys walk back to the foot of the list: {:?}",
        focused_sidebar_text(&application)
    );

    deliver_settings(&mut application, hiding(true));
    assert!(
        focused_sidebar_text(&application).contains("Sidekick at work"),
        "the keys stand on the row carrying the Subsession, in view: {:?}",
        sidebar_lines(&application)
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

/// A Subsession this client's own Server's Sidekick began on a Remote names
/// only the Peer it came from there; the Sidekick's Session here names it
/// among the Sessions it began on Remotes, and so carries it Everywhere, with
/// its Standing, until that Session is gone, when it is listed again. One a
/// Sidekick on any other Server began there stays listed.
#[test]
fn everywhere_a_subsession_begun_on_a_remote_from_here_is_carried_by_its_sidekick_here() {
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
    let begun_here = SessionId::new();
    let studio_root = root.join("studio");
    let on_a_peer = |session: SessionListItem, peer: &str| {
        edit(session, |summary| {
            summary.session.begun_by = Some(Author::PeerSidekick {
                peer: peer.to_owned(),
                fingerprint: "ab12cd34ef56".to_owned(),
            });
        })
    };
    for request in requests {
        let sessions = match request.outlook() {
            Outlook::Local => vec![local.own(root)],
            Outlook::Remote(name) if name == "studio" => vec![
                on_a_peer(
                    working(
                        listed(begun_here, "Begun from here", &studio_root, 3),
                        minutes_ago(5),
                    ),
                    "laptop",
                ),
                on_a_peer(
                    listed(SessionId::new(), "Begun from elsewhere", &studio_root, 4),
                    "desktop",
                ),
            ],
            other => panic!("unexpected listing Origin: {other:?}"),
        };
        application
            .handle_event(ApplicationEvent::SessionsListed { request, sessions })
            .expect("take one Origin's listing");
    }
    assert!(
        listed_in_sidebar(&application, "Begun from here"),
        "listed while nothing here says it began here: {:?}",
        sidebar_lines(&application)
    );

    application
        .handle_event(ApplicationEvent::Managed(
            ManagedEvent::SessionRemoteSubsessionsChanged(
                suru::protocol::SessionRemoteSubsessionsChanged {
                    session_id: local.session,
                    remote_subsessions: vec![suru::protocol::RemoteSession {
                        origin: "studio".to_owned(),
                        session_id: begun_here,
                    }],
                },
            ),
        ))
        .expect("hear the Sidekick began a Session on the Remote");
    assert!(
        !listed_in_sidebar(&application, "Begun from here"),
        "a Remote's Subsession begun from here is hidden Everywhere: {:?}",
        sidebar_lines(&application)
    );
    assert!(
        slot_of(&application, "Sidekick at work").ends_with("Working 5m"),
        "and its Sidekick's row here carries its Standing: {:?}",
        slot_of(&application, "Sidekick at work")
    );
    assert!(
        listed_in_sidebar(&application, "Begun from elsewhere"),
        "a Subsession a Sidekick on another Server began leads to nothing here"
    );

    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::SessionDeleted(
            SessionDeleted {
                session_id: local.session,
            },
        )))
        .expect("hear the Sidekick's Session was deleted");
    assert!(
        listed_in_sidebar(&application, "Begun from here"),
        "once its Sidekick's Session is gone it is listed again: {:?}",
        sidebar_lines(&application)
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
        !screen.contains("Own work elsewhere"),
        "the picker narrows to the current Workspace itself: {screen}"
    );
    assert!(
        picker_row(&screen, "Sidekick at work").contains("1 pending questions"),
        "keeping the Sidekick's row for the Subsession it hides here: {screen}"
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

/// Everywhere, the Session picker leaves out a Subsession this Server's own
/// Sidekick began on a Remote as the Sidebar does, through the same rows,
/// and its search never finds it.
#[test]
fn everywhere_the_session_picker_leaves_out_a_subsession_begun_on_a_remote_from_here() {
    let workspace = workspace_dir();
    let root = workspace.path();
    let sidekick = Sidekick::new();
    let begun_here = SessionId::new();
    let mut application = picker_client(root, true);
    let current = open_picker(&mut application);
    answer(&mut application, current, Vec::new());
    let ApplicationTransition::ListSessions(all) = widen_picker(&mut application) else {
        panic!("widening asks again");
    };
    answer(&mut application, all, Vec::new());
    let ApplicationTransition::ListEverywhereRemotes(discovery) = widen_picker(&mut application)
    else {
        panic!("Everywhere first discovers its Origins");
    };
    let ApplicationTransition::ReconcileCatalogOrigins { requests, .. } = application
        .handle_event(ApplicationEvent::EverywhereRemotesListed {
            request: discovery,
            remotes: vec![remote("studio")],
        })
        .expect("discover the Remote")
    else {
        panic!("Everywhere asks every Origin");
    };
    for request in requests {
        let sessions = match request.outlook() {
            Outlook::Local => vec![edit(sidekick.own(root), |summary| {
                summary.remote_subsessions = vec![suru::protocol::RemoteSession {
                    origin: "studio".to_owned(),
                    session_id: begun_here,
                }];
            })],
            Outlook::Remote(_) => vec![edit(
                owing_an_answer(listed(begun_here, "Begun from here", root, 3)),
                |summary| {
                    summary.session.begun_by = Some(Author::PeerSidekick {
                        peer: "laptop".to_owned(),
                        fingerprint: "ab12cd34ef56".to_owned(),
                    });
                },
            )],
        };
        answer(&mut application, request, sessions);
    }
    let screen = picker_screen(&application);
    assert!(screen.contains("Everywhere"), "{screen}");
    assert!(
        !screen.contains("Begun from here"),
        "the Remote's Subsession begun from here is left out: {screen}"
    );
    assert!(
        picker_row(&screen, "Sidekick at work").contains("1 pending questions"),
        "and its Sidekick's row carries what it owes: {screen}"
    );
    type_terminal_text(&mut application, "Begun");
    let screen = picker_screen(&application);
    assert!(
        !screen.contains("Begun from here") && screen.contains("No Sessions found"),
        "a search never finds it: {screen}"
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

/// Opens the Session picker's selected row, as Enter does.
fn choose_selected(application: &mut Application) -> ApplicationTransition {
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("open the selected row")
}

/// The picker's line for the row titled `title`.
fn picker_row(screen: &str, title: &str) -> String {
    screen
        .lines()
        .find(|line| line.contains(title))
        .unwrap_or_else(|| panic!("the picker lists no {title:?}: {screen}"))
        .to_owned()
}

/// Narrowed to the current Workspace, the picker keeps a Sidekick's row
/// standing for the Subsessions it hides there, though its own Session is
/// rooted elsewhere: speaking for them alone, and opening the Sidekick's
/// Session.
#[test]
fn the_pickers_current_workspace_keeps_the_sidekicks_row_for_the_subsessions_it_hides_there() {
    let workspace = workspace_dir();
    let root = workspace.path();
    let sidekick = Sidekick::new();
    let mut application = picker_client(root, true);
    let request = open_picker(&mut application);
    let mut listing = picker_listing(root, &sidekick);
    listing[0] = owing_a_decision(sidekick.own(root));
    answer(&mut application, request, listing);

    let screen = picker_screen(&application);
    let row = picker_row(&screen, "Sidekick at work");
    assert!(
        row.contains("1 pending questions") && row.contains("active"),
        "the row carries what the Subsession hidden here owes: {row}"
    );
    assert!(
        !row.contains("pending Approvals"),
        "and nothing of its own Session, rooted elsewhere: {row}"
    );

    type_terminal_text(&mut application, "Sidekick");
    assert_eq!(
        choose_selected(&mut application),
        ApplicationTransition::ViewAndAttachSession(SessionReference::new(
            Outlook::Local,
            sidekick.session
        )),
        "the row is the way into the Sidekick's Session"
    );
}

/// The current marker stands on the row answering for the open Session,
/// which for a hidden Subsession is its Sidekick's.
#[test]
fn the_pickers_current_marker_stands_on_the_row_carrying_the_open_subsession() {
    let workspace = workspace_dir();
    let root = workspace.path();
    let sidekick = Sidekick::new();
    let mut application = picker_client(root, true);
    open_session(&mut application, root, sidekick.subsession);
    let request = open_picker(&mut application);
    answer(&mut application, request, picker_listing(root, &sidekick));

    let screen = picker_screen(&application);
    let row = picker_row(&screen, "Sidekick at work");
    assert!(row.contains("current"), "{row}");
    assert!(
        row.contains('›'),
        "and the picker opens on it, as it opens on the current row: {row}"
    );
}

/// Hiding the Subsession the picker has selected moves the selection to the
/// Sidekick's row carrying it, and carries the list to that row wherever the
/// reader had scrolled.
#[test]
fn hiding_the_selected_subsession_moves_the_pickers_selection_to_its_sidekicks_row_in_view() {
    let workspace = workspace_dir();
    let root = workspace.path();
    let sidekick = Sidekick::new();
    let mut application = picker_client(root, false);
    let request = open_picker(&mut application);
    answer(&mut application, request, Vec::new());
    let ApplicationTransition::ListSessions(all) = widen_picker(&mut application) else {
        panic!("widening asks again");
    };
    let mut sessions = (0..30)
        .map(|index| {
            moved_at(
                listed(SessionId::new(), &format!("Filler {index}"), root, 1),
                minutes_ago(index),
            )
        })
        .collect::<Vec<_>>();
    sessions.extend([
        moved_at(sidekick.own(root), minutes_ago(15) - 30_000),
        moved_at(sidekick.began(root), days_ago(3)),
    ]);
    answer(&mut application, all, sessions);
    press(&mut application, KeyCode::Up, KeyModifiers::NONE);
    assert!(
        picker_row(&picker_screen(&application), "Fixing the parser").contains('›'),
        "the selection wraps to the foot of the list"
    );

    assert_eq!(
        deliver_settings(
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
        ),
        ApplicationTransition::Continue,
        "every Workspace is listed already, so nothing is asked again"
    );
    let screen = picker_screen(&application);
    assert!(
        picker_row(&screen, "Sidekick at work").contains('›'),
        "the selection stands on the row carrying the Subsession, in view: {screen}"
    );
    assert_eq!(
        choose_selected(&mut application),
        ApplicationTransition::ViewAndAttachSession(SessionReference::new(
            Outlook::Local,
            sidekick.session
        ))
    );
}

/// Turns the Outlook toward the Remote `studio`, whose own working directory
/// is `workspace`.
fn look_at_studio(application: &mut Application, workspace: &Path) {
    type_terminal_text(application, "/connect");
    press(application, KeyCode::Enter, KeyModifiers::NONE);
    application
        .handle_event(ApplicationEvent::RemotesListed(vec![remote("studio")]))
        .expect("list the paired Remotes");
    application
        .handle_event(ApplicationEvent::RemoteProbed {
            name: "studio".to_owned(),
            result: Ok(RemoteHealth {
                protocol_version: Some(suru::protocol::PROTOCOL_VERSION),
                status: RemoteStatus::Available,
            }),
        })
        .expect("probe the Remote");
    press(application, KeyCode::Down, KeyModifiers::NONE);
    let turned = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("turn toward the Remote");
    assert!(
        matches!(turned, ApplicationTransition::TurnOutlook { .. }),
        "choosing the Remote turns the Outlook: {turned:?}"
    );
    // Turning resolves the Remote's own working directory, numbered among
    // this client's resolutions; only the one it awaits is taken.
    for request_id in 1..=4 {
        application
            .handle_event(ApplicationEvent::WorkspaceResolved {
                outlook: Outlook::Remote("studio".to_owned()),
                surface: WorkspaceResolutionSurface::Outlook,
                request_id,
                result: Ok(ResolvedWorkspace::directory(workspace.to_owned())),
            })
            .expect("take the Remote's working directory");
    }
}

/// What the Remote `studio` lists: its own Sidekick and the Subsession that
/// Sidekick began where the reader looks, and a Subsession whose Sidekick's
/// Session is on another Server, which names nothing on this one. Titled
/// short, because a row tagged with its Remote and its path leaves its Title
/// few columns in the picker.
fn studio_listing(
    studio: &Path,
    sidekick: &Sidekick,
    elsewhere: SessionId,
) -> Vec<SessionListItem> {
    vec![
        titled(sidekick.own(studio), "Aide"),
        titled(
            rooted_at(owing_an_answer(sidekick.began(studio)), studio),
            "Chore",
        ),
        listed(SessionId::new(), "Here", studio, 1),
        listed(SessionId::new(), "Yonder", &repository(studio), 1),
        begun_by(listed(SessionId::new(), "Across", studio, 4), elsewhere),
    ]
}

/// Looking into a Remote, or Everywhere, the picker hides each Subsession by
/// the Sidekick's Session on its own Server, at every scope.
#[test]
fn the_session_picker_hides_a_remotes_subsessions_by_its_own_sidekick() {
    let workspace = workspace_dir();
    let root = workspace.path();
    let studio_root = named_workspace_path("studio");
    let local = Sidekick::new();
    let studio = Sidekick::new();
    let mut application = picker_client(root, true);
    look_at_studio(&mut application, &studio_root);

    let current = open_picker(&mut application);
    assert_eq!(current.outlook(), &Outlook::Remote("studio".to_owned()));
    assert_eq!(current.scope(), &SessionListScope::AllWorkspaces);
    answer(
        &mut application,
        current,
        studio_listing(&studio_root, &studio, local.session),
    );
    let screen = picker_screen(&application);
    assert!(
        !screen.contains("Chore") && !screen.contains("Yonder"),
        "{screen}"
    );
    assert!(
        screen.contains("Here") && screen.contains("Across"),
        "{screen}"
    );
    assert!(
        picker_row(&screen, "Aide").contains("1 pending questions"),
        "{screen}"
    );

    let ApplicationTransition::ListSessions(all) = widen_picker(&mut application) else {
        panic!("widening asks the Remote again");
    };
    assert_eq!(all.outlook(), &Outlook::Remote("studio".to_owned()));
    answer(
        &mut application,
        all,
        studio_listing(&studio_root, &studio, local.session),
    );
    let screen = picker_screen(&application);
    assert!(!screen.contains("Chore"), "{screen}");
    assert!(
        screen.contains("Yonder") && screen.contains("Across"),
        "{screen}"
    );
    assert!(
        picker_row(&screen, "Aide").contains("1 pending questions"),
        "{screen}"
    );

    let ApplicationTransition::ListEverywhereRemotes(discovery) = widen_picker(&mut application)
    else {
        panic!("Everywhere first discovers its Origins");
    };
    let ApplicationTransition::ReconcileCatalogOrigins { requests, .. } = application
        .handle_event(ApplicationEvent::EverywhereRemotesListed {
            request: discovery,
            remotes: vec![remote("studio")],
        })
        .expect("discover the Remote")
    else {
        panic!("Everywhere asks every Origin");
    };
    assert_eq!(requests.len(), 2);
    for request in requests {
        let sessions = match request.outlook() {
            Outlook::Local => vec![local.own(root), local.began(root)],
            Outlook::Remote(_) => studio_listing(&studio_root, &studio, local.session),
        };
        answer(&mut application, request, sessions);
    }
    let screen = picker_screen(&application);
    assert!(
        !screen.contains("Fixing the parser") && !screen.contains("Chore"),
        "each Server's Subsessions are hidden by the Sidekick there: {screen}"
    );
    assert!(
        screen.contains("Across"),
        "and a Sidekick's Session on another Server leads to nothing: {screen}"
    );
    assert!(
        screen.contains("Sidekick at work")
            && screen
                .lines()
                .any(|line| line.contains("[studio]") && line.contains("1 pending questions")),
        "each Server's Sidekick carries what its own hidden Subsession owes: {screen}"
    );
}
