//! Fixtures and rendering helpers shared by more than one area of the TUI tests.

use crossterm::event::{
    Event as InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::{
    Frame, Terminal,
    backend::TestBackend,
    buffer::{Buffer, Cell},
    layout::Position,
    style::Color,
};
use suru::{
    managed_client::{ManagedEvent, SessionEvent},
    protocol::{
        Activity, ActivityId, AgentSelection, EffectiveSettings, Health, LifecycleState, Message,
        MessageId, MessageRole, MessageStatus, ModelAvailability, ModelDescriptor, ModelId, Prompt,
        PromptDelivery, PromptId, PromptOrder, PromptStatus, ProviderId, ServerIdentity, Session,
        SessionChange, SessionId, SessionRevision, SessionSnapshot, SessionStatus,
        SessionTimestamp, SettingsSnapshot, TranscriptItem, Turn, TurnId, TurnStatus, Workspace,
    },
    tui::{Application, ApplicationEvent, ApplicationTransition, CommandId, TerminalFacts},
};
use uuid::Uuid;

/// A Workspace directory whose [`path`](Self::path) is the canonical reading
/// — the one a launching client holds and the server roots Sessions at — so
/// an assertion comparing it against either means the same on every platform.
/// A raw tempdir path is canonical on Linux only by luck: macOS spells
/// `/var/folders/…` for `/private/var/folders/…`, and Windows resolves the
/// short 8.3 names a temporary directory is often reached through. It reads
/// through [`suru::paths::canonical`] so the fixture cannot answer a spelling
/// the Server would never store.
pub struct WorkspaceDir {
    /// Held only to keep the directory on disk for the fixture's lifetime.
    _directory: tempfile::TempDir,
    canonical: std::path::PathBuf,
}

impl WorkspaceDir {
    pub fn path(&self) -> &std::path::Path {
        &self.canonical
    }
}

/// A Workspace directory for a rendering test, held canonical per
/// [`WorkspaceDir`]. It lives as long as the binding, the way a tempdir does.
pub fn workspace_dir() -> WorkspaceDir {
    let directory = tempfile::tempdir().expect("create Workspace");
    let canonical =
        suru::paths::canonical(directory.path()).expect("canonicalize the Workspace fixture");
    WorkspaceDir {
        _directory: directory,
        canonical,
    }
}

/// A spelling of `workspace` that is not its canonical reading on any
/// platform — `..` survives `Path` comparison where `.` does not — the way a
/// path reached through a symlink, or Windows's own `current_dir`, never
/// matches what [`suru::paths::canonical`] answers.
pub fn noncanonical_spelling(workspace: &WorkspaceDir) -> std::path::PathBuf {
    std::fs::create_dir_all(workspace.path().join("sub")).expect("create the spelling's waypoint");
    workspace.path().join("sub").join("..")
}

pub fn rendered_rows(render: impl FnOnce(&mut Frame<'_>)) -> Vec<String> {
    rendered_rows_at(80, 15, render)
}

fn rendered_rows_at(width: u16, height: u16, render: impl FnOnce(&mut Frame<'_>)) -> Vec<String> {
    let mut terminal =
        Terminal::new(TestBackend::new(width, height)).expect("create test terminal");
    terminal
        .draw(render)
        .expect("render headless TUI application");
    let buffer = terminal.backend().buffer();
    buffer
        .content()
        .chunks(buffer.area.width as usize)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect()
}

pub fn rendered_application_rows(application: &Application) -> Vec<String> {
    rendered_rows(|frame| application.render(frame))
}

pub fn rendered_application_rows_at(
    application: &Application,
    width: u16,
    height: u16,
) -> Vec<String> {
    rendered_rows_at(width, height, |frame| application.render(frame))
}

pub fn rendered_application_buffer(application: &Application, width: u16, height: u16) -> Buffer {
    let mut terminal =
        Terminal::new(TestBackend::new(width, height)).expect("create test terminal");
    terminal
        .draw(|frame| application.render(frame))
        .expect("render headless TUI application");
    terminal.backend().buffer().clone()
}

pub fn rendered_application_cursor_at(
    application: &Application,
    width: u16,
    height: u16,
) -> Position {
    let mut terminal =
        Terminal::new(TestBackend::new(width, height)).expect("create test terminal");
    terminal
        .draw(|frame| application.render(frame))
        .expect("render headless TUI application");
    terminal
        .get_cursor_position()
        .expect("read rendered cursor position")
}

/// The text this frame draws on `background`, within `columns` of each row. A
/// surface draws the row the reader is on highlighted while it holds the keys
/// and dimly once they have gone elsewhere, so the two backgrounds tell where
/// the selection is from where the keys are, and each is read by naming one.
pub fn text_on(
    application: &Application,
    background: Color,
    (width, height): (u16, u16),
    columns: std::ops::Range<u16>,
) -> String {
    let buffer = rendered_application_buffer(application, width, height);
    (0..height)
        .map(|row| {
            columns
                .clone()
                .filter_map(|column| buffer.cell((column, row)))
                .filter(|cell| cell.bg == background)
                .map(Cell::symbol)
                .collect::<String>()
        })
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn buffer_rows(buffer: &Buffer) -> Vec<String> {
    buffer
        .content()
        .chunks(buffer.area.width as usize)
        .map(|row| row.iter().map(Cell::symbol).collect::<String>())
        .collect()
}

pub fn text_position(buffer: &Buffer, needle: &str) -> (u16, u16) {
    for (y, row) in buffer_rows(buffer).into_iter().enumerate() {
        if let Some(byte_offset) = row.find(needle) {
            return (
                row[..byte_offset].chars().count() as u16,
                y.try_into().expect("row fits terminal coordinates"),
            );
        }
    }
    panic!("rendered frame did not contain {needle:?}");
}

pub fn rendered_row(rows: &[String], needle: &str) -> usize {
    rows.iter()
        .position(|row| row.contains(needle))
        .unwrap_or_else(|| panic!("rendered frame did not contain {needle:?}"))
}

pub fn ready_health(instance_id: Uuid, pid: u32) -> Health {
    Health::new(
        ServerIdentity {
            instance_id,
            pid,
            protocol_version: 1,
            build_identity: "suru@test".to_owned(),
        },
        LifecycleState::Ready,
    )
}

pub fn fixture_instance_id() -> Uuid {
    Uuid::parse_str("c2f03bd2-b177-4e73-b33a-1fb4f3a8d002").expect("parse fixture instance ID")
}

pub fn connected_application(workspace: &std::path::Path) -> Application {
    connected_application_with_terminal_facts(workspace, TerminalFacts::default())
}

pub fn connected_application_with_terminal_facts(
    workspace: &std::path::Path,
    terminal_facts: TerminalFacts,
) -> Application {
    let instance_id = fixture_instance_id();
    let mut application = Application::new(workspace, terminal_facts);
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::Connected(
            ready_health(instance_id, 42_424),
        )))
        .expect("connect application");
    application
}

/// Hands the client the effective-settings snapshot the server answers with,
/// which is how every Setting a rendering test leans on comes into force.
pub fn deliver_settings(
    application: &mut Application,
    settings: EffectiveSettings,
) -> ApplicationTransition {
    application
        .handle_event(ApplicationEvent::Managed(ManagedEvent::SettingsSnapshot(
            SettingsSnapshot {
                settings,
                pinned: Vec::new(),
                diagnostics: Vec::new(),
            },
        )))
        .expect("receive the effective-settings snapshot")
}

// The Sidebar as a rendering test reads it: the column's own geometry, what its
// selector says, and the path entry the add-Workspace affordance opens. Every
// test that reads a Sidebar shares these, because the geometry is the frame's
// rather than any one area's.

/// Wide enough for the Sidebar and a main view both.
pub const SIDEBAR_WIDE: u16 = 100;

/// Tall enough that the rows a press lands on are drawn.
pub const SIDEBAR_PRESS_HEIGHT: u16 = 20;

/// The screen row the selector and its add-Workspace affordance share, which
/// is the line under the Sidebar's search box.
pub const SELECTOR_ROW: u16 = 1;

/// What the add-Workspace affordance is drawn as, beside the selector on its
/// own line.
pub const ADD_WORKSPACE: char = '+';

/// The Sidebar's own columns of one rendered row, trimmed of the padding that
/// holds them apart from the main view beside them.
pub fn sidebar_column(row: &str) -> String {
    row.chars()
        .take_while(|character| *character != '\u{2502}')
        .collect::<String>()
        .trim()
        .to_owned()
}

pub fn drawn_in_sidebar(rows: &[String], needle: &str) -> bool {
    rows.iter().any(|row| sidebar_column(row).contains(needle))
}

/// What the selector says, read off the label region of the line it shares
/// with the add-Workspace affordance.
pub fn selector_label(rows: &[String]) -> String {
    sidebar_column(&rows[usize::from(SELECTOR_ROW)])
        .trim_end_matches(ADD_WORKSPACE)
        .trim_end()
        .to_owned()
}

/// Opens the path entry the way a pointer does: a press on the affordance at
/// the right of the selector's own line, read off the frame so it lands where
/// the reader would point.
pub fn press_add_workspace(application: &mut Application) -> ApplicationTransition {
    let rows = rendered_application_rows_at(application, SIDEBAR_WIDE, SIDEBAR_PRESS_HEIGHT);
    let column = rows[usize::from(SELECTOR_ROW)]
        .chars()
        .position(|character| character == ADD_WORKSPACE)
        .expect("the affordance is drawn beside the selector");
    click_mouse(
        application,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: column.try_into().expect("the column fits a screen column"),
            row: SELECTOR_ROW,
            modifiers: KeyModifiers::NONE,
        },
    )
    .expect("press the add-Workspace affordance")
}

/// Names a Workspace the way a reader does: open the path entry, type the
/// path, and offer it.
pub fn add_workspace(application: &mut Application, path: &str) -> ApplicationTransition {
    press_add_workspace(application);
    type_terminal_text(application, path);
    let transition = application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )))
        .expect("offer the path the reader typed");
    answer_workspace_resolution(application, transition)
}

