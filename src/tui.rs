//! Ratatui view state and terminal lifecycle.

use std::{
    future::pending,
    io::{Stdout, stdout},
    path::{Path, PathBuf},
};

use anyhow::{Result, anyhow};
use crossterm::{
    cursor::{Hide, Show},
    event::{Event as InputEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph},
};

use crate::{
    managed_client::{
        ManagedClient, ManagedEvent, RecoveryStatus, SessionEvent, SessionProjection,
        SessionSubscription,
    },
    protocol::{
        ActivityKind, CreateSessionRequest, InitialPrompt, MessageRole, PromptId, ServerIdentity,
        SessionSnapshot, ShutdownReason, Workspace,
    },
};

#[derive(Clone, Debug)]
pub struct TuiState {
    identity: Option<ServerIdentity>,
    pending_identity: Option<ServerIdentity>,
    counter: Option<u64>,
    recovery: Option<RecoveryStatus>,
    /// Manual stop preserves the last confirmed identity and counter as useful final context.
    manually_stopped: bool,
    fatal_error: Option<String>,
    workspace: PathBuf,
    composer: String,
    submission_error: Option<String>,
    session: Option<SessionProjection>,
}

impl Default for TuiState {
    fn default() -> Self {
        Self::new(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }
}

impl TuiState {
    fn new(workspace: impl AsRef<Path>) -> Self {
        Self {
            identity: None,
            pending_identity: None,
            counter: None,
            recovery: None,
            manually_stopped: false,
            fatal_error: None,
            workspace: workspace.as_ref().to_owned(),
            composer: String::new(),
            submission_error: None,
            session: None,
        }
    }

    pub fn apply(&mut self, event: ManagedEvent) {
        match event {
            ManagedEvent::Connecting => {
                self.identity = None;
                self.pending_identity = None;
                self.counter = None;
                self.recovery = None;
                self.manually_stopped = false;
                self.fatal_error = None;
            }
            ManagedEvent::Connected(health) => {
                self.pending_identity = Some(health.identity);
                self.manually_stopped = false;
                self.fatal_error = None;
            }
            ManagedEvent::Snapshot(snapshot) => {
                let confirms_pending_identity = self
                    .pending_identity
                    .as_ref()
                    .is_some_and(|identity| identity.instance_id == snapshot.instance_id);
                if confirms_pending_identity {
                    let replaced_server = self
                        .identity
                        .as_ref()
                        .is_some_and(|identity| identity.instance_id != snapshot.instance_id);
                    if replaced_server {
                        self.session = None;
                        self.composer.clear();
                        self.submission_error = None;
                    }
                    self.identity = self.pending_identity.take();
                }
                self.counter = Some(snapshot.value);
                self.recovery = None;
            }
            ManagedEvent::CounterUpdated(update) => self.counter = Some(update.value),
            ManagedEvent::Recovering(status) => {
                self.recovery = Some(status);
                self.manually_stopped = false;
                self.fatal_error = None;
            }
            ManagedEvent::ServerShutdown(shutdown) => {
                if shutdown.reason == ShutdownReason::Manual {
                    self.pending_identity = None;
                    self.recovery = None;
                    self.manually_stopped = true;
                    self.fatal_error = None;
                }
            }
            ManagedEvent::Fatal(error) => self.fatal_error = Some(error),
        }
    }

