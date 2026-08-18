//! Ratatui view state and terminal lifecycle.

mod markdown;

use std::{
    collections::HashMap,
    future::pending,
    io::{Stdout, stdout},
    path::{Path, PathBuf},
};

use anyhow::{Result, anyhow};
use crossterm::{
    cursor::{Hide, Show},
    event::{
        DisableBracketedPaste, EnableBracketedPaste, Event as InputEvent, EventStream, KeyCode,
        KeyEvent, KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    managed_client::{
        ManagedClient, ManagedEvent, RecoveryStatus, SessionCommandClient, SessionEvent,
        SessionProjection, SessionStreamError, SessionSubscription,
    },
    protocol::{
        ActivityKind, AdmitPromptRequest, CreateSessionRequest, InitialPrompt, MessageRole,
        PromptId, ServerIdentity, SessionId, SessionSnapshot, ShutdownReason, TranscriptItem,
        Workspace,
    },
    theme::Theme,
};

mod composer;

use composer::{ComposerKey, ComposerMemory};

#[derive(Clone, Debug)]
struct SessionInteraction {
    scroll_position: usize,
    follow_latest: bool,
}

impl Default for SessionInteraction {
    fn default() -> Self {
        Self {
            scroll_position: 0,
            follow_latest: true,
        }
    }
}

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
    composers: ComposerMemory,
    session_interactions: HashMap<SessionId, SessionInteraction>,
    composer_focused: bool,
    submission_error: Option<String>,
    session: Option<SessionProjection>,
    session_events_blocked: bool,
    pending_submission: Option<PendingSubmission>,
    failed_submissions: HashMap<PromptId, FailedSubmission>,
}

#[derive(Clone, Debug)]
struct PendingSubmission {
    source: ComposerKey,
    target: SubmissionTarget,
    prompt: InitialPrompt,
}

#[derive(Clone, Debug)]
struct FailedSubmission {
    source: ComposerKey,
    prompt: InitialPrompt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SubmissionTarget {
    CreateSession,
    AdmitPrompt(SessionId),
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
            composers: ComposerMemory::default(),
            session_interactions: HashMap::new(),
            composer_focused: true,
            submission_error: None,
            session: None,
            session_events_blocked: false,
            pending_submission: None,
            failed_submissions: HashMap::new(),
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
                        if let Some(session_id) =
                            self.session.as_ref().map(SessionProjection::session_id)
                        {
                            self.composers.recover_session_to_landing(session_id);
                            self.session_interactions.remove(&session_id);
                        }
                        self.session = None;
                        self.session_events_blocked = true;
                        self.submission_error =
                            Some("Session ended because the shared server was replaced".to_owned());
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
        if self.session_events_blocked {
            return Ok(());
        }
        match event {
            SessionEvent::Snapshot(snapshot) => self.hydrate_session(snapshot),
            SessionEvent::Updated(update) => {
                let Some(session) = self.session.as_mut() else {
                    return Err(anyhow!("Session update arrived before its snapshot"));
                };
                session.apply(update)?;
            }
        }
        self.reconcile_pending_submission();
        self.reconcile_failed_submissions();
        Ok(())
    }

    fn apply_created_session(&mut self, snapshot: SessionSnapshot) -> Result<()> {
        self.session_events_blocked = false;
        self.apply_session(SessionEvent::Snapshot(snapshot))
    }

    fn apply_attached_session(&mut self, snapshot: SessionSnapshot) -> Result<()> {
        self.session_events_blocked = false;
        self.apply_session(SessionEvent::Snapshot(snapshot))
    }

    fn hydrate_session(&mut self, snapshot: SessionSnapshot) {
        self.session_interactions
            .entry(snapshot.session.id)
            .or_default();
        self.submission_error = None;
        self.session = Some(SessionProjection::new(snapshot));
    }

    fn composer_key(&self) -> ComposerKey {
        self.session
            .as_ref()
            .map_or(ComposerKey::Landing, |session| {
                ComposerKey::Session(session.session_id())
            })
    }