/// Answers a Workspace resolution transition the way the local Server would.
/// Rendering tests stay at the Application seam: they deliver the server's
/// visible answer rather than reaching into picker or Sidebar state.
pub fn answer_workspace_resolution(
    application: &mut Application,
    transition: ApplicationTransition,
) -> ApplicationTransition {
    let ApplicationTransition::ResolveWorkspace {
        outlook,
        surface,
        request_id,
        request,
    } = transition
    else {
        return transition;
    };
    let base = request
        .base
        .unwrap_or_else(|| std::env::current_dir().expect("read test current directory"));
    let named = base.join(request.path);
    let result = suru::paths::canonical(named)
        .map_err(|_| "No directory there".to_owned())
        .and_then(|path| {
            path.is_dir()
                .then_some(suru::protocol::ResolvedWorkspace::directory(path))
                .ok_or_else(|| "Not a directory".to_owned())
        });
    application
        .handle_event(ApplicationEvent::WorkspaceResolved {
            outlook,
            surface,
            request_id,
            result,
        })
        .expect("deliver the Server's Workspace resolution")
}

pub fn type_terminal_text(application: &mut Application, text: &str) {
    for character in text.chars() {
        assert_eq!(
            application
                .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                    KeyCode::Char(character),
                    KeyModifiers::NONE,
                )))
                .expect("type terminal text"),
            ApplicationTransition::Continue
        );
    }
}