    fn apply_session(&mut self, event: SessionEvent) -> Result<()> {
        match event {
            SessionEvent::Snapshot(snapshot) => {
                self.composer.clear();
                self.submission_error = None;
                self.session = Some(SessionProjection::new(snapshot));
            }
            SessionEvent::Updated(update) => {
                let Some(session) = self.session.as_mut() else {
                    return Err(anyhow!("Session update arrived before its snapshot"));
                };
                session.apply(update)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct Application {
    state: TuiState,
}

#[derive(Debug)]
pub enum ApplicationEvent {
    Command(CommandId),
    Managed(ManagedEvent),
    Session(SessionEvent),
    SessionCreationFailed(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandId {
    Quit,
    SubmitPrompt,
    DeleteBackward,
    InsertText(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApplicationTransition {
    Continue,
    Exit,
    CreateSession(CreateSessionRequest),
}

impl Application {
    pub fn new(workspace: impl AsRef<Path>) -> Self {
        Self {
            state: TuiState::new(workspace),
        }
    }

    pub fn handle_event(&mut self, event: ApplicationEvent) -> Result<ApplicationTransition> {
        match event {
            ApplicationEvent::Command(CommandId::Quit) => Ok(ApplicationTransition::Exit),
            ApplicationEvent::Command(CommandId::InsertText(text)) => {
                if self.state.session.is_none() {
                    self.state.composer.push_str(&text);
                    self.state.submission_error = None;
                }
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::DeleteBackward) => {
                if self.state.session.is_none() {
                    self.state.composer.pop();
                    self.state.submission_error = None;
                }
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::SubmitPrompt) => {
                if self.state.session.is_some() {
                    return Ok(ApplicationTransition::Continue);
                }
                if self.state.composer.trim().is_empty() {
                    self.state.submission_error =
                        Some("Prompt must contain non-whitespace text".to_owned());
                    return Ok(ApplicationTransition::Continue);
                }
                Ok(ApplicationTransition::CreateSession(CreateSessionRequest {
                    workspace: Workspace {
                        path: self.state.workspace.clone(),
                    },
                    prompt: InitialPrompt {
                        id: PromptId::new(),
                        text: self.state.composer.clone(),
                    },
                }))
            }
            ApplicationEvent::Managed(ManagedEvent::Fatal(error)) => Err(anyhow!(error)),
            ApplicationEvent::Managed(event @ ManagedEvent::ServerShutdown(_)) => {
                self.state.apply(event);
                Ok(ApplicationTransition::Exit)
            }
            ApplicationEvent::Managed(event) => {
                self.state.apply(event);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Session(event) => {
                self.state.apply_session(event)?;
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::SessionCreationFailed(error) => {
                self.state.submission_error = Some(error);
                Ok(ApplicationTransition::Continue)
            }
        }
    }

    pub fn render(&self, frame: &mut Frame<'_>) {
        render(frame, &self.state);
    }
}

pub fn command_for_terminal_event(event: InputEvent) -> Option<CommandId> {
    match event {
        InputEvent::Key(key) if key.kind != KeyEventKind::Press => None,
        InputEvent::Key(key) if is_quit(key) => Some(CommandId::Quit),
        InputEvent::Key(key)
            if key.code == KeyCode::Enter
                && !key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
        {
            Some(CommandId::SubmitPrompt)
        }
        InputEvent::Key(key) if key.code == KeyCode::Backspace => Some(CommandId::DeleteBackward),
        InputEvent::Key(key)
            if !key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
        {
            match key.code {
                KeyCode::Char(character) => Some(CommandId::InsertText(character.to_string())),
                _ => None,
            }
        }
        InputEvent::Paste(text) => Some(CommandId::InsertText(text)),
        _ => None,
    }
}

pub fn render(frame: &mut Frame<'_>, state: &TuiState) {
    if let Some(session) = &state.session {
        render_session(frame, state, session.snapshot());
    } else {
        render_landing(frame, state);
    }
}

fn render_landing(frame: &mut Frame<'_>, state: &TuiState) {
    let [main, status_area] =
        Layout::vertical([Constraint::Min(7), Constraint::Length(1)]).areas(frame.area());
    let panel = centered_rect(main, 72, 8);
    let [brand_area, question_area, error_area, composer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(3),
    ])
    .areas(panel);
    frame.render_widget(
        Paragraph::new("Chidori")
            .alignment(Alignment::Center)
            .style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        brand_area,
    );
    frame.render_widget(
        Paragraph::new("What would you like to work on?").alignment(Alignment::Center),
        question_area,
    );
    if let Some(error) = &state.submission_error {
        frame.render_widget(
            Paragraph::new(error.as_str())
                .alignment(Alignment::Center)
                .style(Style::default().fg(Color::Red)),
            error_area,
        );
    }
    let content = if state.composer.is_empty() {
        Span::styled(
            "Type a Prompt and press Enter",
            Style::default().fg(Color::DarkGray),
        )
    } else {
        Span::raw(state.composer.clone())
    };
    frame.render_widget(
        Paragraph::new(Line::from(content)).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Prompt ")
                .border_style(Style::default().fg(Color::Cyan)),
        ),
        composer_area,
    );

    render_status(frame, state, status_area);
}

fn render_session(frame: &mut Frame<'_>, state: &TuiState, snapshot: &SessionSnapshot) {
    let [header_area, transcript_area, status_area] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::styled(
                "Chidori",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::styled(
                snapshot
                    .session
                    .workspace
                    .path
                    .to_string_lossy()
                    .into_owned(),
                Style::default().fg(Color::DarkGray),
            ),
        ])),
        header_area,
    );

    let mut lines = Vec::new();
    for turn in &snapshot.turns {
        for message in snapshot
            .messages
            .iter()
            .filter(|message| message.turn_id == turn.id)
        {
            let (prefix, style) = match message.role {
                MessageRole::User => (
                    "┃ ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                MessageRole::Agent => ("  ", Style::default()),
            };
            push_prefixed_lines(&mut lines, prefix, &message.content, style);
        }
        for activity in snapshot
            .activities
            .iter()
            .filter(|activity| activity.turn_id == turn.id)
        {
            let (prefix, style) = match activity.kind {
                ActivityKind::Status => ("  ", Style::default().fg(Color::DarkGray)),
                ActivityKind::Error => ("  Error: ", Style::default().fg(Color::Red)),
            };
            push_prefixed_lines(&mut lines, prefix, &activity.text, style);
        }
        lines.push(Line::default());
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(Block::default().borders(Borders::TOP)),
        transcript_area,
    );
    render_status(frame, state, status_area);
}

fn render_status(frame: &mut Frame<'_>, state: &TuiState, area: Rect) {
    frame.render_widget(
        Paragraph::new(Line::from(status_text(state)))
            .alignment(Alignment::Center)
            .style(status_style(state)),
        area,
    );
}

fn push_prefixed_lines(lines: &mut Vec<Line<'static>>, prefix: &str, content: &str, style: Style) {
    for (index, line) in content.lines().enumerate() {
        lines.push(Line::styled(
            format!("{}{line}", if index == 0 { prefix } else { "  " }),
            style,
        ));
    }
}

pub async fn run(client: ManagedClient) -> Result<()> {
    let workspace =
        std::env::current_dir().map_err(|error| anyhow!("read current Workspace: {error}"))?;
    let mut session = TerminalSession::enter()?;
    run_loop(&mut session.terminal, client, workspace).await
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut client: ManagedClient,
    workspace: PathBuf,
) -> Result<()> {
    let mut application = Application::new(workspace);
    let mut input = EventStream::new();
    let mut session_subscription: Option<SessionSubscription> = None;

    loop {
        terminal.draw(|frame| application.render(frame))?;
        tokio::select! {
            managed_event = client.next() => {
                match managed_event {
                    Some(event) => {
                        let transition = application
                            .handle_event(ApplicationEvent::Managed(event))?;
                        if transition == ApplicationTransition::Exit {
                            terminal.draw(|frame| application.render(frame))?;
                            return Ok(());
                        }
                    }
                    None => return Err(anyhow!("managed client stopped unexpectedly")),
                }
            }
            session_event = next_session_event(&mut session_subscription) => {
                match session_event {
                    Some(Ok(event)) => {
                        application.handle_event(ApplicationEvent::Session(event))?;
                    }
                    Some(Err(error)) => return Err(anyhow!(error)),
                    None => session_subscription = None,
                }
            }
            input_event = input.next() => {
                match input_event {
                    Some(Ok(event)) => {
                        if let Some(command) = command_for_terminal_event(event) {
                            let transition = application
                                .handle_event(ApplicationEvent::Command(command))?;
                            match transition {
                                ApplicationTransition::Continue => {}
                                ApplicationTransition::Exit => return Ok(()),
                                ApplicationTransition::CreateSession(request) => {
                                    match client.create_session(request).await {
                                        Ok(created) => {
                                            let session_id = created.session.id;
                                            application.handle_event(ApplicationEvent::Session(
                                                SessionEvent::Snapshot(created),
                                            ))?;
                                            match client.subscribe_session(session_id).await {
                                                Ok(subscription) => {
                                                    session_subscription = Some(subscription);
                                                }
                                                Err(error) => {
                                                    application.handle_event(
                                                        ApplicationEvent::SessionCreationFailed(
                                                            error.to_string(),
                                                        ),
                                                    )?;
                                                }
                                            }
                                        }
                                        Err(error) => {
                                            application.handle_event(
                                                ApplicationEvent::SessionCreationFailed(
                                                    error.to_string(),
                                                ),
                                            )?;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Some(Err(error)) => return Err(error.into()),
                    None => return Ok(()),
                }
            }
        }
    }
}

async fn next_session_event(
    subscription: &mut Option<SessionSubscription>,
) -> Option<std::result::Result<SessionEvent, String>> {
    match subscription {
        Some(subscription) => subscription.next().await,
        None => pending().await,
    }
}

fn is_quit(key: KeyEvent) -> bool {
    if key.kind != KeyEventKind::Press {
        return false;
    }
    matches!(key.code, KeyCode::Char('c')) && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn centered_rect(area: Rect, preferred_width: u16, preferred_height: u16) -> Rect {
    let width = preferred_width.min(area.width);
    let height = preferred_height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn status_text(state: &TuiState) -> String {
    if let Some(error) = &state.fatal_error {
        return format!("Connection failed: {error}");
    }
    if state.manually_stopped {
        return state.identity.as_ref().map_or_else(
            || "Shared server stopped intentionally".to_owned(),
            |identity| {
                format!(
                    "Shared server stopped intentionally | {}",
                    server_identity_text(identity)
                )
            },
        );
    }
    if let Some(recovery) = state.recovery {
        let last_server = state.identity.as_ref().map_or_else(
            || "no previous server".to_owned(),
            |identity| format!("last server pid {}", identity.pid),
        );
        return format!(
            "Recovering (attempt {}, retry in {:?}) | {last_server}",
            recovery.attempt, recovery.retry_in
        );
    }
    match &state.identity {
        Some(identity) => format!("Connected | {}", server_identity_text(identity)),
        None => "Connecting to Chidori server...".to_owned(),
    }
}

fn server_identity_text(identity: &ServerIdentity) -> String {
    format!(
        "pid {} | server {}",
        identity.pid,
        &identity.instance_id.to_string()[..8]
    )
}

fn status_style(state: &TuiState) -> Style {
    if state.fatal_error.is_some() {
        Style::default().fg(Color::Red)
    } else if state.identity.is_some() && state.recovery.is_none() && !state.manually_stopped {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::Yellow)
    }
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut output = stdout();
        if let Err(error) = execute!(output, EnterAlternateScreen, Hide) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        match Terminal::new(CrosstermBackend::new(output)) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                let mut output = stdout();
                let _ = execute!(output, LeaveAlternateScreen, Show);
                let _ = disable_raw_mode();
                Err(error.into())
            }
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen, Show);
        let _ = disable_raw_mode();
    }
}