    fn reconcile_pending_submission(&mut self) {
        let Some(pending) = self.pending_submission.as_ref() else {
            return;
        };
        let Some(snapshot) = self.session.as_ref().map(SessionProjection::snapshot) else {
            return;
        };
        if !snapshot
            .prompts
            .iter()
            .any(|prompt| prompt.id == pending.prompt.id)
        {
            return;
        }
        let pending = self
            .pending_submission
            .take()
            .expect("pending submission was just observed");
        self.composers.admission_reconciled(
            pending.source,
            ComposerKey::Session(snapshot.session.id),
            &pending.prompt,
        );
        self.submission_error = None;
    }

    fn session_interaction(&self, session_id: SessionId) -> Option<&SessionInteraction> {
        self.session_interactions.get(&session_id)
    }

    fn composer_border_style(&self, theme: &Theme) -> Style {
        if self.composer_focused {
            theme.form_field.border
        } else {
            theme.border.subdued
        }
    }

    fn fail_pending_submission(&mut self, prompt_id: PromptId, error: String) {
        let matches = self
            .pending_submission
            .as_ref()
            .is_some_and(|pending| pending.prompt.id == prompt_id);
        if !matches {
            return;
        }
        let pending = self
            .pending_submission
            .take()
            .expect("matching pending submission exists");
        self.composers
            .admission_failed(pending.source, &pending.prompt);
        self.failed_submissions.insert(
            pending.prompt.id,
            FailedSubmission {
                source: pending.source,
                prompt: pending.prompt,
            },
        );
        self.submission_error = Some(error);
    }

    fn reconcile_failed_submissions(&mut self) {
        let Some(snapshot) = self.session.as_ref().map(SessionProjection::snapshot) else {
            return;
        };
        let destination = ComposerKey::Session(snapshot.session.id);
        let reconciled = self
            .failed_submissions
            .keys()
            .copied()
            .filter(|prompt_id| {
                snapshot
                    .prompts
                    .iter()
                    .any(|prompt| prompt.id == *prompt_id)
            })
            .collect::<Vec<_>>();
        for prompt_id in reconciled {
            let failed = self
                .failed_submissions
                .remove(&prompt_id)
                .expect("failed submission identity was just observed");
            if self
                .composers
                .late_admission_reconciled(failed.source, destination, &failed.prompt)
            {
                self.submission_error = None;
            }
        }
    }