pub fn enter_session(
    application: &mut Application,
    workspace: &std::path::Path,
) -> (SessionId, SessionSnapshot) {
    application
        .handle_event(ApplicationEvent::Command(CommandId::InsertText(
            "Initial Prompt".to_owned(),
        )))
        .expect("type initial Prompt");
    let ApplicationTransition::CreateSession(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::SubmitSteer))
        .expect("submit initial Prompt")
    else {
        panic!("landing submission should create a Session");
    };
    let session_id = SessionId::new();
    let snapshot = failed_session_snapshot(
        session_id,
        request.prompt.id,
        &request.prompt.text,
        workspace,
    );
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(
            snapshot.clone(),
        )))
        .expect("enter created Session");
    (session_id, snapshot)
}

pub fn enter_active_session(
    application: &mut Application,
    workspace: &std::path::Path,
) -> (SessionId, SessionSnapshot, TurnId) {
    let (session_id, mut snapshot) = enter_session(application, workspace);
    let prompt_id = PromptId::new();
    let turn_id = TurnId::new();
    let message_id = MessageId::new();
    snapshot.revision = SessionRevision(2);
    snapshot.session.status = SessionStatus::Active;
    let started_at = SessionTimestamp::now();
    snapshot.session.working_since = Some(started_at);
    snapshot.prompts.push(Prompt {
        id: prompt_id,
        text: "Long-running work".to_owned(),
        delivery: PromptDelivery::Steer,
        admission_order: PromptOrder(2),
        status: PromptStatus::Delivered,
        skill_invocations: Vec::new(),
    });
    snapshot.turns.push(Turn {
        id: turn_id,
        prompt_id: Some(prompt_id),
        agent: None,
        status: TurnStatus::Active,
        started_at: Some(started_at),
        settled_at: None,
        usage: None,
        cost: None,
        cost_basis: None,
    });
    snapshot.messages.push(Message {
        id: message_id,
        turn_id,
        role: MessageRole::User,
        status: MessageStatus::Completed,
        content: "Long-running work".to_owned(),
        truncated: false,
        skill_invocations: Vec::new(),
    });
    snapshot
        .transcript
        .push(TranscriptItem::Message { message_id });
    application
        .handle_event(ApplicationEvent::SessionAttached(snapshot.clone()))
        .expect("attach active Session");
    (session_id, snapshot, turn_id)
}

pub fn failed_session_snapshot(
    session_id: SessionId,
    prompt_id: PromptId,
    text: &str,
    workspace: &std::path::Path,
) -> SessionSnapshot {
    let delivered = FailedTurnFixture::new(prompt_id, text, PromptOrder::INITIAL);
    let transcript = delivered.transcript();
    SessionSnapshot {
        title: String::new(),
        emoji: None,
        session: Session {
            checkout: None,
            context_fill: None,
            id: session_id,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.to_owned(),
            },
            workspace: Workspace::directory(workspace.to_owned()),
            agent_selection: None,
            agent_selection_availability: ModelAvailability::Available,
            status: SessionStatus::Idle,
            working_since: None,
            parent: None,
        },
        revision: SessionRevision::INITIAL,
        prompts: vec![delivered.prompt],
        turns: vec![delivered.turn],
        messages: vec![delivered.message],
        activities: vec![delivered.activity],
        transcript,
        subagent_questionnaires: Vec::new(),
        subagent_usage: None,
    }
}

pub fn model_descriptor(
    provider: &str,
    id: &str,
    display_name: &str,
    is_default: bool,
    availability: ModelAvailability,
) -> ModelDescriptor {
    ModelDescriptor {
        provider: ProviderId::new(provider),
        id: ModelId::new(id),
        display_name: display_name.to_owned(),
        description: format!("{display_name} description"),
        is_default,
        availability,
        options: Vec::new(),
    }
}

pub fn selected_session_snapshot(
    session_id: SessionId,
    workspace: &std::path::Path,
    selection: AgentSelection,
) -> SessionSnapshot {
    SessionSnapshot {
        title: String::new(),
        emoji: None,
        session: Session {
            checkout: None,
            context_fill: None,
            id: session_id,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.to_owned(),
            },
            workspace: Workspace::directory(workspace.to_owned()),
            agent_selection: Some(selection),
            agent_selection_availability: ModelAvailability::Available,
            status: SessionStatus::Idle,
            working_since: None,
            parent: None,
        },
        revision: SessionRevision::INITIAL,
        prompts: Vec::new(),
        turns: Vec::new(),
        messages: Vec::new(),
        activities: Vec::new(),
        transcript: Vec::new(),
        subagent_questionnaires: Vec::new(),
        subagent_usage: None,
    }
}