    fn provisional_prompt(&self, session_id: SessionId) -> Option<&InitialPrompt> {
        let pending = self.pending_submission.as_ref()?;
        if pending.target != SubmissionTarget::AdmitPrompt(session_id) {
            return None;
        }
        let authoritative = self.session.as_ref().is_some_and(|session| {
            session
                .snapshot()
                .prompts
                .iter()
                .any(|prompt| prompt.id == pending.prompt.id)
        });
        (!authoritative).then_some(&pending.prompt)
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
    SessionSubscriptionEnded,
    PromptAdmissionSucceeded(PromptId),
    PromptAdmissionFailed { prompt_id: PromptId, error: String },
    SessionAttached(SessionSnapshot),
    SessionCreated(SessionSnapshot),
    SessionOperationFailed(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandId {
    ClearOrExit,
    SubmitSteer,
    InsertNewline,
    DeleteBackward,
    DeleteForward,
    MoveCursorLeft,
    MoveCursorRight,
    HistoryPrevious,
    HistoryNext,
    InsertText(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApplicationTransition {
    Continue,
    Exit,
    SessionEnded,
    CreateSession(CreateSessionRequest),
    AdmitPrompt {
        session_id: SessionId,
        request: AdmitPromptRequest,
    },
    SubscribeSession(SessionId),
}

impl Application {
    pub fn new(workspace: impl AsRef<Path>) -> Self {
        Self {
            state: TuiState::new(workspace),
        }
    }

    pub fn handle_event(&mut self, event: ApplicationEvent) -> Result<ApplicationTransition> {
        match event {
            ApplicationEvent::Command(CommandId::ClearOrExit) => {
                let key = self.state.composer_key();
                if self.state.composers.is_empty(key) {
                    return Ok(ApplicationTransition::Exit);
                }
                self.state.composers.clear(key);
                self.state.submission_error = None;
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::InsertText(text)) => {
                let key = self.state.composer_key();
                self.state.composers.insert(key, &text);
                self.state.submission_error = None;
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::InsertNewline) => {
                let key = self.state.composer_key();
                self.state.composers.insert(key, "\n");
                self.state.submission_error = None;
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::DeleteBackward) => {
                let key = self.state.composer_key();
                self.state.composers.delete_backward(key);
                self.state.submission_error = None;
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::DeleteForward) => {
                let key = self.state.composer_key();
                self.state.composers.delete_forward(key);
                self.state.submission_error = None;
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::MoveCursorLeft) => {
                let key = self.state.composer_key();
                self.state.composers.move_left(key);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::MoveCursorRight) => {
                let key = self.state.composer_key();
                self.state.composers.move_right(key);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::HistoryPrevious) => {
                let key = self.state.composer_key();
                self.state.composers.history_previous(key);
                self.state.submission_error = None;
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::HistoryNext) => {
                let key = self.state.composer_key();
                self.state.composers.history_next(key);
                self.state.submission_error = None;
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::SubmitSteer) => {
                if self.state.pending_submission.is_some() {
                    return Ok(ApplicationTransition::Continue);
                }
                let key = self.state.composer_key();
                if self.state.composers.text(key).trim().is_empty() {
                    self.state.submission_error =
                        Some("Prompt must contain non-whitespace text".to_owned());
                    return Ok(ApplicationTransition::Continue);
                }
                let prompt = self.state.composers.begin_submission(key);
                self.state.failed_submissions.remove(&prompt.id);
                self.state.submission_error = None;
                if let ComposerKey::Session(session_id) = key {
                    self.state.pending_submission = Some(PendingSubmission {
                        source: key,
                        target: SubmissionTarget::AdmitPrompt(session_id),
                        prompt: prompt.clone(),
                    });
                    return Ok(ApplicationTransition::AdmitPrompt {
                        session_id,
                        request: AdmitPromptRequest { prompt },
                    });
                }
                self.state.pending_submission = Some(PendingSubmission {
                    source: key,
                    target: SubmissionTarget::CreateSession,
                    prompt: prompt.clone(),
                });
                Ok(ApplicationTransition::CreateSession(CreateSessionRequest {
                    workspace: Workspace {
                        path: self.state.workspace.clone(),
                    },
                    prompt,
                }))
            }
            ApplicationEvent::Managed(ManagedEvent::Fatal(error)) => Err(anyhow!(error)),
            ApplicationEvent::Managed(event @ ManagedEvent::ServerShutdown(_)) => {
                self.state.apply(event);
                Ok(ApplicationTransition::Exit)
            }
            ApplicationEvent::Managed(event) => {
                let had_session = self.state.session.is_some();
                self.state.apply(event);
                if had_session && self.state.session.is_none() {
                    Ok(ApplicationTransition::SessionEnded)
                } else {
                    Ok(ApplicationTransition::Continue)
                }
            }
            ApplicationEvent::Session(event) => {
                self.state.apply_session(event)?;
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::SessionSubscriptionEnded => Ok(self
                .session_id()
                .map_or(ApplicationTransition::Continue, |session_id| {
                    ApplicationTransition::SubscribeSession(session_id)
                })),
            ApplicationEvent::PromptAdmissionSucceeded(_prompt_id) => {
                self.state.reconcile_pending_submission();
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::PromptAdmissionFailed { prompt_id, error } => {
                self.state.fail_pending_submission(prompt_id, error);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::SessionAttached(snapshot) => {
                self.state.apply_attached_session(snapshot)?;
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::SessionCreated(snapshot) => {
                self.state.apply_created_session(snapshot)?;
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::SessionOperationFailed(error) => {
                self.state.submission_error = Some(error);
                Ok(ApplicationTransition::Continue)
            }
        }
    }

    pub fn render(&self, frame: &mut Frame<'_>) {
        render(frame, &self.state);
    }

    fn session_id(&self) -> Option<SessionId> {
        self.state
            .session
            .as_ref()
            .map(SessionProjection::session_id)
    }
}

pub fn command_for_terminal_event(event: InputEvent) -> Option<CommandId> {
    match event {
        InputEvent::Key(key) if key.kind != KeyEventKind::Press => None,
        InputEvent::Key(key) if binding_for(key).is_some() => {
            binding_for(key).map(|binding| binding.command.into_command())
        }
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BoundCommand {
    ClearOrExit,
    SubmitSteer,
    InsertNewline,
    DeleteBackward,
    DeleteForward,
    MoveCursorLeft,
    MoveCursorRight,
    HistoryPrevious,
    HistoryNext,
}

impl BoundCommand {
    fn into_command(self) -> CommandId {
        match self {
            Self::ClearOrExit => CommandId::ClearOrExit,
            Self::SubmitSteer => CommandId::SubmitSteer,
            Self::InsertNewline => CommandId::InsertNewline,
            Self::DeleteBackward => CommandId::DeleteBackward,
            Self::DeleteForward => CommandId::DeleteForward,
            Self::MoveCursorLeft => CommandId::MoveCursorLeft,
            Self::MoveCursorRight => CommandId::MoveCursorRight,
            Self::HistoryPrevious => CommandId::HistoryPrevious,
            Self::HistoryNext => CommandId::HistoryNext,
        }
    }
}

#[derive(Clone, Debug)]
struct CommandBinding {
    code: KeyCode,
    modifiers: KeyModifiers,
    command: BoundCommand,
    label: &'static str,
}

const COMMAND_BINDINGS: &[CommandBinding] = &[
    CommandBinding {
        code: KeyCode::Enter,
        modifiers: KeyModifiers::NONE,
        command: BoundCommand::SubmitSteer,
        label: "Enter",
    },
    CommandBinding {
        code: KeyCode::Enter,
        modifiers: KeyModifiers::SHIFT,
        command: BoundCommand::InsertNewline,
        label: "Shift+Enter",
    },
    CommandBinding {
        code: KeyCode::Enter,
        modifiers: KeyModifiers::CONTROL,
        command: BoundCommand::InsertNewline,
        label: "Ctrl+Enter",
    },
    CommandBinding {
        code: KeyCode::Char('j'),
        modifiers: KeyModifiers::CONTROL,
        command: BoundCommand::InsertNewline,
        label: "Ctrl+J",
    },
    CommandBinding {
        code: KeyCode::Char('c'),
        modifiers: KeyModifiers::CONTROL,
        command: BoundCommand::ClearOrExit,
        label: "Ctrl+C",
    },
    CommandBinding {
        code: KeyCode::Backspace,
        modifiers: KeyModifiers::NONE,
        command: BoundCommand::DeleteBackward,
        label: "Backspace",
    },
    CommandBinding {
        code: KeyCode::Delete,
        modifiers: KeyModifiers::NONE,
        command: BoundCommand::DeleteForward,
        label: "Delete",
    },
    CommandBinding {
        code: KeyCode::Left,
        modifiers: KeyModifiers::NONE,
        command: BoundCommand::MoveCursorLeft,
        label: "Left",
    },
    CommandBinding {
        code: KeyCode::Right,
        modifiers: KeyModifiers::NONE,
        command: BoundCommand::MoveCursorRight,
        label: "Right",
    },
    CommandBinding {
        code: KeyCode::Up,
        modifiers: KeyModifiers::NONE,
        command: BoundCommand::HistoryPrevious,
        label: "Up",
    },
    CommandBinding {
        code: KeyCode::Down,
        modifiers: KeyModifiers::NONE,
        command: BoundCommand::HistoryNext,
        label: "Down",
    },
];

fn binding_for(key: KeyEvent) -> Option<&'static CommandBinding> {
    COMMAND_BINDINGS
        .iter()
        .find(|binding| binding.code == key.code && binding.modifiers == key.modifiers)
}

fn binding_label(command: BoundCommand) -> &'static str {
    COMMAND_BINDINGS
        .iter()
        .find(|binding| binding.command == command)
        .map_or("", |binding| binding.label)
}

pub fn render(frame: &mut Frame<'_>, state: &TuiState) {
    let theme = Theme::system();
    if let Some(session) = &state.session {
        render_session(frame, state, session.snapshot(), &theme);
    } else {
        render_landing(frame, state, &theme);
    }
}

fn render_landing(frame: &mut Frame<'_>, state: &TuiState, theme: &Theme) {
    let [main, status_area] =
        Layout::vertical([Constraint::Min(7), Constraint::Length(1)]).areas(frame.area());
    let key = ComposerKey::Landing;
    let composer_height = composer_block_height(
        frame.area().height,
        72_u16.min(frame.area().width),
        state.composers.text(key),
    );
    let panel = centered_rect(main, 72, composer_height.saturating_add(3));
    let [brand_area, question_area, error_area, composer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(composer_height),
    ])
    .areas(panel);
    frame.render_widget(
        Paragraph::new("Chidori")
            .alignment(Alignment::Center)
            .style(theme.accent.primary.add_modifier(Modifier::BOLD)),
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
                .style(theme.form_field.invalid),
            error_area,
        );
    }
    render_composer(
        frame,
        composer_area,
        state.composers.text(key),
        state.composers.cursor(key),
        state.composer_border_style(theme),
        theme,
    );

    render_status(frame, state, status_area, theme);
}

fn render_session(
    frame: &mut Frame<'_>,
    state: &TuiState,
    snapshot: &SessionSnapshot,
    theme: &Theme,
) {
    let key = ComposerKey::Session(snapshot.session.id);
    let composer_height = composer_block_height(
        frame.area().height,
        frame.area().width,
        state.composers.text(key),
    );
    let [header_area, transcript_area, composer_area, status_area] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(composer_height),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::styled("Chidori", theme.accent.primary.add_modifier(Modifier::BOLD)),
            Line::styled(
                snapshot
                    .session
                    .workspace
                    .path
                    .to_string_lossy()
                    .into_owned(),
                theme.text.subdued,
            ),
        ])),
        header_area,
    );

    let mut lines = transcript_lines(snapshot, theme, transcript_area.width);
    if let Some(provisional) = state.provisional_prompt(snapshot.session.id) {
        push_user_message(&mut lines, &provisional.text, theme, transcript_area.width);
    }
    let scroll_position = state
        .session_interaction(snapshot.session.id)
        .filter(|interaction| !interaction.follow_latest)
        .map_or(0, |interaction| interaction.scroll_position)
        .min(usize::from(u16::MAX)) as u16;
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(theme.border.subdued),
            )
            .scroll((scroll_position, 0)),
        transcript_area,
    );
    render_composer(
        frame,
        composer_area,
        state.composers.text(key),
        state.composers.cursor(key),
        state.composer_border_style(theme),
        theme,
    );
    render_status(frame, state, status_area, theme);
}

fn render_composer(
    frame: &mut Frame<'_>,
    area: Rect,
    text: &str,
    cursor: usize,
    style: Style,
    theme: &Theme,
) {
    let submit = binding_label(BoundCommand::SubmitSteer);
    let newline = binding_label(BoundCommand::InsertNewline);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Prompt · {submit} submit · {newline} newline "))
        .border_style(style);
    let content_width = area.width.saturating_sub(2).max(1);
    let content_height = area.height.saturating_sub(2).max(1);
    let cursor_row = visual_cursor_row(text, cursor, content_width);
    let scroll = cursor_row.saturating_sub(content_height.saturating_sub(1));
    let paragraph = if text.is_empty() {
        Paragraph::new(Span::styled(
            "Type a Prompt and press Enter",
            theme.form_field.placeholder,
        ))
    } else {
        Paragraph::new(text.to_owned()).style(theme.form_field.text)
    };
    frame.render_widget(paragraph.block(block).scroll((scroll, 0)), area);
}

fn composer_block_height(terminal_height: u16, width: u16, text: &str) -> u16 {
    let content_width = width.saturating_sub(2).max(1);
    let desired = visual_row_count(text, content_width).max(1);
    let cap = (terminal_height / 3).max(1);
    desired.min(cap).saturating_add(2)
}

fn visual_row_count(text: &str, width: u16) -> u16 {
    text.split('\n')
        .map(|line| wrapped_line_rows(line, width))
        .fold(0_u16, u16::saturating_add)
}

fn wrapped_line_rows(line: &str, width: u16) -> u16 {
    let cells = line.chars().fold(0_u16, |total, character| {
        total.saturating_add(UnicodeWidthChar::width(character).unwrap_or(0) as u16)
    });
    cells.max(1).saturating_add(width - 1) / width
}

fn visual_cursor_row(text: &str, cursor: usize, width: u16) -> u16 {
    let prefix = &text[..cursor];
    let mut lines = prefix.split('\n');
    let Some(last) = lines.next_back() else {
        return 0;
    };
    let previous_rows = lines
        .map(|line| wrapped_line_rows(line, width))
        .fold(0_u16, u16::saturating_add);
    let last_cells = last.chars().fold(0_u16, |total, character| {
        total.saturating_add(UnicodeWidthChar::width(character).unwrap_or(0) as u16)
    });
    previous_rows.saturating_add(last_cells / width)
}

fn transcript_lines(
    snapshot: &SessionSnapshot,
    theme: &Theme,
    available_width: u16,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for item in &snapshot.transcript {
        match item {
            TranscriptItem::Message { message_id } => {
                let Some(message) = snapshot
                    .messages
                    .iter()
                    .find(|message| message.id == *message_id)
                else {
                    continue;
                };
                match message.role {
                    MessageRole::User => {
                        push_user_message(&mut lines, &message.content, theme, available_width)
                    }
                    MessageRole::Agent => push_agent_message(&mut lines, &message.content, theme),
                }
            }
            TranscriptItem::Activity { activity_id } => {
                let Some(activity) = snapshot
                    .activities
                    .iter()
                    .find(|activity| activity.id == *activity_id)
                else {
                    continue;
                };
                let (prefix, style) = match activity.kind {
                    ActivityKind::Status => ("  ", theme.text.subdued),
                    ActivityKind::Error => ("  Error: ", theme.feedback.error),
                };
                push_prefixed_lines(&mut lines, prefix, &activity.text, style);
            }
        }
    }
    lines
}

fn push_user_message(
    lines: &mut Vec<Line<'static>>,
    content: &str,
    theme: &Theme,
    available_width: u16,
) {
    let surface = theme.surface.elevated.patch(theme.text.primary);
    let accent = theme.surface.elevated.patch(theme.accent.primary);
    let available_width = usize::from(available_width);
    let content_width = available_width.saturating_sub(2).max(1);
    for content_line in wrapped_content_lines(content, content_width) {
        let padding = available_width.saturating_sub(2 + content_line.width());
        lines.push(Line::from(vec![
            Span::styled("┃ ", accent),
            Span::styled(content_line, surface),
            Span::styled(" ".repeat(padding), surface),
        ]));
    }
    lines.push(Line::default());
}

fn wrapped_content_lines(content: &str, width: usize) -> Vec<String> {
    let mut wrapped = Vec::new();
    for source_line in content.split('\n') {
        if source_line.is_empty() {
            wrapped.push(String::new());
            continue;
        }
        let mut line = String::new();
        let mut line_width = 0;
        for character in source_line.chars() {
            let character_width = character.width().unwrap_or(1);
            if line_width > 0 && line_width + character_width > width {
                wrapped.push(std::mem::take(&mut line));
                line_width = 0;
            }
            line.push(character);
            line_width += character_width;
        }
        wrapped.push(line);
    }
    wrapped
}

fn push_agent_message(lines: &mut Vec<Line<'static>>, content: &str, theme: &Theme) {
    for mut line in markdown::render(content, theme) {
        if !line.spans.is_empty() {
            line.spans.insert(0, Span::styled("  ", theme.text.primary));
        }
        lines.push(line);
    }
    if !content.is_empty() {
        lines.push(Line::default());
    }
}

fn render_status(frame: &mut Frame<'_>, state: &TuiState, area: Rect, theme: &Theme) {
    frame.render_widget(
        Paragraph::new(Line::from(status_text(state)))
            .alignment(Alignment::Center)
            .style(status_style(state, theme)),
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
    let mut session_subscription_task: Option<(SessionId, tokio::task::JoinHandle<()>)> = None;
    let (submission_tx, mut submission_rx) = tokio::sync::mpsc::unbounded_channel();
    let (subscription_tx, mut subscription_rx) = tokio::sync::mpsc::unbounded_channel();

    loop {
        terminal.draw(|frame| application.render(frame))?;
        tokio::select! {
            managed_event = client.next() => {
                match managed_event {
                    Some(event) => {
                        let transition = application
                            .handle_event(ApplicationEvent::Managed(event))?;
                        match transition {
                            ApplicationTransition::Continue => {}
                            ApplicationTransition::SessionEnded => {
                                session_subscription = None;
                            }
                            ApplicationTransition::Exit => {
                                terminal.draw(|frame| application.render(frame))?;
                                return Ok(());
                            }
                            ApplicationTransition::CreateSession(_)
                            | ApplicationTransition::AdmitPrompt { .. }
                            | ApplicationTransition::SubscribeSession(_) => {
                                unreachable!("managed events do not issue Session commands");
                            }
                        }
                        if application.session_id().is_none() {
                            session_subscription = None;
                            if let Some((_, task)) = session_subscription_task.take() {
                                task.abort();
                            }
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
                    Some(Err(error)) if !error.is_recoverable() => return Err(error.into()),
                    Some(Err(_)) | None => {
                        recover_session_subscription(
                            &mut application,
                            &mut session_subscription,
                            &mut session_subscription_task,
                            client.session_commands(),
                            &subscription_tx,
                        )?;
                    }
                }
            }
            connected = subscription_rx.recv() => {
                let Some(connected) = connected else {
                    return Err(anyhow!("Session subscription task channel stopped unexpectedly"));
                };
                if session_subscription_task
                    .as_ref()
                    .is_some_and(|(session_id, _)| *session_id == connected.session_id)
                {
                    session_subscription_task = None;
                }
                if application.session_id() == Some(connected.session_id) {
                    session_subscription = Some(connected.subscription);
                }
            }
            submission = submission_rx.recv() => {
                let Some(submission) = submission else {
                    return Err(anyhow!("Prompt admission task channel stopped unexpectedly"));
                };
                match submission {
                    SubmissionResult::SessionCreated(created) => {
                        let session_id = created.session.id;
                        application.handle_event(ApplicationEvent::SessionCreated(*created))?;
                        if let Some((_, task)) = session_subscription_task.take() {
                            task.abort();
                        }
                        session_subscription_task = Some((
                            session_id,
                            spawn_session_subscription(
                                client.session_commands(),
                                session_id,
                                subscription_tx.clone(),
                            ),
                        ));
                    }
                    SubmissionResult::PromptAdmitted(prompt_id) => {
                        application.handle_event(
                            ApplicationEvent::PromptAdmissionSucceeded(prompt_id),
                        )?;
                    }
                    SubmissionResult::Failed { prompt_id, error } => {
                        application.handle_event(ApplicationEvent::PromptAdmissionFailed {
                            prompt_id,
                            error,
                        })?;
                    }
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
                                ApplicationTransition::SessionEnded => {
                                    session_subscription = None;
                                }
                                ApplicationTransition::Exit => return Ok(()),
                                ApplicationTransition::CreateSession(request) => {
                                    let prompt_id = request.prompt.id;
                                    let commands = client.session_commands();
                                    let results = submission_tx.clone();
                                    tokio::spawn(async move {
                                        let result = match commands.create_session(request).await {
                                            Ok(created) => {
                                                SubmissionResult::SessionCreated(Box::new(created))
                                            }
                                            Err(error) => SubmissionResult::Failed {
                                                prompt_id,
                                                error: error.to_string(),
                                            },
                                        };
                                        let _ = results.send(result);
                                    });
                                }
                                ApplicationTransition::AdmitPrompt { session_id, request } => {
                                    let prompt_id = request.prompt.id;
                                    let commands = client.session_commands();
                                    let results = submission_tx.clone();
                                    tokio::spawn(async move {
                                        let result = match commands.admit_prompt(session_id, request).await {
                                            Ok(prompt) => SubmissionResult::PromptAdmitted(prompt.id),
                                            Err(error) => SubmissionResult::Failed {
                                                prompt_id,
                                                error: error.to_string(),
                                            },
                                        };
                                        let _ = results.send(result);
                                    });
                                }
                                ApplicationTransition::SubscribeSession(_) => {
                                    unreachable!("terminal input cannot end a Session subscription")
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

enum SubmissionResult {
    SessionCreated(Box<SessionSnapshot>),
    PromptAdmitted(PromptId),
    Failed { prompt_id: PromptId, error: String },
}

struct ConnectedSessionSubscription {
    session_id: SessionId,
    subscription: SessionSubscription,
}

fn spawn_session_subscription(
    commands: SessionCommandClient,
    session_id: SessionId,
    connected: tokio::sync::mpsc::UnboundedSender<ConnectedSessionSubscription>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut retry_in = tokio::time::Duration::from_millis(50);
        loop {
            if connected.is_closed() {
                return;
            }
            match commands.subscribe_session(session_id).await {
                Ok(subscription) => {
                    let _ = connected.send(ConnectedSessionSubscription {
                        session_id,
                        subscription,
                    });
                    return;
                }
                Err(_) => {
                    tokio::time::sleep(retry_in).await;
                    retry_in = retry_in
                        .saturating_mul(2)
                        .min(tokio::time::Duration::from_secs(1));
                }
            }
        }
    })
}

fn recover_session_subscription(
    application: &mut Application,
    subscription: &mut Option<SessionSubscription>,
    subscription_task: &mut Option<(SessionId, tokio::task::JoinHandle<()>)>,
    commands: SessionCommandClient,
    connected: &tokio::sync::mpsc::UnboundedSender<ConnectedSessionSubscription>,
) -> Result<()> {
    *subscription = None;
    let transition = application.handle_event(ApplicationEvent::SessionSubscriptionEnded)?;
    if let ApplicationTransition::SubscribeSession(session_id) = transition
        && subscription_task.is_none()
    {
        *subscription_task = Some((
            session_id,
            spawn_session_subscription(commands, session_id, connected.clone()),
        ));
    }
    Ok(())
}

async fn next_session_event(
    subscription: &mut Option<SessionSubscription>,
) -> Option<std::result::Result<SessionEvent, SessionStreamError>> {
    match subscription {
        Some(subscription) => subscription.next().await,
        None => pending().await,
    }
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

fn status_style(state: &TuiState, theme: &Theme) -> Style {
    if state.fatal_error.is_some() {
        theme.feedback.error
    } else if state.identity.is_some() && state.recovery.is_none() && !state.manually_stopped {
        theme.feedback.success
    } else {
        theme.feedback.warning
    }
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut output = stdout();
        if let Err(error) = execute!(output, EnterAlternateScreen, Hide, EnableBracketedPaste) {
            let _ = execute!(output, DisableBracketedPaste, LeaveAlternateScreen, Show);
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        match Terminal::new(CrosstermBackend::new(output)) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                let mut output = stdout();
                let _ = execute!(output, DisableBracketedPaste, LeaveAlternateScreen, Show);
                let _ = disable_raw_mode();
                Err(error.into())
            }
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = execute!(
            self.terminal.backend_mut(),
            DisableBracketedPaste,
            LeaveAlternateScreen,
            Show
        );
        let _ = disable_raw_mode();
    }
}