pub fn navigable_session_snapshot(
    session_id: SessionId,
    workspace: &std::path::Path,
    section_count: usize,
) -> SessionSnapshot {
    let mut snapshot = SessionSnapshot {
        title: String::new(),
        emoji: None,
        session: Session {
            checkout: None,
            context_fill: None,
            id: session_id,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.to_owned(),
            },
            workspace: Workspace::directory(workspace.to_owned()),
            agent_selection: None,
            agent_selection_availability: ModelAvailability::Available,
            status: SessionStatus::Idle,
            working_since: None,
            parent: None,
        },
        revision: SessionRevision::INITIAL,
        prompts: Vec::new(),
        turns: Vec::new(),
        messages: Vec::new(),
        activities: Vec::new(),
        transcript: Vec::new(),
        subagent_questionnaires: Vec::new(),
        subagent_usage: None,
    };
    for section in 1..=section_count {
        let prompt_id = PromptId::new();
        let turn_id = TurnId::new();
        let user_message_id = MessageId::new();
        let agent_message_id = MessageId::new();
        snapshot.prompts.push(Prompt {
            id: prompt_id,
            text: format!("Prompt section {section}"),
            delivery: PromptDelivery::Steer,
            admission_order: PromptOrder(section as u64),
            status: PromptStatus::Delivered,
            skill_invocations: Vec::new(),
        });
        snapshot.turns.push(Turn {
            id: turn_id,
            prompt_id: Some(prompt_id),
            agent: None,
            status: TurnStatus::Completed,
            started_at: None,
            settled_at: None,
            usage: None,
            cost: None,
            cost_basis: None,
        });
        snapshot.messages.extend([
            Message {
                id: user_message_id,
                turn_id,
                role: MessageRole::User,
                status: MessageStatus::Completed,
                content: format!("Prompt section {section}"),
                truncated: false,
                skill_invocations: Vec::new(),
            },
            Message {
                id: agent_message_id,
                turn_id,
                role: MessageRole::Agent,
                status: MessageStatus::Completed,
                content: format!(
                    "## Agent section {section}\n\nA multiline Markdown response for section {section}."
                ),
                truncated: false,
                skill_invocations: Vec::new(),
            },
        ]);
        snapshot.transcript.extend([
            TranscriptItem::Message {
                message_id: user_message_id,
            },
            TranscriptItem::Message {
                message_id: agent_message_id,
            },
        ]);
    }
    snapshot
}

pub struct FailedTurnFixture {
    pub prompt: Prompt,
    turn: Turn,
    message: Message,
    activity: Activity,
}

impl FailedTurnFixture {
    pub fn new(prompt_id: PromptId, text: &str, admission_order: PromptOrder) -> Self {
        let turn_id = TurnId::new();
        Self {
            prompt: Prompt {
                id: prompt_id,
                text: text.to_owned(),
                delivery: PromptDelivery::Steer,
                admission_order,
                status: PromptStatus::Delivered,
                skill_invocations: Vec::new(),
            },
            turn: Turn {
                id: turn_id,
                prompt_id: Some(prompt_id),
                agent: None,
                status: TurnStatus::Failed,
                started_at: None,
                settled_at: None,
                usage: None,
                cost: None,
                cost_basis: None,
            },
            message: Message {
                id: MessageId::new(),
                turn_id,
                role: MessageRole::User,
                status: MessageStatus::Completed,
                content: text.to_owned(),
                truncated: false,
                skill_invocations: Vec::new(),
            },
            activity: Activity::Error {
                id: ActivityId::new(),
                turn_id,
                text: "No Agent is selected".to_owned(),
            },
        }
    }

    fn transcript(&self) -> Vec<TranscriptItem> {
        vec![
            TranscriptItem::Message {
                message_id: self.message.id,
            },
            TranscriptItem::Activity {
                activity_id: self.activity.id(),
            },
        ]
    }

    pub fn into_changes(self) -> Vec<SessionChange> {
        vec![
            SessionChange::PromptAdded {
                prompt: self.prompt,
            },
            SessionChange::TurnAdded { turn: self.turn },
            SessionChange::MessageAdded {
                message: self.message,
            },
            SessionChange::ActivityAdded {
                activity: self.activity,
            },
        ]
    }
}

/// Sends a complete click, retaining right-button actions on Down.
pub fn click_mouse(
    application: &mut Application,
    mut mouse: MouseEvent,
) -> anyhow::Result<ApplicationTransition> {
    let transition = application.handle_terminal_event(InputEvent::Mouse(mouse))?;
    if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
        mouse.kind = MouseEventKind::Up(MouseButton::Left);
        application.handle_terminal_event(InputEvent::Mouse(mouse))
    } else {
        Ok(transition)
    }
}
