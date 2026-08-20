//! Ratatui view state and terminal lifecycle.

mod commands;
mod markdown;
mod model_options;
mod model_picker;
mod session_picker;
mod slots;

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    future::pending,
    io::{Stdout, stdout},
    path::{Path, PathBuf},
    pin::Pin,
    time::{Duration, SystemTime, UNIX_EPOCH},
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
    layout::{Alignment, Constraint, Layout, Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    managed_client::{
        ManagedClient, ManagedEvent, RecoveryStatus, SessionCommandClient, SessionEvent,
        SessionProjection, SessionStreamError, SessionSubscription,
    },
    protocol::{
        Activity, AdmitPromptRequest, AgentSelection, AgentSelectionOperationId,
        CreateSessionRequest, FileChange, InitialPrompt, MessageId, MessageRole, ModelAvailability,
        ModelCatalog, ModelDescriptor, PromptDelivery, PromptId, PromptStatus, ServerIdentity,
        SessionChange, SessionId, SessionListItem, SessionSnapshot, SessionStatus,
        SessionTimestamp, ShutdownReason, TranscriptItem, TurnId, TurnStatus,
        UpdateAgentSelectionRequest, Workspace,
    },
    theme::Theme,
};

mod composer;

pub use commands::SemanticCommandId;
use commands::{
    CommandAutocomplete, command_for_direct_semantic_key, command_for_leader_key, descriptor,
};
use composer::{ComposerKey, ComposerMemory};
use model_options::{ModelOptionChoiceRow, ModelOptions, ReasoningCycle, cycle_reasoning_effort};
use model_picker::{ModelPicker, ModelPickerAction, ModelPickerRow};
use session_picker::{SessionPicker, SessionPickerRow};
use slots::{
    HomeFooterSlotContext, PromptContextSlotContext, PromptFooterSlotContext,
    PromptStatusSlotContext, RenderSlots, RenderedSlot, SessionComposerTopSlotContext, SlotText,
    truncate_to_width,
};

const NARROW_TERMINAL_WIDTH: u16 = 44;
const MINIMUM_TERMINAL_WIDTH: u16 = 28;
const MINIMUM_TERMINAL_HEIGHT: u16 = 5;
const LANDING_BRAND_MINIMUM_HEIGHT: u16 = 9;
const SESSION_HEADER_MINIMUM_HEIGHT: u16 = 8;
const RECONNECT_GRACE_PERIOD: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionListScope {
    CurrentWorkspace(PathBuf),
    AllWorkspaces,
}

impl SessionListScope {
    fn workspace_filter(&self) -> Option<&Path> {
        match self {
            Self::CurrentWorkspace(workspace) => Some(workspace),
            Self::AllWorkspaces => None,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::CurrentWorkspace(_) => "Current Workspace",
            Self::AllWorkspaces => "All Workspaces",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionListRequest {
    id: u64,
    scope: SessionListScope,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelListRequest {
    sequence: u64,
}

impl ModelListRequest {
    const fn new(sequence: u64) -> Self {
        Self { sequence }
    }
}

impl SessionListRequest {
    fn new(id: u64, scope: SessionListScope) -> Self {
        Self { id, scope }
    }

    pub fn scope(&self) -> &SessionListScope {
        &self.scope
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResponsiveDetail {
    CoreOnly,
    Secondary,
}

impl ResponsiveDetail {
    fn for_width(width: u16) -> Self {
        if width < NARROW_TERMINAL_WIDTH {
            Self::CoreOnly
        } else {
            Self::Secondary
        }
    }

    fn secondary_only_when(self, visible: bool) -> Self {
        if visible { self } else { Self::CoreOnly }
    }

    fn shows_secondary(self) -> bool {
        self == Self::Secondary
    }
}

#[derive(Clone, Debug)]
struct SessionInteraction {
    follow_latest: Cell<bool>,
    anchor: Cell<Option<TranscriptAnchor>>,
    /// Rendering records the latest terminal geometry so semantic page commands can use it.
    viewport: RefCell<Option<TranscriptViewport>>,
}

impl Default for SessionInteraction {
    fn default() -> Self {
        Self {
            follow_latest: Cell::new(true),
            anchor: Cell::new(None),
            viewport: RefCell::new(None),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct TranscriptAnchor {
    message_id: MessageId,
    screen_row: isize,
}

#[derive(Clone, Copy, Debug)]
struct MessageStart {
    message_id: MessageId,
    row: usize,
}

#[derive(Clone, Debug)]
struct TranscriptViewport {
    height: usize,
    scroll_position: usize,
    maximum_scroll: usize,
    message_starts: Vec<MessageStart>,
}

impl TranscriptViewport {
    fn anchor_at(&self, scroll_position: usize) -> Option<TranscriptAnchor> {
        let message_start = self
            .message_starts
            .iter()
            .rev()
            .find(|start| start.row <= scroll_position)
            .or_else(|| self.message_starts.first())?;
        Some(TranscriptAnchor {
            message_id: message_start.message_id,
            screen_row: (message_start.row as isize).saturating_sub_unsigned(scroll_position),
        })
    }
}

#[derive(Clone, Debug)]
pub struct TuiState {
    identity: Option<ServerIdentity>,
    recovery: Option<RecoveryStatus>,
    /// Manual stop preserves the last confirmed identity as useful final context.
    manually_stopped: bool,
    fatal_error: Option<String>,
    workspace: PathBuf,
    composers: ComposerMemory,
    session_interactions: HashMap<SessionId, SessionInteraction>,
    composer_focused: bool,
    submission_error: Option<String>,
    session: Option<SessionProjection>,
    landing_agent_selection: Option<AgentSelection>,
    confirmed_landing_agent_selection: Option<AgentSelection>,
    pending_landing_agent_selection: Option<AgentSelection>,
    queued_landing_agent_selection: Option<AgentSelection>,
    pending_agent_selection: Option<PendingAgentSelection>,
    /// Newest complete Agent Selection awaiting the in-flight request; rapid
    /// cycles coalesce here so transport stays serialized per Session.
    queued_agent_selection: Option<(SessionId, AgentSelection)>,
    confirmed_agent_selection: Option<(SessionId, AgentSelection)>,
    session_events_blocked: bool,
    reconnect_overlay_visible: bool,
    pending_submission: Option<PendingSubmission>,
    failed_submissions: HashMap<PromptId, FailedSubmission>,
    pending_steers: Vec<PendingSteer>,
    command_mode: CommandMode,
    command_autocomplete: CommandAutocomplete,
    pending_model_options: bool,
    model_options: ModelOptions,
    model_picker: ModelPicker,
    session_picker: SessionPicker,
}

#[derive(Clone, Debug)]
struct PendingSubmission {
    source: ComposerKey,
    target: SubmissionTarget,
    prompt: InitialPrompt,
}

#[derive(Clone, Debug)]
struct PendingAgentSelection {
    session_id: SessionId,
    operation_id: AgentSelectionOperationId,
    selection: AgentSelection,
}

#[derive(Clone, Debug)]
struct FailedSubmission {
    source: ComposerKey,
    target: SubmissionTarget,
    prompt: InitialPrompt,
}

#[derive(Clone, Debug)]
struct PendingSteer {
    session_id: SessionId,
    prompt: InitialPrompt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SubmissionTarget {
    CreateSession,
    AdmitPrompt(SessionId, PromptDelivery),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum CommandMode {
    #[default]
    Composer,
    Leader,
    QueuedPrompts {
        selected: PromptId,
    },
    InterruptConfirmation {
        turn_id: TurnId,
    },
}

impl Default for TuiState {
    fn default() -> Self {
        Self::new(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }
}

impl TuiState {
    fn new(workspace: impl AsRef<Path>) -> Self {
        let workspace = workspace.as_ref().to_owned();
        Self {
            identity: None,
            recovery: None,
            manually_stopped: false,
            fatal_error: None,
            workspace: workspace.clone(),
            composers: ComposerMemory::default(),
            session_interactions: HashMap::new(),
            composer_focused: true,
            submission_error: None,
            session: None,
            landing_agent_selection: None,
            confirmed_landing_agent_selection: None,
            pending_landing_agent_selection: None,
            queued_landing_agent_selection: None,
            pending_agent_selection: None,
            queued_agent_selection: None,
            confirmed_agent_selection: None,
            session_events_blocked: false,
            reconnect_overlay_visible: false,
            pending_submission: None,
            failed_submissions: HashMap::new(),
            pending_steers: Vec::new(),
            command_mode: CommandMode::Composer,
            command_autocomplete: CommandAutocomplete::default(),
            pending_model_options: false,
            model_options: ModelOptions::default(),
            model_picker: ModelPicker::default(),
            session_picker: SessionPicker::new(workspace),
        }
    }

    fn sync_command_autocomplete(&mut self) {
        let key = self.composer_key();
        self.command_autocomplete
            .sync(self.composers.text(key), self.composers.cursor(key));
    }

    fn edit_composer(&mut self, edit: impl FnOnce(&mut ComposerMemory, ComposerKey)) {
        let key = self.composer_key();
        edit(&mut self.composers, key);
        self.submission_error = None;
        self.sync_command_autocomplete();
    }

    fn navigate_composer(&mut self, navigate: impl FnOnce(&mut ComposerMemory, ComposerKey)) {
        let key = self.composer_key();
        navigate(&mut self.composers, key);
        self.sync_command_autocomplete();
    }

    fn paste_into_composer(&mut self, text: &str) {
        let key = self.composer_key();
        self.composers.insert(key, text);
        self.submission_error = None;
        self.command_autocomplete
            .dismiss_for_text(self.composers.text(key));
    }

    pub fn apply(&mut self, event: ManagedEvent) {
        match event {
            ManagedEvent::Connecting => {
                self.identity = None;
                self.recovery = None;
                self.reconnect_overlay_visible = false;
                self.manually_stopped = false;
                self.fatal_error = None;
            }
            ManagedEvent::Connected(health) => {
                let replaced_server = self
                    .identity
                    .as_ref()
                    .is_some_and(|identity| identity.instance_id != health.instance_id);
                if replaced_server {
                    if let Some(session_id) =
                        self.session.as_ref().map(SessionProjection::session_id)
                    {
                        self.composers.recover_session_to_landing(session_id);
                        self.session_interactions.remove(&session_id);
                        self.pending_steers
                            .retain(|steer| steer.session_id != session_id);
                    }
                    self.session = None;
                    self.pending_agent_selection = None;
                    self.queued_agent_selection = None;
                    self.confirmed_agent_selection = None;
                    self.session_events_blocked = true;
                    self.submission_error =
                        Some("Session ended because the shared server was replaced".to_owned());
                    self.sync_command_autocomplete();
                }
                if self.session.is_none() {
                    self.landing_agent_selection = health.landing_agent_selection.clone();
                    self.confirmed_landing_agent_selection = health.landing_agent_selection.clone();
                    self.pending_landing_agent_selection = None;
                    self.queued_landing_agent_selection = None;
                    self.model_picker
                        .refocus(self.landing_agent_selection.as_ref());
                }
                self.identity = Some(health.identity);
                self.recovery = None;
                self.reconnect_overlay_visible = false;
                self.manually_stopped = false;
                self.fatal_error = None;
            }
            ManagedEvent::Recovering(status) => {
                if self.recovery.is_none() {
                    self.reconnect_overlay_visible = false;
                }
                self.recovery = Some(status);
                self.manually_stopped = false;
                self.fatal_error = None;
            }
            ManagedEvent::ServerShutdown(shutdown) => {
                if shutdown.reason == ShutdownReason::Manual {
                    self.recovery = None;
                    self.reconnect_overlay_visible = false;
                    self.manually_stopped = true;
                    self.fatal_error = None;
                }
            }
            ManagedEvent::SessionDeleted(deleted) => {
                self.session_picker.remove(deleted.session_id);
                self.remove_deleted_session(deleted.session_id);
            }
            ManagedEvent::SessionCatalogReconciled(snapshot) => {
                self.session_picker.retain_catalog(&snapshot.session_ids);
                if let Some(session_id) = self.session.as_ref().map(SessionProjection::session_id)
                    && !snapshot.session_ids.contains(&session_id)
                {
                    self.remove_deleted_session(session_id);
                }
            }
            ManagedEvent::Fatal(error) => {
                self.reconnect_overlay_visible = false;
                self.fatal_error = Some(error);
            }
        }
    }

    fn remove_deleted_session(&mut self, deleted_session_id: SessionId) {
        if !self
            .session
            .as_ref()
            .is_some_and(|session| session.session_id() == deleted_session_id)
        {
            return;
        }
        self.composers.discard_session(deleted_session_id);
        self.session_interactions.remove(&deleted_session_id);
        self.pending_steers
            .retain(|steer| steer.session_id != deleted_session_id);
        if self.pending_submission.as_ref().is_some_and(|submission| {
            matches!(
                submission.target,
                SubmissionTarget::AdmitPrompt(session_id, _)
                    if session_id == deleted_session_id
            )
        }) {
            self.pending_submission = None;
        }
        self.failed_submissions.retain(|_, submission| {
            !matches!(
                submission.target,
                SubmissionTarget::AdmitPrompt(session_id, _)
                    if session_id == deleted_session_id
            )
        });
        if self
            .pending_agent_selection
            .as_ref()
            .is_some_and(|pending| pending.session_id == deleted_session_id)
        {
            self.pending_agent_selection = None;
        }
        if self
            .queued_agent_selection
            .as_ref()
            .is_some_and(|(session_id, _)| *session_id == deleted_session_id)
        {
            self.queued_agent_selection = None;
        }
        if self
            .confirmed_agent_selection
            .as_ref()
            .is_some_and(|(session_id, _)| *session_id == deleted_session_id)
        {
            self.confirmed_agent_selection = None;
        }
        self.session = None;
        self.session_events_blocked = true;
        self.command_mode = CommandMode::Composer;
        self.submission_error = Some("Session ended because it was deleted".to_owned());
        self.model_picker
            .refocus(self.landing_agent_selection.as_ref());
        self.sync_command_autocomplete();
    }

    fn apply_session(&mut self, event: SessionEvent) -> Result<()> {
        if self.session_events_blocked {
            return Ok(());
        }
        let selection_changed = match &event {
            SessionEvent::Snapshot(_) => true,
            SessionEvent::Updated(update) => update
                .changes
                .iter()
                .any(|change| matches!(change, SessionChange::AgentSelectionChanged { .. })),
        };
        match event {
            SessionEvent::Snapshot(snapshot) => self.hydrate_session(snapshot),
            SessionEvent::Updated(update) => {
                let Some(session) = self.session.as_mut() else {
                    return Err(anyhow!("Session update arrived before its snapshot"));
                };
                session.apply(update)?;
            }
        }
        if selection_changed {
            self.confirmed_agent_selection = None;
            let current = self.agent_selection().cloned();
            self.model_picker.refocus(current.as_ref());
        }
        self.reconcile_pending_submission();
        self.reconcile_failed_submissions();
        self.reconcile_pending_steers();
        self.reconcile_command_mode();
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
        self.sync_command_autocomplete();
    }

    fn composer_key(&self) -> ComposerKey {
        self.session
            .as_ref()
            .map_or(ComposerKey::Landing, |session| {
                ComposerKey::Session(session.session_id())
            })
    }

    fn agent_selection(&self) -> Option<&AgentSelection> {
        let Some(session) = self.session.as_ref() else {
            return self.landing_agent_selection.as_ref();
        };
        let session_id = session.session_id();
        self.queued_agent_selection
            .as_ref()
            .filter(|(queued_session, _)| *queued_session == session_id)
            .map(|(_, selection)| selection)
            .or_else(|| {
                self.pending_agent_selection
                    .as_ref()
                    .filter(|pending| pending.session_id == session_id)
                    .map(|pending| &pending.selection)
            })
            .or_else(|| {
                self.confirmed_agent_selection
                    .as_ref()
                    .filter(|(confirmed_session, _)| *confirmed_session == session_id)
                    .map(|(_, selection)| selection)
            })
            .or(session.snapshot().session.agent_selection.as_ref())
    }

    fn selection_update_pending(&self) -> bool {
        self.pending_agent_selection.is_some() || self.queued_agent_selection.is_some()
    }

    fn reconcile_pending_submission(&mut self) {
        let Some(pending) = self.pending_submission.as_ref() else {
            return;
        };
        let Some(snapshot) = self.session.as_ref().map(SessionProjection::snapshot) else {
            return;
        };
        let Some(authoritative) = snapshot
            .prompts
            .iter()
            .find(|prompt| prompt.id == pending.prompt.id)
            .cloned()
        else {
            return;
        };
        let destination = ComposerKey::Session(snapshot.session.id);
        let pending = self
            .pending_submission
            .take()
            .expect("pending submission was just observed");
        if let SubmissionTarget::AdmitPrompt(session_id, PromptDelivery::Steer) = pending.target
            && authoritative.status == PromptStatus::Pending
        {
            self.track_pending_steer(session_id, pending.prompt.clone());
        }
        self.composers
            .admission_reconciled(pending.source, destination, &pending.prompt);
        self.submission_error = None;
    }

    fn acknowledge_pending_submission(&mut self, prompt_id: PromptId) {
        let detached = self.pending_submission.as_ref().is_some_and(|pending| {
            pending.prompt.id == prompt_id
                && matches!(
                    pending.target,
                    SubmissionTarget::AdmitPrompt(session_id, _)
                        if self.session.as_ref().map(SessionProjection::session_id)
                            != Some(session_id)
                )
        });
        if !detached {
            self.reconcile_pending_submission();
            return;
        }
        let pending = self
            .pending_submission
            .take()
            .expect("detached pending submission was just observed");
        self.composers
            .admission_reconciled(pending.source, pending.source, &pending.prompt);
        self.submission_error = None;
    }

    fn session_interaction(&self, session_id: SessionId) -> Option<&SessionInteraction> {
        self.session_interactions.get(&session_id)
    }

    fn navigate_transcript_page(&mut self, direction: TranscriptDirection) {
        let Some(session_id) = self.session.as_ref().map(SessionProjection::session_id) else {
            return;
        };
        let Some(interaction) = self.session_interactions.get_mut(&session_id) else {
            return;
        };
        let Some(viewport) = interaction.viewport.get_mut().as_ref() else {
            return;
        };
        let target = match direction {
            TranscriptDirection::Up => viewport
                .scroll_position
                .saturating_sub(viewport.height.max(1)),
            TranscriptDirection::Down => viewport
                .scroll_position
                .saturating_add(viewport.height.max(1))
                .min(viewport.maximum_scroll),
        };
        if target >= viewport.maximum_scroll {
            interaction.follow_latest.set(true);
            interaction.anchor.set(None);
            return;
        }
        interaction.follow_latest.set(false);
        interaction.anchor.set(viewport.anchor_at(target));
    }

    fn follow_latest(&mut self) {
        let Some(session_id) = self.session.as_ref().map(SessionProjection::session_id) else {
            return;
        };
        let interaction = self.session_interactions.entry(session_id).or_default();
        interaction.follow_latest.set(true);
        interaction.anchor.set(None);
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
                target: pending.target,
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
            .filter_map(|prompt_id| {
                snapshot
                    .prompts
                    .iter()
                    .find(|prompt| prompt.id == prompt_id)
                    .cloned()
            })
            .collect::<Vec<_>>();
        for authoritative in reconciled {
            let failed = self
                .failed_submissions
                .remove(&authoritative.id)
                .expect("failed submission identity was just observed");
            if let SubmissionTarget::AdmitPrompt(session_id, PromptDelivery::Steer) = failed.target
                && authoritative.status == PromptStatus::Pending
            {
                self.track_pending_steer(session_id, failed.prompt.clone());
            }
            if self
                .composers
                .late_admission_reconciled(failed.source, destination, &failed.prompt)
            {
                self.submission_error = None;
            }
        }
    }

    fn provisional_prompts(&self, session_id: SessionId) -> Vec<&InitialPrompt> {
        let mut prompts = self
            .pending_steers
            .iter()
            .filter(|steer| steer.session_id == session_id)
            .map(|steer| &steer.prompt)
            .collect::<Vec<_>>();
        if let Some(pending) = self.pending_submission.as_ref().filter(|pending| {
            pending.target == SubmissionTarget::AdmitPrompt(session_id, PromptDelivery::Steer)
        }) {
            prompts.push(&pending.prompt);
        }
        prompts
    }

    fn track_pending_steer(&mut self, session_id: SessionId, prompt: InitialPrompt) {
        if !self
            .pending_steers
            .iter()
            .any(|pending| pending.prompt.id == prompt.id)
        {
            self.pending_steers
                .push(PendingSteer { session_id, prompt });
        }
    }

    fn reconcile_pending_steers(&mut self) {
        let Some(snapshot) = self.session.as_ref().map(SessionProjection::snapshot) else {
            return;
        };
        self.pending_steers.retain(|pending| {
            pending.session_id != snapshot.session.id
                || snapshot.prompts.iter().any(|prompt| {
                    prompt.id == pending.prompt.id && prompt.status == PromptStatus::Pending
                })
        });
    }

    fn queued_prompts(&self, session_id: SessionId) -> Vec<QueuedPrompt<'_>> {
        let mut queued = self
            .session
            .as_ref()
            .filter(|session| session.session_id() == session_id)
            .map(|session| {
                let mut prompts = session
                    .snapshot()
                    .prompts
                    .iter()
                    .filter(|prompt| {
                        prompt.status == PromptStatus::Pending
                            && prompt.delivery == PromptDelivery::Queue
                    })
                    .collect::<Vec<_>>();
                prompts.sort_unstable_by_key(|prompt| prompt.admission_order);
                prompts
                    .into_iter()
                    .map(|prompt| QueuedPrompt {
                        id: prompt.id,
                        text: &prompt.text,
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if let Some(pending) = self.pending_submission.as_ref().filter(|pending| {
            pending.target == SubmissionTarget::AdmitPrompt(session_id, PromptDelivery::Queue)
                && !queued.iter().any(|entry| entry.id == pending.prompt.id)
        }) {
            queued.push(QueuedPrompt {
                id: pending.prompt.id,
                text: &pending.prompt.text,
            });
        }
        queued
    }

    fn active_turn_id(&self) -> Option<TurnId> {
        self.session
            .as_ref()?
            .snapshot()
            .turns
            .iter()
            .find(|turn| turn.status == TurnStatus::Active)
            .map(|turn| turn.id)
    }

    fn reconcile_command_mode(&mut self) {
        match self.command_mode {
            CommandMode::QueuedPrompts { selected } => {
                let Some(session_id) = self.session.as_ref().map(SessionProjection::session_id)
                else {
                    self.command_mode = CommandMode::Composer;
                    return;
                };
                let queued = self.queued_prompts(session_id);
                if !queued.iter().any(|prompt| prompt.id == selected) {
                    self.command_mode = queued.first().map_or(CommandMode::Composer, |prompt| {
                        CommandMode::QueuedPrompts {
                            selected: prompt.id,
                        }
                    });
                }
            }
            CommandMode::InterruptConfirmation { turn_id }
                if self.active_turn_id() != Some(turn_id) =>
            {
                self.command_mode = CommandMode::Composer;
            }
            _ => {}
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum TranscriptDirection {
    Up,
    Down,
}

#[derive(Clone, Copy)]
struct QueuedPrompt<'a> {
    id: PromptId,
    text: &'a str,
}

pub struct Application {
    state: TuiState,
    slots: RenderSlots,
}

impl std::fmt::Debug for Application {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Application")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl Default for Application {
    fn default() -> Self {
        Self {
            state: TuiState::default(),
            slots: RenderSlots::builtins(),
        }
    }
}

#[derive(Debug)]
pub enum ApplicationEvent {
    Command(CommandId),
    ReconnectGraceElapsed,
    Managed(ManagedEvent),
    Session(SessionEvent),
    SessionSubscriptionEnded,
    PromptAdmissionSucceeded(PromptId),
    PromptAdmissionFailed {
        prompt_id: PromptId,
        error: String,
    },
    SessionAttached(SessionSnapshot),
    SessionsListed {
        request: SessionListRequest,
        sessions: Vec<SessionListItem>,
    },
    SessionListingFailed {
        request: SessionListRequest,
        error: String,
    },
    ModelsListed {
        request: ModelListRequest,
        catalog: ModelCatalog,
    },
    ModelsRefreshed {
        request: ModelListRequest,
        catalog: ModelCatalog,
    },
    ModelListingFailed {
        request: ModelListRequest,
        error: String,
    },
    LandingAgentSelectionConfirmed(AgentSelection),
    LandingAgentSelectionConfirmationFailed(String),
    AgentSelectionUpdated {
        operation_id: AgentSelectionOperationId,
        selection: AgentSelection,
    },
    AgentSelectionUpdateFailed {
        operation_id: AgentSelectionOperationId,
        error: String,
    },
    SessionAttachmentFailed(String),
    SessionDeletionFailed {
        session_id: SessionId,
        error: String,
    },
    SessionCreated(SessionSnapshot),
    SessionOperationFailed(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandId {
    ClearOrExit,
    SubmitSteer,
    SubmitQueue,
    InsertNewline,
    DeleteBackward,
    DeleteForward,
    MoveCursorLeft,
    MoveCursorRight,
    HistoryPrevious,
    HistoryNext,
    ScrollTranscriptPageUp,
    ScrollTranscriptPageDown,
    FollowLatest,
    BeginLeader,
    OpenQueuedPrompts,
    SelectPreviousQueuedPrompt,
    SelectNextQueuedPrompt,
    PromoteSelectedPrompt,
    CancelSelectedPrompt,
    RequestInterrupt,
    ConfirmInterrupt,
    CloseCommandMode,
    SelectPreviousAutocomplete,
    SelectNextAutocomplete,
    DismissAutocomplete,
    SelectAutocomplete,
    InsertSessionSearch(String),
    DeleteSessionSearchBackward,
    SelectPreviousSession,
    SelectNextSession,
    PagePreviousSessions,
    PageNextSessions,
    ToggleSessionScope,
    SelectSession,
    CloseSessionPicker,
    InsertModelSearch(String),
    DeleteModelSearchBackward,
    SelectPreviousModel,
    SelectNextModel,
    PagePreviousModels,
    PageNextModels,
    SelectModel,
    CloseModelPicker,
    InvokeSemantic(SemanticCommandId),
    InsertText(String),
    PasteText(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApplicationTransition {
    Continue,
    Exit,
    SessionEnded,
    DetachSession,
    DeleteSession(SessionId),
    CreateSession(CreateSessionRequest),
    AdmitPrompt {
        session_id: SessionId,
        request: AdmitPromptRequest,
    },
    PromotePrompt {
        session_id: SessionId,
        prompt_id: PromptId,
    },
    CancelPrompt {
        session_id: SessionId,
        prompt_id: PromptId,
    },
    InterruptTurn {
        session_id: SessionId,
        turn_id: TurnId,
    },
    SubscribeSession(SessionId),
    AttachSession(SessionId),
    ListSessions(SessionListRequest),
    ListModels(ModelListRequest),
    ConfirmLandingAgentSelection(AgentSelection),
    UpdateAgentSelection {
        session_id: SessionId,
        request: UpdateAgentSelectionRequest,
    },
}

impl Application {
    pub fn new(workspace: impl AsRef<Path>) -> Self {
        Self {
            state: TuiState::new(workspace),
            slots: RenderSlots::builtins(),
        }
    }

    pub fn handle_event(&mut self, event: ApplicationEvent) -> Result<ApplicationTransition> {
        match event {
            ApplicationEvent::Command(_) if self.state.reconnect_overlay_visible => {
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(
                CommandId::SubmitSteer
                | CommandId::SubmitQueue
                | CommandId::SelectSession
                | CommandId::SelectModel
                | CommandId::InvokeSemantic(SemanticCommandId::SessionList)
                | CommandId::InvokeSemantic(SemanticCommandId::SessionNew)
                | CommandId::InvokeSemantic(SemanticCommandId::ModelOptionsApply),
            ) if self.state.selection_update_pending() => Ok(ApplicationTransition::Continue),
            ApplicationEvent::Command(CommandId::ClearOrExit) => {
                let key = self.state.composer_key();
                if self.state.composers.is_empty(key) {
                    return Ok(ApplicationTransition::Exit);
                }
                self.state
                    .edit_composer(|composers, key| composers.clear(key));
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::InsertText(text)) => {
                self.state
                    .edit_composer(|composers, key| composers.insert(key, &text));
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::PasteText(text)) => {
                self.state.paste_into_composer(&text);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::InsertNewline) => {
                self.state
                    .edit_composer(|composers, key| composers.insert(key, "\n"));
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::DeleteBackward) => {
                self.state
                    .edit_composer(|composers, key| composers.delete_backward(key));
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::DeleteForward) => {
                self.state
                    .edit_composer(|composers, key| composers.delete_forward(key));
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::MoveCursorLeft) => {
                self.state
                    .navigate_composer(|composers, key| composers.move_left(key));
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::MoveCursorRight) => {
                self.state
                    .navigate_composer(|composers, key| composers.move_right(key));
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::HistoryPrevious) => {
                self.state
                    .edit_composer(|composers, key| composers.history_previous(key));
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::HistoryNext) => {
                self.state
                    .edit_composer(|composers, key| composers.history_next(key));
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::ScrollTranscriptPageUp) => {
                self.state.navigate_transcript_page(TranscriptDirection::Up);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::ScrollTranscriptPageDown) => {
                self.state
                    .navigate_transcript_page(TranscriptDirection::Down);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::FollowLatest) => {
                self.state.follow_latest();
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::SelectPreviousAutocomplete) => {
                self.state.command_autocomplete.select_previous();
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::SelectNextAutocomplete) => {
                self.state.command_autocomplete.select_next();
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::DismissAutocomplete) => {
                let key = self.state.composer_key();
                let text = self.state.composers.text(key);
                self.state.command_autocomplete.dismiss_for_text(text);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::SelectAutocomplete) => {
                let Some(command) = self.state.command_autocomplete.selected() else {
                    return Ok(ApplicationTransition::Continue);
                };
                if self.state.selection_update_pending()
                    && matches!(
                        command,
                        SemanticCommandId::SessionList | SemanticCommandId::SessionNew
                    )
                {
                    return Ok(ApplicationTransition::Continue);
                }
                let key = self.state.composer_key();
                self.state.composers.clear(key);
                self.state.sync_command_autocomplete();
                self.invoke_semantic(command)
            }
            ApplicationEvent::Command(CommandId::InsertSessionSearch(text)) => {
                self.edit_session_picker(|picker| picker.insert(&text));
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::DeleteSessionSearchBackward) => {
                self.edit_session_picker(SessionPicker::delete_backward);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::SelectPreviousSession) => {
                self.edit_session_picker(SessionPicker::select_previous);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::SelectNextSession) => {
                self.edit_session_picker(SessionPicker::select_next);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::PagePreviousSessions) => {
                self.edit_session_picker(SessionPicker::page_previous);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::PageNextSessions) => {
                self.edit_session_picker(SessionPicker::page_next);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::ToggleSessionScope) => {
                if self.state.session_picker.is_busy() {
                    return Ok(ApplicationTransition::Continue);
                }
                let request = self.state.session_picker.toggle_scope();
                Ok(ApplicationTransition::ListSessions(request))
            }
            ApplicationEvent::Command(CommandId::SelectSession) => {
                Ok(self.state.session_picker.begin_attachment().map_or(
                    ApplicationTransition::Continue,
                    ApplicationTransition::AttachSession,
                ))
            }
            ApplicationEvent::Command(CommandId::CloseSessionPicker) => {
                if !self.state.session_picker.is_busy() {
                    self.state.session_picker.close();
                }
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::InsertModelSearch(text)) => {
                self.state.model_picker.insert(&text);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::DeleteModelSearchBackward) => {
                self.state.model_picker.delete_backward();
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::SelectPreviousModel) => {
                self.state.model_picker.select_previous();
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::SelectNextModel) => {
                self.state.model_picker.select_next();
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::PagePreviousModels) => {
                self.state.model_picker.page_previous();
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::PageNextModels) => {
                self.state.model_picker.page_next();
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::SelectModel) => {
                match self.state.model_picker.choose() {
                    Some(ModelPickerAction::Retry) => {
                        Ok(self.state.model_picker.begin_retry().map_or(
                            ApplicationTransition::Continue,
                            ApplicationTransition::ListModels,
                        ))
                    }
                    Some(ModelPickerAction::Select(model)) => {
                        self.state.model_picker.close();
                        if !model.options.is_empty() {
                            let current = self.state.agent_selection().cloned();
                            self.state.model_options.open(model, current.as_ref());
                            return Ok(ApplicationTransition::Continue);
                        }
                        self.apply_agent_selection(model.default_agent_selection())
                    }
                    None => Ok(ApplicationTransition::Continue),
                }
            }
            ApplicationEvent::Command(CommandId::CloseModelPicker) => {
                self.state.model_picker.close();
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::InvokeSemantic(command)) => {
                self.invoke_semantic(command)
            }
            ApplicationEvent::Command(
                command @ (CommandId::SubmitSteer | CommandId::SubmitQueue),
            ) => {
                if self.state.pending_submission.is_some() {
                    return Ok(ApplicationTransition::Continue);
                }
                let delivery = if command == CommandId::SubmitQueue {
                    PromptDelivery::Queue
                } else {
                    PromptDelivery::Steer
                };
                let key = self.state.composer_key();
                if self.state.composers.text(key).trim().is_empty() {
                    self.state.submission_error =
                        Some("Prompt must contain non-whitespace text".to_owned());
                    return Ok(ApplicationTransition::Continue);
                }
                let prompt = self.state.composers.begin_submission(key);
                self.state.sync_command_autocomplete();
                self.state.failed_submissions.remove(&prompt.id);
                self.state.submission_error = None;
                if let ComposerKey::Session(session_id) = key {
                    self.state.pending_submission = Some(PendingSubmission {
                        source: key,
                        target: SubmissionTarget::AdmitPrompt(session_id, delivery),
                        prompt: prompt.clone(),
                    });
                    return Ok(ApplicationTransition::AdmitPrompt {
                        session_id,
                        request: AdmitPromptRequest { prompt, delivery },
                    });
                }
                self.state.pending_submission = Some(PendingSubmission {
                    source: key,
                    target: SubmissionTarget::CreateSession,
                    prompt: prompt.clone(),
                });
                Ok(ApplicationTransition::CreateSession(CreateSessionRequest {
                    agent_selection: self.state.landing_agent_selection.clone(),
                    workspace: Workspace {
                        path: self.state.workspace.clone(),
                    },
                    prompt,
                }))
            }
            ApplicationEvent::Command(CommandId::BeginLeader) => {
                self.state.command_mode = CommandMode::Leader;
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::OpenQueuedPrompts) => {
                let Some(session_id) = self.session_id() else {
                    self.state.command_mode = CommandMode::Composer;
                    return Ok(ApplicationTransition::Continue);
                };
                self.state.command_mode = self.state.queued_prompts(session_id).first().map_or(
                    CommandMode::Composer,
                    |prompt| CommandMode::QueuedPrompts {
                        selected: prompt.id,
                    },
                );
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(
                command @ (CommandId::SelectPreviousQueuedPrompt
                | CommandId::SelectNextQueuedPrompt),
            ) => {
                let (Some(session_id), CommandMode::QueuedPrompts { selected }) =
                    (self.session_id(), self.state.command_mode)
                else {
                    return Ok(ApplicationTransition::Continue);
                };
                let queued = self.state.queued_prompts(session_id);
                if let Some(index) = queued.iter().position(|prompt| prompt.id == selected) {
                    let next = if command == CommandId::SelectPreviousQueuedPrompt {
                        index.saturating_sub(1)
                    } else {
                        (index + 1).min(queued.len().saturating_sub(1))
                    };
                    self.state.command_mode = CommandMode::QueuedPrompts {
                        selected: queued[next].id,
                    };
                }
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(
                command @ (CommandId::PromoteSelectedPrompt | CommandId::CancelSelectedPrompt),
            ) => {
                let (Some(session_id), CommandMode::QueuedPrompts { selected }) =
                    (self.session_id(), self.state.command_mode)
                else {
                    return Ok(ApplicationTransition::Continue);
                };
                self.state.command_mode = CommandMode::Composer;
                Ok(if command == CommandId::PromoteSelectedPrompt {
                    ApplicationTransition::PromotePrompt {
                        session_id,
                        prompt_id: selected,
                    }
                } else {
                    ApplicationTransition::CancelPrompt {
                        session_id,
                        prompt_id: selected,
                    }
                })
            }
            ApplicationEvent::Command(CommandId::RequestInterrupt) => {
                if let Some(turn_id) = self.state.active_turn_id() {
                    self.state.command_mode = CommandMode::InterruptConfirmation { turn_id };
                }
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::ConfirmInterrupt) => {
                let (Some(session_id), CommandMode::InterruptConfirmation { turn_id }) =
                    (self.session_id(), self.state.command_mode)
                else {
                    return Ok(ApplicationTransition::Continue);
                };
                self.state.command_mode = CommandMode::Composer;
                Ok(ApplicationTransition::InterruptTurn {
                    session_id,
                    turn_id,
                })
            }
            ApplicationEvent::Command(CommandId::CloseCommandMode) => {
                self.state.command_mode = CommandMode::Composer;
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::ReconnectGraceElapsed => {
                if self.state.recovery.is_some() {
                    self.state.reconnect_overlay_visible = true;
                }
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Managed(ManagedEvent::Fatal(error)) => Err(anyhow!(error)),
            ApplicationEvent::Managed(event @ ManagedEvent::ServerShutdown(_)) => {
                self.state.apply(event);
                Ok(ApplicationTransition::Exit)
            }
            ApplicationEvent::Managed(event) => {
                let had_session = self.state.session.is_some();
                self.state.apply(event);
                self.state.reconcile_command_mode();
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
            ApplicationEvent::PromptAdmissionSucceeded(prompt_id) => {
                self.state.acknowledge_pending_submission(prompt_id);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::PromptAdmissionFailed { prompt_id, error } => {
                self.state.fail_pending_submission(prompt_id, error);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::SessionAttached(snapshot) => {
                let closes_picker = self.state.session_picker.attaching_to(snapshot.session.id);
                self.state.apply_attached_session(snapshot)?;
                if closes_picker {
                    self.state.session_picker.close();
                }
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::SessionsListed { request, sessions } => {
                let current = self.session_id();
                self.state.session_picker.load(&request, sessions, current);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::SessionListingFailed { request, error } => {
                self.state.session_picker.fail_listing(&request, error);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::ModelsListed { request, catalog } => {
                let accepted = self.state.model_picker.is_active_request(&request);
                let current = self.state.agent_selection().cloned();
                self.state
                    .model_picker
                    .load(&request, catalog, current.as_ref());
                if accepted {
                    self.reconcile_model_options(false);
                }
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::ModelsRefreshed { request, catalog } => {
                let accepted = self.state.model_picker.is_active_request(&request);
                let current = self.state.agent_selection().cloned();
                self.state
                    .model_picker
                    .load(&request, catalog, current.as_ref());
                self.state.model_picker.finish(&request);
                if accepted {
                    self.reconcile_model_options(true);
                }
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::ModelListingFailed { request, error } => {
                let accepted = self.state.model_picker.is_active_request(&request);
                self.state.model_picker.fail(&request, error);
                if accepted {
                    self.reconcile_model_options(true);
                }
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::LandingAgentSelectionConfirmed(selection) => {
                if self.state.pending_landing_agent_selection.take().is_none() {
                    return Ok(ApplicationTransition::Continue);
                }
                self.state.confirmed_landing_agent_selection = Some(selection.clone());
                self.state.submission_error = None;
                if let Some(queued) = self.state.queued_landing_agent_selection.take()
                    && queued != selection
                {
                    return Ok(self.begin_landing_agent_selection_confirmation(queued));
                }
                self.state.landing_agent_selection = Some(selection);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::LandingAgentSelectionConfirmationFailed(error) => {
                if self.state.pending_landing_agent_selection.take().is_none() {
                    return Ok(ApplicationTransition::Continue);
                }
                if let Some(queued) = self.state.queued_landing_agent_selection.take() {
                    return Ok(self.begin_landing_agent_selection_confirmation(queued));
                }
                self.state.landing_agent_selection =
                    self.state.confirmed_landing_agent_selection.clone();
                self.state.submission_error = Some(error);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::AgentSelectionUpdated {
                operation_id,
                selection,
            } => {
                if self
                    .state
                    .pending_agent_selection
                    .as_ref()
                    .is_some_and(|pending| pending.operation_id == operation_id)
                {
                    let session_id = self
                        .state
                        .pending_agent_selection
                        .take()
                        .expect("matching pending Agent Selection exists")
                        .session_id;
                    self.state.submission_error = None;
                    if let Some((queued_session, queued)) = self.state.queued_agent_selection.take()
                        && queued_session == session_id
                        && queued != selection
                    {
                        self.state.confirmed_agent_selection = Some((session_id, selection));
                        return self.begin_agent_selection_update(queued_session, queued);
                    }
                    self.state.confirmed_agent_selection = Some((session_id, selection));
                }
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::AgentSelectionUpdateFailed {
                operation_id,
                error,
            } => {
                if self
                    .state
                    .pending_agent_selection
                    .as_ref()
                    .is_some_and(|pending| pending.operation_id == operation_id)
                {
                    let session_id = self
                        .state
                        .pending_agent_selection
                        .take()
                        .expect("matching pending Agent Selection exists")
                        .session_id;
                    if let Some((queued_session, queued)) = self.state.queued_agent_selection.take()
                        && queued_session == session_id
                    {
                        // A newer queued selection supersedes this failure.
                        return self.begin_agent_selection_update(queued_session, queued);
                    }
                    // Keep any prior confirmed acceptance: it is newer
                    // authoritative state than the snapshot base.
                    self.state.submission_error = Some(error);
                }
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::SessionAttachmentFailed(error) => {
                let request = self.state.session_picker.fail_attachment(error);
                Ok(ApplicationTransition::ListSessions(request))
            }
            ApplicationEvent::SessionDeletionFailed { session_id, error } => {
                self.state.session_picker.fail_deletion(session_id, error);
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

    fn invoke_semantic(&mut self, command: SemanticCommandId) -> Result<ApplicationTransition> {
        if self.state.selection_update_pending()
            && matches!(
                command,
                SemanticCommandId::SessionList | SemanticCommandId::SessionNew
            )
        {
            return Ok(ApplicationTransition::Continue);
        }
        match command {
            SemanticCommandId::ModelList => {
                let current = self.state.agent_selection().cloned();
                let provider_scope = self
                    .state
                    .session
                    .as_ref()
                    .and_then(|_| current.as_ref().map(|selection| selection.provider.clone()));
                let request = self
                    .state
                    .model_picker
                    .open(current.as_ref(), provider_scope);
                self.state.command_mode = CommandMode::Composer;
                Ok(ApplicationTransition::ListModels(request))
            }
            SemanticCommandId::ModelOptions => {
                self.state.pending_model_options = true;
                self.state.submission_error = None;
                let request = self.state.model_picker.begin_refresh();
                self.reconcile_model_options(false);
                self.state.command_mode = CommandMode::Composer;
                Ok(ApplicationTransition::ListModels(request))
            }
            SemanticCommandId::ModelOptionsPrevious => {
                self.state.model_options.select_previous();
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::ModelOptionsNext => {
                self.state.model_options.select_next();
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::ModelOptionsSelect => {
                self.state.model_options.choose();
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::ModelOptionsApply => {
                if self.state.model_options.is_choice_picker_open() {
                    return Ok(ApplicationTransition::Continue);
                }
                let Some(selection) = self.state.model_options.apply() else {
                    return Ok(ApplicationTransition::Continue);
                };
                self.state.model_options.close();
                self.apply_agent_selection(selection)
            }
            SemanticCommandId::ModelOptionsCancel => {
                self.state.model_options.close();
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::ModelOptionReasoningCycle => {
                let current = self.state.agent_selection().cloned();
                let Some(model) = self
                    .state
                    .model_picker
                    .cached_model_for_options(current.as_ref())
                else {
                    self.state.submission_error = Some(
                        "No concrete Model is loaded yet; use /models to choose one".to_owned(),
                    );
                    let request = self.state.model_picker.begin_refresh();
                    return Ok(ApplicationTransition::ListModels(request));
                };
                match cycle_reasoning_effort(&model, current.as_ref()) {
                    ReasoningCycle::Advanced(selection) => self.apply_agent_selection(selection),
                    ReasoningCycle::Unavailable(message) => {
                        self.state.submission_error = Some(message);
                        Ok(ApplicationTransition::Continue)
                    }
                }
            }
            SemanticCommandId::SessionList => {
                let request = self.state.session_picker.open();
                self.state.command_mode = CommandMode::Composer;
                Ok(ApplicationTransition::ListSessions(request))
            }
            SemanticCommandId::SessionDelete => {
                Ok(self.state.session_picker.begin_deletion().map_or(
                    ApplicationTransition::Continue,
                    ApplicationTransition::DeleteSession,
                ))
            }
            SemanticCommandId::SessionNew => {
                let inherited_selection = self.state.agent_selection().cloned();
                let source = self.state.composer_key();
                self.state.composers.clear(source);
                self.state.composers.clear(ComposerKey::Landing);
                self.state.submission_error = None;
                self.state.command_mode = CommandMode::Composer;
                let detached = self.state.session.take().is_some();
                if detached {
                    self.state.landing_agent_selection = inherited_selection.clone();
                    self.state.confirmed_landing_agent_selection = inherited_selection;
                    self.state.pending_landing_agent_selection = None;
                    self.state.queued_landing_agent_selection = None;
                    self.state.confirmed_agent_selection = None;
                }
                self.state.session_events_blocked = detached;
                self.state.sync_command_autocomplete();
                Ok(if detached {
                    ApplicationTransition::DetachSession
                } else {
                    ApplicationTransition::Continue
                })
            }
        }
    }

    fn apply_agent_selection(
        &mut self,
        selection: AgentSelection,
    ) -> Result<ApplicationTransition> {
        let Some(session_id) = self.session_id() else {
            self.state.landing_agent_selection = Some(selection.clone());
            if self.state.pending_landing_agent_selection.is_some() {
                self.state.queued_landing_agent_selection = Some(selection);
                return Ok(ApplicationTransition::Continue);
            }
            return Ok(self.begin_landing_agent_selection_confirmation(selection));
        };
        self.state.submission_error = None;
        if self.state.pending_agent_selection.is_some() {
            // Transport stays serialized: coalesce to the newest complete
            // Agent Selection instead of sending every intermediate state.
            self.state.queued_agent_selection = Some((session_id, selection));
            return Ok(ApplicationTransition::Continue);
        }
        self.begin_agent_selection_update(session_id, selection)
    }

    fn begin_landing_agent_selection_confirmation(
        &mut self,
        selection: AgentSelection,
    ) -> ApplicationTransition {
        self.state.pending_landing_agent_selection = Some(selection.clone());
        ApplicationTransition::ConfirmLandingAgentSelection(selection)
    }

    fn begin_agent_selection_update(
        &mut self,
        session_id: SessionId,
        selection: AgentSelection,
    ) -> Result<ApplicationTransition> {
        let operation_id = AgentSelectionOperationId::new();
        self.state.pending_agent_selection = Some(PendingAgentSelection {
            session_id,
            operation_id,
            selection: selection.clone(),
        });
        Ok(ApplicationTransition::UpdateAgentSelection {
            session_id,
            request: UpdateAgentSelectionRequest {
                operation_id,
                selection,
            },
        })
    }

    fn reconcile_model_options(&mut self, catalog_request_settled: bool) {
        if self.state.model_options.is_open() {
            let current_model = self
                .state
                .model_options
                .model()
                .map(|model| (model.provider.clone(), model.id.clone()));
            if let Some((provider, model)) = current_model {
                if let Some(refreshed) = self.state.model_picker.cached_model(&provider, &model) {
                    self.state.model_options.refresh(refreshed);
                } else if catalog_request_settled {
                    self.state.model_options.mark_model_unavailable();
                }
            }
        }
        if !self.state.pending_model_options {
            return;
        }
        let current = self.state.agent_selection().cloned();
        let Some(model) = self
            .state
            .model_picker
            .cached_model_for_options(current.as_ref())
        else {
            if catalog_request_settled {
                self.state.pending_model_options = false;
                self.state.submission_error =
                    Some("No concrete Model is available; use /models to choose one".to_owned());
            }
            return;
        };
        if model.options.is_empty() {
            if catalog_request_settled {
                self.state.pending_model_options = false;
            }
            self.state.submission_error = Some(format!(
                "{} has no configurable options; use /models to choose another Model",
                model.display_name
            ));
            return;
        }
        self.state.pending_model_options = false;
        self.state.submission_error = None;
        self.state.model_options.open(model, current.as_ref());
    }

    fn edit_session_picker(&mut self, edit: impl FnOnce(&mut SessionPicker)) {
        if !self.state.session_picker.is_busy() {
            edit(&mut self.state.session_picker);
        }
    }

    pub fn render(&self, frame: &mut Frame<'_>) {
        render_with_slots(frame, &self.state, &self.slots);
    }

    pub fn handle_terminal_event(&mut self, event: InputEvent) -> Result<ApplicationTransition> {
        if self.state.model_options.is_open() {
            return command_for_model_options_event(event)
                .map_or(Ok(ApplicationTransition::Continue), |command| {
                    self.handle_event(ApplicationEvent::Command(command))
                });
        }
        if self.state.model_picker.is_open() {
            return command_for_model_picker_event(event)
                .map_or(Ok(ApplicationTransition::Continue), |command| {
                    self.handle_event(ApplicationEvent::Command(command))
                });
        }
        if self.state.session_picker.is_open() {
            return command_for_session_picker_event(event)
                .map_or(Ok(ApplicationTransition::Continue), |command| {
                    self.handle_event(ApplicationEvent::Command(command))
                });
        }
        if self.state.command_autocomplete.is_visible()
            && let Some(command) = command_for_autocomplete_event(event.clone())
        {
            return self.handle_event(ApplicationEvent::Command(command));
        }
        let command = match self.state.command_mode {
            CommandMode::Composer => command_for_terminal_event(event),
            CommandMode::Leader => command_for_leader_event(event),
            CommandMode::QueuedPrompts { .. } => command_for_queued_prompt_event(event),
            CommandMode::InterruptConfirmation { .. } => {
                command_for_interrupt_confirmation_event(event)
            }
        };
        command.map_or(Ok(ApplicationTransition::Continue), |command| {
            self.handle_event(ApplicationEvent::Command(command))
        })
    }

    fn session_id(&self) -> Option<SessionId> {
        self.state
            .session
            .as_ref()
            .map(SessionProjection::session_id)
    }

    fn is_recovering(&self) -> bool {
        self.state.recovery.is_some()
    }
}

pub fn command_for_terminal_event(event: InputEvent) -> Option<CommandId> {
    match event {
        InputEvent::Key(key) if key.kind != KeyEventKind::Press => None,
        InputEvent::Key(key) if binding_for(key).is_some() => {
            binding_for(key).map(|binding| binding.command.clone())
        }
        InputEvent::Key(key) if command_for_direct_semantic_key(key).is_some() => {
            command_for_direct_semantic_key(key).map(CommandId::InvokeSemantic)
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
        InputEvent::Paste(text) => Some(CommandId::PasteText(text)),
        _ => None,
    }
}

fn command_for_autocomplete_event(event: InputEvent) -> Option<CommandId> {
    let InputEvent::Key(key) = event else {
        return None;
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    match (key.code, key.modifiers) {
        (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
            Some(CommandId::SelectPreviousAutocomplete)
        }
        (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
            Some(CommandId::SelectNextAutocomplete)
        }
        (KeyCode::Enter | KeyCode::Tab, KeyModifiers::NONE) => Some(CommandId::SelectAutocomplete),
        (KeyCode::Esc, KeyModifiers::NONE) => Some(CommandId::DismissAutocomplete),
        _ => None,
    }
}

fn command_for_session_picker_event(event: InputEvent) -> Option<CommandId> {
    if let InputEvent::Key(key) = &event
        && key.kind == KeyEventKind::Press
        && key.code == KeyCode::Char('d')
        && key.modifiers == KeyModifiers::CONTROL
    {
        return Some(CommandId::InvokeSemantic(SemanticCommandId::SessionDelete));
    }
    command_for_picker_event(event, &SESSION_PICKER_COMMANDS)
}

fn command_for_model_picker_event(event: InputEvent) -> Option<CommandId> {
    command_for_picker_event(event, &MODEL_PICKER_COMMANDS)
}

fn command_for_model_options_event(event: InputEvent) -> Option<CommandId> {
    let InputEvent::Key(key) = event else {
        return None;
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    match (key.code, key.modifiers) {
        (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => Some(
            CommandId::InvokeSemantic(SemanticCommandId::ModelOptionsPrevious),
        ),
        (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => Some(
            CommandId::InvokeSemantic(SemanticCommandId::ModelOptionsNext),
        ),
        (KeyCode::Enter, KeyModifiers::NONE) => Some(CommandId::InvokeSemantic(
            SemanticCommandId::ModelOptionsSelect,
        )),
        (KeyCode::Enter, KeyModifiers::CONTROL) => Some(CommandId::InvokeSemantic(
            SemanticCommandId::ModelOptionsApply,
        )),
        (KeyCode::Esc, KeyModifiers::NONE) => Some(CommandId::InvokeSemantic(
            SemanticCommandId::ModelOptionsCancel,
        )),
        _ => None,
    }
}

struct PickerCommandBindings {
    previous: CommandId,
    next: CommandId,
    page_previous: CommandId,
    page_next: CommandId,
    select: CommandId,
    close: CommandId,
    delete_backward: CommandId,
    insert: fn(String) -> CommandId,
    toggle_scope: Option<CommandId>,
}

const SESSION_PICKER_COMMANDS: PickerCommandBindings = PickerCommandBindings {
    previous: CommandId::SelectPreviousSession,
    next: CommandId::SelectNextSession,
    page_previous: CommandId::PagePreviousSessions,
    page_next: CommandId::PageNextSessions,
    select: CommandId::SelectSession,
    close: CommandId::CloseSessionPicker,
    delete_backward: CommandId::DeleteSessionSearchBackward,
    insert: CommandId::InsertSessionSearch,
    toggle_scope: Some(CommandId::ToggleSessionScope),
};

const MODEL_PICKER_COMMANDS: PickerCommandBindings = PickerCommandBindings {
    previous: CommandId::SelectPreviousModel,
    next: CommandId::SelectNextModel,
    page_previous: CommandId::PagePreviousModels,
    page_next: CommandId::PageNextModels,
    select: CommandId::SelectModel,
    close: CommandId::CloseModelPicker,
    delete_backward: CommandId::DeleteModelSearchBackward,
    insert: CommandId::InsertModelSearch,
    toggle_scope: None,
};

fn command_for_picker_event(
    event: InputEvent,
    bindings: &PickerCommandBindings,
) -> Option<CommandId> {
    match event {
        InputEvent::Key(key) if key.kind != KeyEventKind::Press => None,
        InputEvent::Key(key) => match (key.code, key.modifiers) {
            (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                Some(bindings.previous.clone())
            }
            (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                Some(bindings.next.clone())
            }
            (KeyCode::PageUp, KeyModifiers::NONE) => Some(bindings.page_previous.clone()),
            (KeyCode::PageDown, KeyModifiers::NONE) => Some(bindings.page_next.clone()),
            (KeyCode::Char('a'), KeyModifiers::CONTROL) => bindings.toggle_scope.clone(),
            (KeyCode::Enter, KeyModifiers::NONE) => Some(bindings.select.clone()),
            (KeyCode::Esc, KeyModifiers::NONE) => Some(bindings.close.clone()),
            (KeyCode::Backspace, KeyModifiers::NONE) => Some(bindings.delete_backward.clone()),
            (KeyCode::Char(character), modifiers)
                if !modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
            {
                Some((bindings.insert)(character.to_string()))
            }
            _ => None,
        },
        InputEvent::Paste(text) => Some((bindings.insert)(text)),
        _ => None,
    }
}

#[derive(Clone, Debug)]
struct CommandBinding {
    code: KeyCode,
    modifiers: KeyModifiers,
    command: CommandId,
    label: &'static str,
}

const COMMAND_BINDINGS: &[CommandBinding] = &[
    CommandBinding {
        code: KeyCode::Enter,
        modifiers: KeyModifiers::NONE,
        command: CommandId::SubmitSteer,
        label: "Enter",
    },
    CommandBinding {
        code: KeyCode::Enter,
        modifiers: KeyModifiers::ALT,
        command: CommandId::SubmitQueue,
        label: "Alt+Enter",
    },
    CommandBinding {
        code: KeyCode::Enter,
        modifiers: KeyModifiers::SHIFT,
        command: CommandId::InsertNewline,
        label: "Shift+Enter",
    },
    CommandBinding {
        code: KeyCode::Enter,
        modifiers: KeyModifiers::CONTROL,
        command: CommandId::InsertNewline,
        label: "Ctrl+Enter",
    },
    CommandBinding {
        code: KeyCode::Char('j'),
        modifiers: KeyModifiers::CONTROL,
        command: CommandId::InsertNewline,
        label: "Ctrl+J",
    },
    CommandBinding {
        code: KeyCode::Char('c'),
        modifiers: KeyModifiers::CONTROL,
        command: CommandId::ClearOrExit,
        label: "Ctrl+C",
    },
    CommandBinding {
        code: KeyCode::Backspace,
        modifiers: KeyModifiers::NONE,
        command: CommandId::DeleteBackward,
        label: "Backspace",
    },
    CommandBinding {
        code: KeyCode::Delete,
        modifiers: KeyModifiers::NONE,
        command: CommandId::DeleteForward,
        label: "Delete",
    },
    CommandBinding {
        code: KeyCode::Left,
        modifiers: KeyModifiers::NONE,
        command: CommandId::MoveCursorLeft,
        label: "Left",
    },
    CommandBinding {
        code: KeyCode::Right,
        modifiers: KeyModifiers::NONE,
        command: CommandId::MoveCursorRight,
        label: "Right",
    },
    CommandBinding {
        code: KeyCode::Up,
        modifiers: KeyModifiers::NONE,
        command: CommandId::HistoryPrevious,
        label: "Up",
    },
    CommandBinding {
        code: KeyCode::Down,
        modifiers: KeyModifiers::NONE,
        command: CommandId::HistoryNext,
        label: "Down",
    },
    CommandBinding {
        code: KeyCode::PageUp,
        modifiers: KeyModifiers::NONE,
        command: CommandId::ScrollTranscriptPageUp,
        label: "PageUp",
    },
    CommandBinding {
        code: KeyCode::PageDown,
        modifiers: KeyModifiers::NONE,
        command: CommandId::ScrollTranscriptPageDown,
        label: "PageDown",
    },
    CommandBinding {
        code: KeyCode::End,
        modifiers: KeyModifiers::NONE,
        command: CommandId::FollowLatest,
        label: "End",
    },
    CommandBinding {
        code: KeyCode::Char('x'),
        modifiers: KeyModifiers::CONTROL,
        command: CommandId::BeginLeader,
        label: "Ctrl+X",
    },
    CommandBinding {
        code: KeyCode::Esc,
        modifiers: KeyModifiers::NONE,
        command: CommandId::RequestInterrupt,
        label: "Esc",
    },
];

const LEADER_BINDINGS: &[CommandBinding] = &[CommandBinding {
    code: KeyCode::Char('q'),
    modifiers: KeyModifiers::NONE,
    command: CommandId::OpenQueuedPrompts,
    label: "q",
}];

const QUEUED_PROMPT_BINDINGS: &[CommandBinding] = &[
    CommandBinding {
        code: KeyCode::Enter,
        modifiers: KeyModifiers::NONE,
        command: CommandId::PromoteSelectedPrompt,
        label: "Enter",
    },
    CommandBinding {
        code: KeyCode::Char('d'),
        modifiers: KeyModifiers::CONTROL,
        command: CommandId::CancelSelectedPrompt,
        label: "Ctrl+D",
    },
    CommandBinding {
        code: KeyCode::Up,
        modifiers: KeyModifiers::NONE,
        command: CommandId::SelectPreviousQueuedPrompt,
        label: "Up",
    },
    CommandBinding {
        code: KeyCode::Down,
        modifiers: KeyModifiers::NONE,
        command: CommandId::SelectNextQueuedPrompt,
        label: "Down",
    },
    CommandBinding {
        code: KeyCode::Esc,
        modifiers: KeyModifiers::NONE,
        command: CommandId::CloseCommandMode,
        label: "Esc",
    },
];

const INTERRUPT_CONFIRMATION_BINDINGS: &[CommandBinding] = &[CommandBinding {
    code: KeyCode::Esc,
    modifiers: KeyModifiers::NONE,
    command: CommandId::ConfirmInterrupt,
    label: "Esc",
}];

fn binding_for(key: KeyEvent) -> Option<&'static CommandBinding> {
    COMMAND_BINDINGS
        .iter()
        .find(|binding| binding.code == key.code && binding.modifiers == key.modifiers)
}

fn binding_label(command: &CommandId) -> &'static str {
    if let CommandId::InvokeSemantic(command) = command {
        return descriptor(*command)
            .keybinding
            .map_or("", |binding| binding.label);
    }
    COMMAND_BINDINGS
        .iter()
        .chain(LEADER_BINDINGS)
        .chain(QUEUED_PROMPT_BINDINGS)
        .chain(INTERRUPT_CONFIRMATION_BINDINGS)
        .find(|binding| &binding.command == command)
        .map_or("", |binding| binding.label)
}

fn command_for_leader_event(event: InputEvent) -> Option<CommandId> {
    let semantic = match &event {
        InputEvent::Key(key) if key.kind == KeyEventKind::Press => {
            command_for_leader_key(*key).map(CommandId::InvokeSemantic)
        }
        _ => None,
    };
    semantic
        .or_else(|| command_from_scoped_bindings(event, LEADER_BINDINGS))
        .or(Some(CommandId::CloseCommandMode))
}

fn command_for_queued_prompt_event(event: InputEvent) -> Option<CommandId> {
    command_from_scoped_bindings(event, QUEUED_PROMPT_BINDINGS)
}

fn command_for_interrupt_confirmation_event(event: InputEvent) -> Option<CommandId> {
    command_from_scoped_bindings(event, INTERRUPT_CONFIRMATION_BINDINGS)
        .or(Some(CommandId::CloseCommandMode))
}

fn command_from_scoped_bindings(
    event: InputEvent,
    bindings: &'static [CommandBinding],
) -> Option<CommandId> {
    let InputEvent::Key(key) = event else {
        return None;
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    bindings
        .iter()
        .find(|binding| binding.code == key.code && binding.modifiers == key.modifiers)
        .map(|binding| binding.command.clone())
}

pub fn render(frame: &mut Frame<'_>, state: &TuiState) {
    render_with_slots(frame, state, &RenderSlots::builtins());
}

fn render_with_slots(frame: &mut Frame<'_>, state: &TuiState, slots: &RenderSlots) {
    let theme = Theme::system();
    if terminal_is_too_small(frame.area()) {
        render_terminal_too_small(frame, &theme);
        return;
    }
    let composer = if state.session.is_some() {
        render_session(frame, state, slots, &theme)
    } else {
        render_landing(frame, state, slots, &theme)
    };
    if state.command_autocomplete.is_visible() && !state.reconnect_overlay_visible {
        render_command_autocomplete(frame, state, composer.area, &theme);
    }
    if state.session_picker.is_open() && !state.reconnect_overlay_visible {
        render_session_picker(frame, state, &theme);
    }
    if state.model_picker.is_open() && !state.reconnect_overlay_visible {
        render_model_picker(frame, state, &theme);
    }
    if state.model_options.is_open() && !state.reconnect_overlay_visible {
        render_model_options(frame, state, &theme);
    }
    if state.reconnect_overlay_visible {
        render_reconnect_overlay(frame, &theme);
    } else if !state.session_picker.is_open()
        && !state.model_picker.is_open()
        && !state.model_options.is_open()
        && state.composer_focused
        && matches!(state.command_mode, CommandMode::Composer)
    {
        frame.set_cursor_position(composer.cursor);
    }
}

fn render_session_picker(frame: &mut Frame<'_>, state: &TuiState, theme: &Theme) {
    let area = centered_rect(
        frame.area(),
        frame.area().width.saturating_sub(4).min(72),
        frame.area().height.saturating_sub(2).min(12),
    );
    let content_width = area.width.saturating_sub(2);
    let content_height = area.height.saturating_sub(2);
    let mut lines = Vec::with_capacity(usize::from(content_height));
    let shows_search_and_footer = content_height >= 3;
    let error_in_title = content_height <= 3
        && !state.session_picker.is_loading()
        && state.session_picker.error().is_some();
    if shows_search_and_footer {
        lines.push(Line::styled(
            truncate_to_width(
                &format!("Search: {}", state.session_picker.query()),
                usize::from(content_width),
            ),
            theme.text.subdued,
        ));
    }
    if let Some(error) = state.session_picker.error()
        && !error_in_title
        && lines.len() < usize::from(content_height)
    {
        lines.push(Line::styled(
            truncate_to_width(&format!("Error: {error}"), usize::from(content_width)),
            theme.feedback.error,
        ));
    }
    if state.session_picker.is_loading() && lines.len() < usize::from(content_height) {
        lines.push(Line::styled("Loading Sessions…", theme.text.subdued));
    } else {
        let current = state.session.as_ref().map(SessionProjection::session_id);
        let footer_rows = usize::from(shows_search_and_footer);
        let row_capacity = usize::from(content_height).saturating_sub(lines.len() + footer_rows);
        let now = current_time_millis();
        let rows = state
            .session_picker
            .visible_rows(row_capacity, current)
            .map(|row| {
                let content = session_picker_row_text(row, usize::from(content_width), now);
                Line::styled(
                    content,
                    if row.selected {
                        theme.selection.focused
                    } else if row.unreadable {
                        theme.text.subdued
                    } else {
                        theme.text.primary
                    },
                )
            })
            .collect::<Vec<_>>();
        if rows.is_empty() && lines.len() < usize::from(content_height).saturating_sub(footer_rows)
        {
            lines.push(Line::styled("No Sessions found", theme.text.subdued));
        } else {
            lines.extend(rows);
        }
    }
    if shows_search_and_footer && lines.len() < usize::from(content_height) {
        let scope = state.session_picker.scope().label();
        let status = if state.session_picker.is_attaching() {
            "Attaching…"
        } else if state.session_picker.is_deleting() {
            "Deleting…"
        } else {
            "Ctrl+A scope · Enter attach · Ctrl+D delete · Esc close"
        };
        lines.push(Line::styled(
            truncate_to_width(&format!("{scope} · {status}"), usize::from(content_width)),
            theme.text.subdued,
        ));
    }
    let title = if error_in_title {
        format!(
            " Sessions · Error: {} ",
            state
                .session_picker
                .error()
                .expect("error title requires a picker error")
        )
    } else {
        " Sessions ".to_owned()
    };
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(theme.border.default)
                .style(theme.surface.overlay),
        ),
        area,
    );
}

fn render_model_picker(frame: &mut Frame<'_>, state: &TuiState, theme: &Theme) {
    let area = centered_rect(
        frame.area(),
        frame.area().width.saturating_sub(4).min(76),
        frame.area().height.saturating_sub(2).min(14),
    );
    let content_width = area.width.saturating_sub(2);
    let content_height = area.height.saturating_sub(2);
    let mut lines = Vec::with_capacity(usize::from(content_height));
    if content_height >= 3 {
        lines.push(Line::styled(
            truncate_to_width(
                &format!("Search: {}", state.model_picker.query()),
                usize::from(content_width),
            ),
            theme.text.subdued,
        ));
    }
    if content_height >= 3
        && let Some(provider) = state.model_picker.provider_scope()
        && lines.len() < usize::from(content_height)
    {
        lines.push(Line::styled(
            truncate_to_width(
                &format!("Session Provider {provider} · use /new to change Provider"),
                usize::from(content_width),
            ),
            theme.text.subdued,
        ));
    }
    if state.model_picker.is_loading() && lines.len() < usize::from(content_height) {
        lines.push(Line::styled("Loading Models…", theme.text.subdued));
    } else {
        let footer_rows = usize::from(content_height >= 3);
        let row_capacity = usize::from(content_height).saturating_sub(lines.len() + footer_rows);
        let current = state.agent_selection();
        let rows = state
            .model_picker
            .visible_rows(row_capacity, current)
            .map(|row| match row {
                ModelPickerRow::Provider {
                    provider,
                    refreshing,
                } => Line::styled(
                    truncate_to_width(
                        &format!(
                            "Provider {provider}{}",
                            if refreshing { " · refreshing" } else { "" }
                        ),
                        usize::from(content_width),
                    ),
                    theme.accent.primary.add_modifier(Modifier::BOLD),
                ),
                ModelPickerRow::Model {
                    model,
                    selected,
                    current,
                } => Line::styled(
                    model_picker_row_text(model, selected, current, usize::from(content_width)),
                    if selected {
                        theme.selection.focused
                    } else if model.availability == ModelAvailability::Unavailable {
                        theme.text.subdued
                    } else {
                        theme.text.primary
                    },
                ),
                ModelPickerRow::Error {
                    provider,
                    message,
                    selected,
                } => Line::styled(
                    truncate_to_width(
                        &format!(
                            "{}Retry {provider}: {message}",
                            if selected { "› " } else { "  " }
                        ),
                        usize::from(content_width),
                    ),
                    if selected {
                        theme.selection.focused
                    } else {
                        theme.feedback.error
                    },
                ),
            })
            .collect::<Vec<_>>();
        lines.extend(rows);
        if !state.model_picker.has_rows() && lines.len() < usize::from(content_height) {
            lines.push(Line::styled("No Models found", theme.text.subdued));
        }
    }
    if content_height >= 3 && lines.len() < usize::from(content_height) {
        lines.push(Line::styled(
            truncate_to_width(
                "Type to search · Enter select/retry · Esc close",
                usize::from(content_width),
            ),
            theme.text.subdued,
        ));
    }
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Models ")
                .border_style(theme.border.default)
                .style(theme.surface.overlay),
        ),
        area,
    );
}

fn render_model_options(frame: &mut Frame<'_>, state: &TuiState, theme: &Theme) {
    let area = centered_rect(
        frame.area(),
        frame.area().width.saturating_sub(4).min(76),
        frame.area().height.saturating_sub(2).min(14),
    );
    let content_width = usize::from(area.width.saturating_sub(2));
    let content_height = usize::from(area.height.saturating_sub(2));
    let mut lines = Vec::with_capacity(content_height);
    let model = state
        .model_options
        .model()
        .expect("an open options screen has a Model");
    if content_height >= 2 {
        let unavailable = if model.availability == ModelAvailability::Unavailable {
            " · unavailable"
        } else {
            ""
        };
        lines.push(Line::styled(
            truncate_to_width(
                &format!(
                    "{} · Provider {}{unavailable}",
                    model.display_name, model.provider
                ),
                content_width,
            ),
            theme.accent.primary.add_modifier(Modifier::BOLD),
        ));
    }

    if state.model_options.is_choice_picker_open() {
        let descriptor = state
            .model_options
            .selected_descriptor()
            .expect("an open choice picker has a descriptor");
        if content_height >= 3 {
            lines.push(Line::styled(
                truncate_to_width(
                    descriptor
                        .description
                        .as_deref()
                        .unwrap_or("Choose a value"),
                    content_width,
                ),
                theme.text.subdued,
            ));
        }
        let footer_rows = usize::from(content_height >= 3);
        let capacity = content_height.saturating_sub(lines.len() + footer_rows);
        let rows = state.model_options.choice_rows();
        let selected = rows.iter().position(|row| row.selected).unwrap_or(0);
        let start = selected.saturating_add(1).saturating_sub(capacity);
        lines.extend(
            rows.into_iter()
                .skip(start)
                .take(capacity)
                .map(|row| model_option_choice_line(row, content_width, theme)),
        );
        if footer_rows > 0 && lines.len() < content_height {
            lines.push(Line::styled(
                truncate_to_width("Enter choose · Esc cancel all edits", content_width),
                theme.text.subdued,
            ));
        }
        render_model_options_box(
            frame,
            area,
            lines,
            &format!(" {} Choices ", descriptor.label),
            theme,
        );
        return;
    }

    let footer_rows = usize::from(content_height >= 3);
    let capacity = content_height.saturating_sub(lines.len() + footer_rows);
    let rows = state.model_options.rows();
    let selected = rows.iter().position(|row| row.selected).unwrap_or(0);
    let start = selected.saturating_add(1).saturating_sub(capacity);
    for row in rows.into_iter().skip(start).take(capacity) {
        let marker = if row.selected { "› " } else { "  " };
        let unavailable = if row.available { "" } else { " [unavailable]" };
        let description = row
            .description
            .map_or_else(String::new, |description| format!(" · {description}"));
        lines.push(Line::styled(
            truncate_to_width(
                &format!(
                    "{marker}{} · {}{unavailable}{description}",
                    row.label, row.value
                ),
                content_width,
            ),
            if row.selected {
                theme.selection.focused
            } else if row.available {
                theme.text.primary
            } else {
                theme.text.subdued
            },
        ));
    }
    if footer_rows > 0 && lines.len() < content_height {
        let controls = if state.model_options.is_valid() {
            "Enter configure · Ctrl+Enter apply · Esc cancel"
        } else {
            "Enter configure · Apply unavailable · Esc cancel"
        };
        lines.push(Line::styled(
            truncate_to_width(controls, content_width),
            theme.text.subdued,
        ));
    }
    render_model_options_box(frame, area, lines, " Model Options ", theme);
}

fn model_option_choice_line(
    row: ModelOptionChoiceRow,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let marker = if row.selected { "› " } else { "  " };
    let current = if row.current { " [current]" } else { "" };
    let unavailable = if row.available { "" } else { " [unavailable]" };
    let description = row
        .description
        .map_or_else(String::new, |description| format!(" · {description}"));
    Line::styled(
        truncate_to_width(
            &format!("{marker}{}{current}{unavailable}{description}", row.label),
            width,
        ),
        if row.selected {
            theme.selection.focused
        } else if row.available {
            theme.text.primary
        } else {
            theme.text.subdued
        },
    )
}

fn render_model_options_box(
    frame: &mut Frame<'_>,
    area: Rect,
    lines: Vec<Line<'static>>,
    title: &str,
    theme: &Theme,
) {
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(title.to_owned())
                .border_style(theme.border.default)
                .style(theme.surface.overlay),
        ),
        area,
    );
}

fn model_picker_row_text(
    model: &ModelDescriptor,
    selected: bool,
    current: bool,
    width: usize,
) -> String {
    let marker = if selected { "› " } else { "  " };
    let native = (model.display_name != model.id.as_str()).then_some(model.id.as_str());
    let mut states = Vec::new();
    let mut compact_states = Vec::new();
    if current {
        states.push("current");
        compact_states.push("C");
    }
    if model.is_default {
        states.push("default");
        compact_states.push("D");
    }
    if model.availability == ModelAvailability::Unavailable {
        states.push("unavailable");
        compact_states.push("U");
    }
    let marker_width = marker.width();
    let available = width.saturating_sub(marker_width);
    let field_count = 1 + usize::from(native.is_some()) + usize::from(!states.is_empty());
    let wide_separator = " · ";
    let compact_separator = " ";
    let full_state = (!states.is_empty()).then(|| format!("[{}]", states.join(", ")));
    let compact_state =
        (!compact_states.is_empty()).then(|| format!("[{}]", compact_states.join(",")));
    let minimum_text_width = 4 * (1 + usize::from(native.is_some()));
    let full_fixed = full_state.as_ref().map_or(0, |state| state.width())
        + wide_separator.width() * field_count.saturating_sub(1);
    let (separator, state) = if available >= full_fixed.saturating_add(minimum_text_width) {
        (wide_separator, full_state)
    } else {
        (compact_separator, compact_state)
    };
    let fixed = state.as_ref().map_or(0, |state| state.width())
        + separator.width() * field_count.saturating_sub(1);
    let flexible = available.saturating_sub(fixed);
    let (display_width, native_width) = native.map_or((flexible, 0), |native| {
        let native_width = native.width().min((flexible / 2).max(1));
        (flexible.saturating_sub(native_width), native_width)
    });
    let mut fields = vec![truncate_to_width(&model.display_name, display_width)];
    if let Some(native) = native {
        fields.push(truncate_to_width(native, native_width));
    }
    if let Some(state) = state {
        fields.push(state);
    }
    truncate_to_width(&format!("{marker}{}", fields.join(separator)), width)
}

fn session_picker_row_text(row: SessionPickerRow<'_>, width: usize, now: u64) -> String {
    let marker = if row.selected { "› " } else { "  " };
    if row.confirming_delete {
        return truncate_to_width(&format!("{marker}Press Ctrl+D again to confirm"), width);
    }
    let compact = width < usize::from(NARROW_TERMINAL_WIDTH);
    let status = if row.unreadable {
        Some(if compact {
            "U".to_owned()
        } else {
            "[unreadable]".to_owned()
        })
    } else {
        match (compact, row.current, row.active) {
            (_, false, false) => None,
            (true, true, true) => Some("CA".to_owned()),
            (true, true, false) => Some("C".to_owned()),
            (true, false, true) => Some("A".to_owned()),
            (false, true, true) => Some("[current, active]".to_owned()),
            (false, true, false) => Some("[current]".to_owned()),
            (false, false, true) => Some("[active]".to_owned()),
        }
    };
    let age = if compact {
        relative_update_time_compact(row.updated_at, now)
    } else {
        relative_update_time(row.updated_at, now)
    };
    let separator = if compact { " " } else { " · " };
    let mut metadata = status.into_iter().chain([age]).collect::<Vec<_>>();
    let marker_width = marker.width();
    let available = width.saturating_sub(marker_width);
    let fixed_metadata_width = metadata.join(separator).width();
    if let Some(workspace) = row.workspace {
        let minimum_title_width = usize::from(available > 0);
        let path_budget = available
            .saturating_sub(minimum_title_width)
            .saturating_sub(separator.width())
            .saturating_sub(fixed_metadata_width)
            .saturating_sub(separator.width());
        if path_budget > 0 {
            metadata.push(truncate_from_left_to_width(
                workspace.to_string_lossy().as_ref(),
                path_budget,
            ));
        }
    }
    let metadata = metadata.join(separator);
    let title_width = available
        .saturating_sub(metadata.width())
        .saturating_sub(separator.width());
    let title = truncate_to_width(row.title, title_width);
    truncate_to_width(&format!("{marker}{title}{separator}{metadata}"), width)
}

fn truncate_from_left_to_width(value: &str, width: usize) -> String {
    if value.width() <= width {
        return value.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_owned();
    }
    let suffix_width = width - 1;
    let mut suffix = String::new();
    let mut used = 0_usize;
    for character in value.chars().rev() {
        let character_width = character.width().unwrap_or(1);
        if used.saturating_add(character_width) > suffix_width {
            break;
        }
        suffix.insert(0, character);
        used += character_width;
    }
    format!("…{suffix}")
}

fn current_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn relative_update_time(updated_at: SessionTimestamp, now: u64) -> String {
    let elapsed_seconds = now.saturating_sub(updated_at.0) / 1_000;
    match elapsed_seconds {
        0..=59 => "now".to_owned(),
        60..=3_599 => format!("{}m ago", elapsed_seconds / 60),
        3_600..=86_399 => format!("{}h ago", elapsed_seconds / 3_600),
        _ => format!("{}d ago", elapsed_seconds / 86_400),
    }
}

fn relative_update_time_compact(updated_at: SessionTimestamp, now: u64) -> String {
    let elapsed_seconds = now.saturating_sub(updated_at.0) / 1_000;
    match elapsed_seconds {
        0..=59 => "now".to_owned(),
        60..=3_599 => format!("{}m", elapsed_seconds / 60),
        3_600..=86_399 => format!("{}h", elapsed_seconds / 3_600),
        _ => format!("{}d", elapsed_seconds / 86_400),
    }
}

#[derive(Clone, Copy, Debug)]
struct RenderedComposer {
    area: Rect,
    cursor: Position,
}

fn render_command_autocomplete(
    frame: &mut Frame<'_>,
    state: &TuiState,
    composer_area: Rect,
    theme: &Theme,
) {
    let available_width = frame
        .area()
        .width
        .saturating_sub(horizontal_padding(frame.area().width).saturating_mul(2));
    let width = available_width.clamp(1, 72);
    let room_above = composer_area.y.saturating_sub(frame.area().y);
    let bordered = room_above >= 3;
    let row_count = state.command_autocomplete.rows().len() as u16;
    let height = if bordered {
        row_count.saturating_add(2).min(room_above)
    } else {
        row_count.min(room_above.max(1))
    };
    let row_capacity = height.saturating_sub(if bordered { 2 } else { 0 });
    let x = frame
        .area()
        .x
        .saturating_add(frame.area().width.saturating_sub(width) / 2);
    let y = composer_area.y.saturating_sub(height).max(frame.area().y);
    let area = Rect::new(x, y, width, height);
    let content_width = width.saturating_sub(if bordered { 2 } else { 0 });
    let rows = state
        .command_autocomplete
        .visible_rows(usize::from(row_capacity))
        .map(|(selected, command)| {
            let slash = command
                .slash
                .expect("autocomplete only contains commands with slash metadata");
            let content = truncate_to_width(
                &format!(
                    "/{}  {} · {}",
                    slash.name, command.title, command.description
                ),
                usize::from(content_width),
            );
            Line::styled(
                content,
                if selected {
                    theme.selection.focused
                } else {
                    theme.text.primary
                },
            )
        })
        .collect::<Vec<_>>();
    let paragraph = if bordered {
        Paragraph::new(rows).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Commands ")
                .border_style(theme.border.default)
                .style(theme.surface.overlay),
        )
    } else {
        Paragraph::new(rows).style(theme.surface.overlay)
    };
    frame.render_widget(Clear, area);
    frame.render_widget(paragraph, area);
}

fn render_landing(
    frame: &mut Frame<'_>,
    state: &TuiState,
    slots: &RenderSlots,
    theme: &Theme,
) -> RenderedComposer {
    let detail = ResponsiveDetail::for_width(frame.area().width);
    let show_brand = frame.area().height >= LANDING_BRAND_MINIMUM_HEIGHT;
    let footer_detail = detail.secondary_only_when(show_brand);
    let footer_width = frame
        .area()
        .width
        .saturating_sub(horizontal_padding(frame.area().width).saturating_mul(2));
    let agent = agent_selection_context(state, footer_detail);
    let context = if footer_detail.shows_secondary() {
        format!("{agent} · Workspace {}", state.workspace.to_string_lossy())
    } else {
        agent
    };
    let footer = slots.home_footer(&HomeFooterSlotContext {
        width: footer_width,
        context: SlotText::new(context, theme.text.subdued),
        connection: SlotText::new(
            connection_status_text(state, footer_detail),
            status_style(state, theme),
        ),
    });
    let [main, footer_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(footer.height())])
            .areas(frame.area());
    let content = horizontally_inset(main, horizontal_padding(frame.area().width));
    let key = ComposerKey::Landing;
    let composer_text = state.composers.text(key);
    let composer_cursor = state.composers.cursor(key);
    let composer_height = composer_block_height(
        frame.area().height,
        72_u16.min(content.width),
        composer_text,
        composer_cursor,
    );
    let error_height = u16::from(state.submission_error.is_some());
    let show_question = state.submission_error.is_none()
        || main.height
            >= composer_height
                .saturating_add(error_height)
                .saturating_add(1);
    let panel_height = composer_height
        .saturating_add(u16::from(show_question))
        .saturating_add(u16::from(show_brand))
        .saturating_add(error_height);
    let panel = centered_rect(content, 72, panel_height);
    let mut row = panel.y;
    if show_brand {
        frame.render_widget(
            Paragraph::new("Chidori")
                .alignment(Alignment::Center)
                .style(theme.accent.primary.add_modifier(Modifier::BOLD)),
            Rect::new(panel.x, row, panel.width, 1),
        );
        row = row.saturating_add(1);
    }
    if show_question {
        frame.render_widget(
            Paragraph::new("What would you like to work on?").alignment(Alignment::Center),
            Rect::new(panel.x, row, panel.width, 1),
        );
        row = row.saturating_add(1);
    }
    if let Some(error) = &state.submission_error {
        frame.render_widget(
            Paragraph::new(error.as_str())
                .alignment(Alignment::Center)
                .style(theme.form_field.invalid),
            Rect::new(panel.x, row, panel.width, 1),
        );
        row = row.saturating_add(1);
    }
    let composer_area = Rect::new(panel.x, row, panel.width, composer_height);
    let cursor = render_composer(
        frame,
        composer_area,
        composer_text,
        composer_cursor,
        state.composer_border_style(theme),
        detail,
        theme,
    );

    render_slot(
        frame,
        horizontally_inset(footer_area, horizontal_padding(frame.area().width)),
        footer,
        theme,
    );
    RenderedComposer {
        area: composer_area,
        cursor,
    }
}

fn render_session(
    frame: &mut Frame<'_>,
    state: &TuiState,
    slots: &RenderSlots,
    theme: &Theme,
) -> RenderedComposer {
    let snapshot = state
        .session
        .as_ref()
        .expect("Session renderer requires a Session")
        .snapshot();
    let detail = ResponsiveDetail::for_width(frame.area().width);
    let padding = horizontal_padding(frame.area().width);
    let show_header = frame.area().height >= SESSION_HEADER_MINIMUM_HEIGHT;
    let session_id = snapshot.session.id;
    let key = ComposerKey::Session(session_id);
    let composer_text = state.composers.text(key);
    let composer_cursor = state.composers.cursor(key);
    let content_width = frame.area().width.saturating_sub(padding.saturating_mul(2));
    let desired_composer_height = composer_block_height(
        frame.area().height,
        content_width,
        composer_text,
        composer_cursor,
    );
    let composer_top = slots.session_composer_top(&SessionComposerTopSlotContext { session_id });
    let status = match snapshot.session.status {
        SessionStatus::Idle => "idle".to_owned(),
        SessionStatus::Active => {
            let interrupt = binding_label(&CommandId::RequestInterrupt);
            if matches!(
                state.command_mode,
                CommandMode::InterruptConfirmation { .. }
            ) {
                format!("active · {interrupt} again to interrupt")
            } else {
                format!("active · {interrupt} interrupt")
            }
        }
    };
    let activity_style = if snapshot.session.status == SessionStatus::Active {
        theme.feedback.warning
    } else {
        theme.text.subdued
    };
    let (agent, agent_style) = if let Some(error) = state.submission_error.as_ref() {
        (
            format!(
                "Error: {error} · {}",
                agent_selection_context(state, ResponsiveDetail::CoreOnly)
            ),
            theme.feedback.error,
        )
    } else if snapshot.session.status == SessionStatus::Active && !detail.shows_secondary() {
        (String::new(), activity_style)
    } else {
        (agent_selection_context(state, detail), activity_style)
    };
    let footer = slots.prompt_footer(
        &PromptFooterSlotContext {
            session_id,
            width: content_width,
        },
        &PromptStatusSlotContext {
            session_id,
            status: SlotText::new(status, activity_style),
        },
        &PromptContextSlotContext {
            session_id,
            agent: SlotText::new(agent, agent_style),
            connection: SlotText::new(
                connection_status_text(state, ResponsiveDetail::CoreOnly),
                status_style(state, theme),
            ),
        },
    );
    let queued_prompts = state.queued_prompts(session_id);
    let desired_pending_height = if queued_prompts.is_empty() {
        0
    } else {
        (queued_prompts.len() as u16).min(3).saturating_add(2)
    };
    let core_height = u16::from(show_header)
        .saturating_add(desired_composer_height)
        .saturating_add(composer_top.height())
        .saturating_add(footer.height())
        .saturating_add(1);
    let pending_room = frame.area().height.saturating_sub(core_height);
    let pending_height = if pending_room >= 3 {
        desired_pending_height.min(pending_room)
    } else {
        0
    };
    let reserved_height = u16::from(show_header)
        .saturating_add(pending_height)
        .saturating_add(composer_top.height())
        .saturating_add(footer.height())
        .saturating_add(1);
    let composer_height =
        desired_composer_height.min(frame.area().height.saturating_sub(reserved_height).max(1));
    let mut transcript = transcript_projection(snapshot, theme, content_width);
    for provisional in state.provisional_prompts(session_id) {
        push_user_message(
            &mut transcript.lines,
            &provisional.text,
            theme,
            content_width,
        );
    }
    transcript.split_oversized_lines(content_width);
    let transcript_layout = transcript.layout(content_width);
    let interaction = state
        .session_interaction(session_id)
        .expect("Session interaction is initialized with its snapshot");
    let [_, transcript_without_latest, _, _, _, _, _] = session_areas(
        frame.area(),
        u16::from(show_header),
        pending_height,
        0,
        composer_top.height(),
        composer_height,
        footer.height(),
    );
    let viewport_without_latest = transcript_viewport_height(transcript_without_latest);
    let [_, transcript_with_latest, _, _, _, _, _] = session_areas(
        frame.area(),
        u16::from(show_header),
        pending_height,
        1,
        composer_top.height(),
        composer_height,
        footer.height(),
    );
    let viewport_with_latest = transcript_viewport_height(transcript_with_latest);
    let (away_from_bottom, viewport_height, maximum_scroll, scroll_position) =
        if interaction.follow_latest.get() {
            let maximum_scroll = transcript_layout
                .row_count
                .saturating_sub(viewport_without_latest);
            (
                false,
                viewport_without_latest,
                maximum_scroll,
                maximum_scroll,
            )
        } else {
            let viewport_height = viewport_with_latest;
            let maximum_scroll = transcript_layout.row_count.saturating_sub(viewport_height);
            let scroll_position = interaction.anchor.get().map_or(maximum_scroll, |anchor| {
                transcript_layout
                    .message_starts
                    .iter()
                    .find(|start| start.message_id == anchor.message_id)
                    .map_or(maximum_scroll, |start| {
                        (start.row as isize)
                            .saturating_sub(anchor.screen_row)
                            .clamp(0, maximum_scroll as isize) as usize
                    })
            });
            if scroll_position >= maximum_scroll {
                interaction.follow_latest.set(true);
                interaction.anchor.set(None);
                let maximum_scroll = transcript_layout
                    .row_count
                    .saturating_sub(viewport_without_latest);
                (
                    false,
                    viewport_without_latest,
                    maximum_scroll,
                    maximum_scroll,
                )
            } else {
                (true, viewport_height, maximum_scroll, scroll_position)
            }
        };
    let latest_height = u16::from(away_from_bottom);
    let [
        header_area,
        transcript_area,
        pending_area,
        latest_area,
        composer_top_area,
        composer_area,
        status_area,
    ] = session_areas(
        frame.area(),
        u16::from(show_header),
        pending_height,
        latest_height,
        composer_top.height(),
        composer_height,
        footer.height(),
    );
    if show_header {
        render_session_header(
            frame,
            state,
            snapshot,
            horizontally_inset(header_area, padding),
            detail,
            theme,
        );
    }

    let transcript_area = horizontally_inset(transcript_area, padding);
    let pending_area = horizontally_inset(pending_area, padding);
    let latest_area = horizontally_inset(latest_area, padding);
    let composer_top_area = horizontally_inset(composer_top_area, padding);
    let composer_area = horizontally_inset(composer_area, padding);
    let footer_area = horizontally_inset(status_area, padding);
    let first_visible_line = transcript_layout
        .line_starts
        .partition_point(|row| *row <= scroll_position)
        .saturating_sub(1);
    let window_start = transcript_layout
        .line_starts
        .get(first_visible_line)
        .copied()
        .unwrap_or(0);
    let local_scroll = scroll_position
        .saturating_sub(window_start)
        .min(usize::from(u16::MAX.saturating_sub(transcript_area.height)))
        as u16;
    interaction.viewport.replace(Some(TranscriptViewport {
        height: viewport_height,
        scroll_position,
        maximum_scroll,
        message_starts: transcript_layout.message_starts,
    }));
    let transcript_widget = Paragraph::new(Text::from(
        transcript
            .lines
            .into_iter()
            .skip(first_visible_line)
            .collect::<Vec<_>>(),
    ))
    .wrap(Wrap { trim: false });
    let transcript_widget = if transcript_area.height > 1 {
        transcript_widget.block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(theme.border.subdued),
        )
    } else {
        transcript_widget
    };
    frame.render_widget(transcript_widget.scroll((local_scroll, 0)), transcript_area);
    if pending_height > 0 {
        render_pending_prompts(frame, pending_area, state, &queued_prompts, detail, theme);
    }
    if away_from_bottom {
        frame.render_widget(
            Paragraph::new(Line::styled(
                format!("Latest ↓ · {}", binding_label(&CommandId::FollowLatest)),
                theme.action.primary,
            ))
            .alignment(Alignment::Right),
            latest_area,
        );
    }
    render_slot(frame, composer_top_area, composer_top, theme);
    let cursor = render_composer(
        frame,
        composer_area,
        composer_text,
        composer_cursor,
        state.composer_border_style(theme),
        detail,
        theme,
    );
    render_slot(frame, footer_area, footer, theme);
    RenderedComposer {
        area: composer_area,
        cursor,
    }
}

fn render_session_header(
    frame: &mut Frame<'_>,
    state: &TuiState,
    snapshot: &SessionSnapshot,
    area: Rect,
    detail: ResponsiveDetail,
    theme: &Theme,
) {
    let connection = connection_status_text(state, ResponsiveDetail::CoreOnly);
    let connection_width = connection.width().min(usize::from(area.width));
    let left_width = usize::from(area.width).saturating_sub(connection_width.saturating_add(2));
    let brand = truncate_to_width("Chidori", left_width);
    let orientation_width = left_width.saturating_sub(brand.width());
    let orientation = if detail.shows_secondary() {
        truncate_to_width(
            &format!(
                " · Workspace {}",
                snapshot.session.workspace.path.to_string_lossy()
            ),
            orientation_width,
        )
    } else {
        String::new()
    };
    let spacing = " ".repeat(
        usize::from(area.width)
            .saturating_sub(brand.width() + orientation.width() + connection_width),
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(brand, theme.accent.primary.add_modifier(Modifier::BOLD)),
            Span::styled(orientation, theme.text.subdued),
            Span::raw(spacing),
            Span::styled(connection, status_style(state, theme)),
        ])),
        area,
    );
}

fn agent_selection_context(state: &TuiState, detail: ResponsiveDetail) -> String {
    match state.agent_selection() {
        None => "Agent unavailable".to_owned(),
        Some(selection) => state
            .model_picker
            .selection_summary(selection, detail.shows_secondary()),
    }
}

fn session_areas(
    area: Rect,
    header_height: u16,
    pending_height: u16,
    latest_height: u16,
    composer_top_height: u16,
    composer_height: u16,
    footer_height: u16,
) -> [Rect; 7] {
    Layout::vertical([
        Constraint::Length(header_height),
        Constraint::Min(1),
        Constraint::Length(pending_height),
        Constraint::Length(latest_height),
        Constraint::Length(composer_top_height),
        Constraint::Length(composer_height),
        Constraint::Length(footer_height),
    ])
    .areas(area)
}

fn transcript_viewport_height(area: Rect) -> usize {
    usize::from(if area.height > 1 {
        area.height.saturating_sub(1)
    } else {
        area.height
    })
}

fn render_composer(
    frame: &mut Frame<'_>,
    area: Rect,
    text: &str,
    cursor: usize,
    style: Style,
    detail: ResponsiveDetail,
    theme: &Theme,
) -> Position {
    let submit = binding_label(&CommandId::SubmitSteer);
    let queue = binding_label(&CommandId::SubmitQueue);
    let newline = binding_label(&CommandId::InsertNewline);
    let title = if detail.shows_secondary() {
        format!(" Prompt · {submit} submit · {queue} queue · {newline} newline ")
    } else {
        " Prompt ".to_owned()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(style);
    let content_width = area.width.saturating_sub(2).max(1);
    let content_height = area.height.saturating_sub(2).max(1);
    let (cursor_row, cursor_column) = visual_cursor_position(text, cursor, content_width);
    let scroll = cursor_row.saturating_sub(content_height.saturating_sub(1));
    let paragraph = if text.is_empty() {
        Paragraph::new(Span::styled(
            "Type a Prompt and press Enter",
            theme.form_field.placeholder,
        ))
    } else {
        Paragraph::new(wrapped_composer_lines(text, content_width)).style(theme.form_field.text)
    };
    frame.render_widget(paragraph.block(block).scroll((scroll, 0)), area);
    Position::new(
        area.x.saturating_add(1).saturating_add(cursor_column),
        area.y
            .saturating_add(1)
            .saturating_add(cursor_row.saturating_sub(scroll)),
    )
}

fn render_pending_prompts(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &TuiState,
    prompts: &[QueuedPrompt<'_>],
    detail: ResponsiveDetail,
    theme: &Theme,
) {
    let leader = binding_label(&CommandId::BeginLeader);
    let queue = binding_label(&CommandId::OpenQueuedPrompts);
    let managing = matches!(state.command_mode, CommandMode::QueuedPrompts { .. });
    let title = if !detail.shows_secondary() {
        " Pending ".to_owned()
    } else if managing {
        format!(
            " Pending · {} steer · {} cancel ",
            binding_label(&CommandId::PromoteSelectedPrompt),
            binding_label(&CommandId::CancelSelectedPrompt)
        )
    } else {
        format!(" Pending · {leader} {queue} manage ")
    };
    let selected = match state.command_mode {
        CommandMode::QueuedPrompts { selected } => Some(selected),
        _ => None,
    };
    let lines = prompts
        .iter()
        .map(|prompt| {
            let text = prompt.text.replace('\n', " ");
            Line::styled(
                format!(
                    "{}{}",
                    if selected == Some(prompt.id) {
                        "› "
                    } else {
                        "  "
                    },
                    text
                ),
                if selected == Some(prompt.id) {
                    theme.selection.focused
                } else {
                    theme.text.subdued
                },
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(if managing {
                    theme.border.default
                } else {
                    theme.border.subdued
                }),
        ),
        area,
    );
}

fn render_reconnect_overlay(frame: &mut Frame<'_>, theme: &Theme) {
    frame.render_widget(Block::default().style(theme.surface.overlay), frame.area());
    let area = centered_rect(frame.area(), 48, 5);
    frame.render_widget(Clear, area);
    let details = if area.width >= 42 {
        vec![
            Line::styled("Reconnecting to Chidori…", theme.feedback.warning),
            Line::default(),
            Line::styled("Your Session will resume automatically", theme.text.subdued),
        ]
    } else {
        vec![
            Line::styled("Reconnecting to Chidori…", theme.feedback.warning),
            Line::styled("Your Session will", theme.text.subdued),
            Line::styled("resume automatically", theme.text.subdued),
        ]
    };
    frame.render_widget(
        Paragraph::new(Text::from(details))
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme.border.default)
                    .style(theme.surface.elevated),
            ),
        area,
    );
}

fn composer_block_height(terminal_height: u16, width: u16, text: &str, cursor: usize) -> u16 {
    let content_width = width.saturating_sub(2).max(1);
    let cursor_rows = visual_cursor_position(text, cursor, content_width)
        .0
        .saturating_add(1);
    let desired = visual_row_count(text, content_width)
        .max(cursor_rows)
        .max(1);
    let cap = (terminal_height / 3).max(1);
    desired.min(cap).saturating_add(2)
}

fn visual_row_count(text: &str, width: u16) -> u16 {
    visual_text_end(text, width).0.saturating_add(1)
}

fn visual_cursor_position(text: &str, cursor: usize, width: u16) -> (u16, u16) {
    let width = width.max(1);
    let (row, column) = visual_text_end(&text[..cursor], width);
    if column >= width {
        (row.saturating_add(1), 0)
    } else {
        (row, column)
    }
}

fn visual_text_end(text: &str, width: u16) -> (u16, u16) {
    let width = width.max(1);
    let mut row = 0_u16;
    let mut column = 0_u16;
    for character in text.chars() {
        if character == '\n' {
            row = row.saturating_add(1);
            column = 0;
            continue;
        }
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0) as u16;
        if column > 0 && column.saturating_add(character_width) > width {
            row = row.saturating_add(1);
            column = 0;
        }
        column = column.saturating_add(character_width);
    }
    (row, column)
}

fn wrapped_composer_lines(text: &str, width: u16) -> Text<'static> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut line_width = 0_u16;
    for character in text.chars() {
        if character == '\n' {
            lines.push(Line::from(std::mem::take(&mut line)));
            line_width = 0;
            continue;
        }
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0) as u16;
        if line_width > 0 && line_width.saturating_add(character_width) > width {
            lines.push(Line::from(std::mem::take(&mut line)));
            line_width = 0;
        }
        line.push(character);
        line_width = line_width.saturating_add(character_width);
    }
    lines.push(Line::from(line));
    Text::from(lines)
}

struct TranscriptProjection {
    lines: Vec<Line<'static>>,
    message_boundaries: Vec<MessageBoundary>,
}

const MAX_TRANSCRIPT_SOURCE_LINE_ROWS: usize = 32_000;

#[derive(Clone, Copy)]
struct MessageBoundary {
    message_id: MessageId,
    line_index: usize,
}

struct TranscriptLayout {
    row_count: usize,
    message_starts: Vec<MessageStart>,
    line_starts: Vec<usize>,
}

impl TranscriptProjection {
    fn split_oversized_lines(&mut self, width: u16) {
        let original_lines = std::mem::take(&mut self.lines);
        let mut remapped_line_indices = Vec::with_capacity(original_lines.len() + 1);
        for line in original_lines {
            remapped_line_indices.push(self.lines.len());
            split_oversized_line(line, width, &mut self.lines);
        }
        remapped_line_indices.push(self.lines.len());
        for boundary in &mut self.message_boundaries {
            boundary.line_index = remapped_line_indices
                .get(boundary.line_index)
                .copied()
                .unwrap_or(self.lines.len());
        }
    }

    fn layout(&self, width: u16) -> TranscriptLayout {
        let mut row_count = 0;
        let mut boundaries = self.message_boundaries.iter().peekable();
        let mut message_starts = Vec::with_capacity(self.message_boundaries.len());
        let mut line_starts = Vec::with_capacity(self.lines.len());
        for (line_index, line) in self.lines.iter().enumerate() {
            line_starts.push(row_count);
            while boundaries
                .peek()
                .is_some_and(|boundary| boundary.line_index == line_index)
            {
                let boundary = boundaries.next().expect("peeked Message boundary exists");
                message_starts.push(MessageStart {
                    message_id: boundary.message_id,
                    row: row_count,
                });
            }
            row_count = row_count.saturating_add(wrapped_line_count(line, width));
        }
        for boundary in boundaries {
            message_starts.push(MessageStart {
                message_id: boundary.message_id,
                row: row_count,
            });
        }
        TranscriptLayout {
            row_count,
            message_starts,
            line_starts,
        }
    }
}

fn wrapped_line_count(line: &Line<'static>, width: u16) -> usize {
    Paragraph::new(line.clone())
        .wrap(Wrap { trim: false })
        .line_count(width)
}

fn split_oversized_line(line: Line<'static>, width: u16, output: &mut Vec<Line<'static>>) {
    if wrapped_line_count(&line, width) <= MAX_TRANSCRIPT_SOURCE_LINE_ROWS {
        output.push(line);
        return;
    }
    let character_count = line
        .spans
        .iter()
        .map(|span| span.content.chars().count())
        .sum::<usize>();
    if character_count < 2 {
        output.push(line);
        return;
    }
    let (left, right) = split_line_at_character_midpoint(line, character_count);
    split_oversized_line(left, width, output);
    split_oversized_line(right, width, output);
}

fn split_line_at_character_midpoint(
    line: Line<'static>,
    character_count: usize,
) -> (Line<'static>, Line<'static>) {
    let Line {
        style,
        alignment,
        spans,
    } = line;
    let mut remaining_left = character_count / 2;
    let mut left_spans = Vec::new();
    let mut right_spans = Vec::new();
    for span in spans {
        if remaining_left == 0 {
            right_spans.push(span);
            continue;
        }
        let span_character_count = span.content.chars().count();
        if span_character_count <= remaining_left {
            remaining_left -= span_character_count;
            left_spans.push(span);
            continue;
        }

        let content = span.content.into_owned();
        let split_byte = content
            .char_indices()
            .nth(remaining_left)
            .map_or(content.len(), |(index, _)| index);
        let (left, right) = content.split_at(split_byte);
        if !left.is_empty() {
            left_spans.push(Span::styled(left.to_owned(), span.style));
        }
        if !right.is_empty() {
            right_spans.push(Span::styled(right.to_owned(), span.style));
        }
        remaining_left = 0;
    }

    (
        Line {
            style,
            alignment,
            spans: left_spans,
        },
        Line {
            style,
            alignment,
            spans: right_spans,
        },
    )
}

fn transcript_projection(
    snapshot: &SessionSnapshot,
    theme: &Theme,
    available_width: u16,
) -> TranscriptProjection {
    let mut lines = Vec::new();
    let mut message_boundaries = Vec::new();
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
                message_boundaries.push(MessageBoundary {
                    message_id: *message_id,
                    line_index: lines.len(),
                });
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
                    .find(|activity| activity.id() == *activity_id)
                else {
                    continue;
                };
                match activity {
                    Activity::Status { text, .. } => {
                        push_prefixed_lines(&mut lines, "  ", text, theme.text.subdued)
                    }
                    Activity::Error { text, .. } => {
                        push_prefixed_lines(&mut lines, "  Error: ", text, theme.feedback.error)
                    }
                    Activity::Command {
                        status,
                        command,
                        cwd,
                        output,
                        exit_status,
                        ..
                    } => push_command_activity(
                        &mut lines,
                        *status,
                        command,
                        cwd.as_deref(),
                        output,
                        *exit_status,
                        theme,
                    ),
                    Activity::FileChange {
                        status, changes, ..
                    } => push_file_change_activity(&mut lines, *status, changes, theme),
                }
            }
        }
    }
    TranscriptProjection {
        lines,
        message_boundaries,
    }
}

fn push_command_activity(
    lines: &mut Vec<Line<'static>>,
    status: crate::protocol::ActivityStatus,
    command: &str,
    cwd: Option<&std::path::Path>,
    output: &str,
    exit_status: Option<i32>,
    theme: &Theme,
) {
    use crate::protocol::ActivityStatus;

    let (marker, style) = match status {
        ActivityStatus::Active => ("$ ", theme.accent.primary),
        ActivityStatus::Completed => ("✓ ", theme.feedback.success),
        ActivityStatus::Failed => ("× ", theme.feedback.error),
    };
    let command = match (status, exit_status) {
        (ActivityStatus::Failed, Some(exit_status)) => {
            format!("{command} (exit {exit_status})")
        }
        _ => command.to_owned(),
    };
    push_prefixed_lines(lines, &format!("  {marker}"), &command, style);
    if let Some(cwd) = cwd {
        push_prefixed_lines(
            lines,
            "    in ",
            cwd.to_string_lossy().as_ref(),
            theme.text.subdued,
        );
    }
    if !output.is_empty() {
        push_prefixed_lines(lines, "    ", output, theme.text.subdued);
    }
}

fn push_file_change_activity(
    lines: &mut Vec<Line<'static>>,
    status: crate::protocol::ActivityStatus,
    changes: &[FileChange],
    theme: &Theme,
) {
    use crate::protocol::ActivityStatus;

    let (marker, label, style) = match status {
        ActivityStatus::Active => ("… ", "Applying file changes", theme.accent.primary),
        ActivityStatus::Completed => ("✓ ", "Applied file changes", theme.feedback.success),
        ActivityStatus::Failed => ("× ", "Failed to apply file changes", theme.feedback.error),
    };
    push_prefixed_lines(lines, &format!("  {marker}"), label, style);
    for change in changes {
        let summary = match change {
            FileChange::Add { path } => format!("A {}", path.to_string_lossy()),
            FileChange::Delete { path } => format!("D {}", path.to_string_lossy()),
            FileChange::Update {
                path,
                moved_to: Some(moved_to),
            } => format!(
                "R {} → {}",
                path.to_string_lossy(),
                moved_to.to_string_lossy()
            ),
            FileChange::Update {
                path,
                moved_to: None,
            } => format!("M {}", path.to_string_lossy()),
        };
        push_prefixed_lines(lines, "    ", &summary, theme.text.subdued);
    }
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
    let mut reconnect_grace: Option<Pin<Box<tokio::time::Sleep>>> = None;
    let (submission_tx, mut submission_rx) = tokio::sync::mpsc::unbounded_channel();
    let (subscription_tx, mut subscription_rx) = tokio::sync::mpsc::unbounded_channel();
    let (picker_tx, mut picker_rx) = tokio::sync::mpsc::unbounded_channel();
    let (model_tx, mut model_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut session_list_task: Option<(SessionListRequest, tokio::task::JoinHandle<()>)> = None;
    let mut session_attachment_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut model_list_task: Option<(ModelListRequest, tokio::task::JoinHandle<()>)> = None;

    loop {
        terminal.draw(|frame| application.render(frame))?;
        tokio::select! {
            managed_event = client.next() => {
                match managed_event {
                    Some(event) => {
                        let was_recovering = application.is_recovering();
                        let transition = application
                            .handle_event(ApplicationEvent::Managed(event))?;
                        let is_recovering = application.is_recovering();
                        if !was_recovering && is_recovering {
                            reconnect_grace = Some(Box::pin(tokio::time::sleep(
                                RECONNECT_GRACE_PERIOD,
                            )));
                        } else if !is_recovering {
                            reconnect_grace = None;
                        }
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
                            | ApplicationTransition::DetachSession
                            | ApplicationTransition::DeleteSession(_)
                            | ApplicationTransition::AdmitPrompt { .. }
                            | ApplicationTransition::PromotePrompt { .. }
                            | ApplicationTransition::CancelPrompt { .. }
                            | ApplicationTransition::InterruptTurn { .. }
                            | ApplicationTransition::SubscribeSession(_)
                            | ApplicationTransition::AttachSession(_)
                            | ApplicationTransition::ListSessions(_)
                            | ApplicationTransition::ListModels(_)
                            | ApplicationTransition::ConfirmLandingAgentSelection(_)
                            | ApplicationTransition::UpdateAgentSelection { .. } => {
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
            _ = wait_for_reconnect_grace(&mut reconnect_grace) => {
                application.handle_event(ApplicationEvent::ReconnectGraceElapsed)?;
                reconnect_grace = None;
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
                    SubmissionResult::OperationSucceeded => {}
                    SubmissionResult::OperationFailed(error) => {
                        application.handle_event(ApplicationEvent::SessionOperationFailed(error))?;
                    }
                    SubmissionResult::SessionDeletionFailed { session_id, error } => {
                        application.handle_event(ApplicationEvent::SessionDeletionFailed {
                            session_id,
                            error,
                        })?;
                    }
                    SubmissionResult::LandingAgentSelectionConfirmed(selection) => {
                        let transition = application.handle_event(
                            ApplicationEvent::LandingAgentSelectionConfirmed(selection),
                        )?;
                        flush_landing_agent_selection(&client, transition, &submission_tx);
                    }
                    SubmissionResult::LandingAgentSelectionConfirmationFailed(error) => {
                        let transition = application.handle_event(
                            ApplicationEvent::LandingAgentSelectionConfirmationFailed(error),
                        )?;
                        flush_landing_agent_selection(&client, transition, &submission_tx);
                    }
                    SubmissionResult::AgentSelectionUpdated {
                        operation_id,
                        selection,
                    } => {
                        let transition = application.handle_event(
                            ApplicationEvent::AgentSelectionUpdated {
                                operation_id,
                                selection,
                            },
                        )?;
                        flush_agent_selection(&client, transition, &submission_tx);
                    }
                    SubmissionResult::AgentSelectionUpdateFailed {
                        operation_id,
                        error,
                    } => {
                        let transition = application.handle_event(
                            ApplicationEvent::AgentSelectionUpdateFailed {
                                operation_id,
                                error,
                            },
                        )?;
                        flush_agent_selection(&client, transition, &submission_tx);
                    }
                }
            }
            model = model_rx.recv() => {
                let Some(model) = model else {
                    return Err(anyhow!("Model picker task channel stopped unexpectedly"));
                };
                match model {
                    ModelPickerResult::Listed { request, catalog } => {
                        application.handle_event(ApplicationEvent::ModelsListed {
                            request,
                            catalog,
                        })?;
                    }
                    ModelPickerResult::Refreshed { request, catalog } => {
                        finish_listing(&mut model_list_task, &request);
                        application.handle_event(ApplicationEvent::ModelsRefreshed {
                            request,
                            catalog,
                        })?;
                    }
                    ModelPickerResult::Failed { request, error } => {
                        finish_listing(&mut model_list_task, &request);
                        application.handle_event(ApplicationEvent::ModelListingFailed {
                            request,
                            error,
                        })?;
                    }
                }
            }
            picker = picker_rx.recv() => {
                let Some(picker) = picker else {
                    return Err(anyhow!("Session picker task channel stopped unexpectedly"));
                };
                match picker {
                    SessionPickerResult::Listed { request, sessions } => {
                        finish_listing(&mut session_list_task, &request);
                        application.handle_event(ApplicationEvent::SessionsListed {
                            request,
                            sessions,
                        })?;
                    }
                    SessionPickerResult::ListingFailed { request, error } => {
                        finish_listing(&mut session_list_task, &request);
                        application.handle_event(ApplicationEvent::SessionListingFailed {
                            request,
                            error,
                        })?;
                    }
                    SessionPickerResult::Attached {
                        snapshot,
                        subscription,
                    } => {
                        session_attachment_task = None;
                        application.handle_event(ApplicationEvent::SessionAttached(*snapshot))?;
                        session_subscription = Some(subscription);
                        if let Some((_, task)) = session_subscription_task.take() {
                            task.abort();
                        }
                    }
                    SessionPickerResult::AttachmentFailed(error) => {
                        session_attachment_task = None;
                        let transition = application.handle_event(
                            ApplicationEvent::SessionAttachmentFailed(error),
                        )?;
                        if let ApplicationTransition::ListSessions(request) = transition {
                            replace_session_listing(
                                &mut session_list_task,
                                client.session_commands(),
                                request,
                                picker_tx.clone(),
                            );
                        }
                    }
                }
            }
            input_event = input.next() => {
                match input_event {
                    Some(Ok(event)) => {
                            let transition = application.handle_terminal_event(event)?;
                            match transition {
                                ApplicationTransition::Continue => {}
                                ApplicationTransition::SessionEnded => {
                                    session_subscription = None;
                                }
                                ApplicationTransition::DetachSession => {
                                    session_subscription = None;
                                    if let Some((_, task)) = session_subscription_task.take() {
                                        task.abort();
                                    }
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
                                ApplicationTransition::PromotePrompt { session_id, prompt_id } => {
                                    spawn_session_operation(
                                        client.session_commands(),
                                        SessionOperation::PromotePrompt { session_id, prompt_id },
                                        submission_tx.clone(),
                                    );
                                }
                                ApplicationTransition::CancelPrompt { session_id, prompt_id } => {
                                    spawn_session_operation(
                                        client.session_commands(),
                                        SessionOperation::CancelPrompt { session_id, prompt_id },
                                        submission_tx.clone(),
                                    );
                                }
                                ApplicationTransition::InterruptTurn { session_id, turn_id } => {
                                    spawn_session_operation(
                                        client.session_commands(),
                                        SessionOperation::InterruptTurn { session_id, turn_id },
                                        submission_tx.clone(),
                                    );
                                }
                                ApplicationTransition::DeleteSession(session_id) => {
                                    spawn_session_operation(
                                        client.session_commands(),
                                        SessionOperation::DeleteSession { session_id },
                                        submission_tx.clone(),
                                    );
                                }
                                ApplicationTransition::SubscribeSession(_) => {
                                    unreachable!("terminal input cannot end a Session subscription")
                                }
                                ApplicationTransition::AttachSession(session_id) => {
                                    if session_attachment_task.is_none() {
                                        session_attachment_task = Some(spawn_session_attachment(
                                            client.session_commands(),
                                            session_id,
                                            picker_tx.clone(),
                                        ));
                                    }
                                }
                                ApplicationTransition::ListSessions(request) => {
                                    replace_session_listing(
                                        &mut session_list_task,
                                        client.session_commands(),
                                        request,
                                        picker_tx.clone(),
                                    );
                                }
                                ApplicationTransition::ListModels(request) => {
                                    replace_listing(
                                        &mut model_list_task,
                                        request,
                                        |request| spawn_model_listing(
                                            client.session_commands(),
                                            request,
                                            model_tx.clone(),
                                        ),
                                    );
                                }
                                ApplicationTransition::ConfirmLandingAgentSelection(selection) => {
                                    spawn_landing_agent_selection_confirmation(
                                        client.session_commands(),
                                        selection,
                                        submission_tx.clone(),
                                    );
                                }
                                ApplicationTransition::UpdateAgentSelection {
                                    session_id,
                                    request,
                                } => {
                                    spawn_agent_selection_update(
                                        client.session_commands(),
                                        session_id,
                                        request,
                                        submission_tx.clone(),
                                    );
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
    Failed {
        prompt_id: PromptId,
        error: String,
    },
    OperationSucceeded,
    OperationFailed(String),
    SessionDeletionFailed {
        session_id: SessionId,
        error: String,
    },
    LandingAgentSelectionConfirmed(AgentSelection),
    LandingAgentSelectionConfirmationFailed(String),
    AgentSelectionUpdated {
        operation_id: AgentSelectionOperationId,
        selection: AgentSelection,
    },
    AgentSelectionUpdateFailed {
        operation_id: AgentSelectionOperationId,
        error: String,
    },
}

enum ModelPickerResult {
    Listed {
        request: ModelListRequest,
        catalog: ModelCatalog,
    },
    Refreshed {
        request: ModelListRequest,
        catalog: ModelCatalog,
    },
    Failed {
        request: ModelListRequest,
        error: String,
    },
}

fn spawn_model_listing(
    commands: SessionCommandClient,
    request: ModelListRequest,
    results: tokio::sync::mpsc::UnboundedSender<ModelPickerResult>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let listing_error = match commands.list_models().await {
            Ok(catalog) => {
                if results
                    .send(ModelPickerResult::Listed {
                        request: request.clone(),
                        catalog,
                    })
                    .is_err()
                {
                    return;
                }
                None
            }
            Err(error) => Some(error.to_string()),
        };
        let result = match commands.refresh_models().await {
            Ok(catalog) => ModelPickerResult::Refreshed { request, catalog },
            Err(error) => ModelPickerResult::Failed {
                request,
                error: listing_error.map_or_else(
                    || error.to_string(),
                    |listing| format!("{listing}; refresh failed: {error}"),
                ),
            },
        };
        let _ = results.send(result);
    })
}

/// Dispatches the follow-up request when settling one Agent Selection
/// operation released a coalesced newer selection.
fn flush_agent_selection(
    client: &ManagedClient,
    transition: ApplicationTransition,
    results: &tokio::sync::mpsc::UnboundedSender<SubmissionResult>,
) {
    if let ApplicationTransition::UpdateAgentSelection {
        session_id,
        request,
    } = transition
    {
        spawn_agent_selection_update(
            client.session_commands(),
            session_id,
            request,
            results.clone(),
        );
    }
}

fn flush_landing_agent_selection(
    client: &ManagedClient,
    transition: ApplicationTransition,
    results: &tokio::sync::mpsc::UnboundedSender<SubmissionResult>,
) {
    if let ApplicationTransition::ConfirmLandingAgentSelection(selection) = transition {
        spawn_landing_agent_selection_confirmation(
            client.session_commands(),
            selection,
            results.clone(),
        );
    }
}

fn spawn_landing_agent_selection_confirmation(
    commands: SessionCommandClient,
    selection: AgentSelection,
    results: tokio::sync::mpsc::UnboundedSender<SubmissionResult>,
) {
    tokio::spawn(async move {
        let result = match commands.confirm_landing_agent_selection(selection).await {
            Ok(selection) => SubmissionResult::LandingAgentSelectionConfirmed(selection),
            Err(error) => {
                SubmissionResult::LandingAgentSelectionConfirmationFailed(error.to_string())
            }
        };
        let _ = results.send(result);
    });
}

fn spawn_agent_selection_update(
    commands: SessionCommandClient,
    session_id: SessionId,
    request: UpdateAgentSelectionRequest,
    results: tokio::sync::mpsc::UnboundedSender<SubmissionResult>,
) {
    tokio::spawn(async move {
        let operation_id = request.operation_id;
        let result = match commands.update_agent_selection(session_id, request).await {
            Ok(selection) => SubmissionResult::AgentSelectionUpdated {
                operation_id,
                selection,
            },
            Err(error) => SubmissionResult::AgentSelectionUpdateFailed {
                operation_id,
                error: error.to_string(),
            },
        };
        let _ = results.send(result);
    });
}

enum SessionPickerResult {
    Listed {
        request: SessionListRequest,
        sessions: Vec<SessionListItem>,
    },
    ListingFailed {
        request: SessionListRequest,
        error: String,
    },
    Attached {
        snapshot: Box<SessionSnapshot>,
        subscription: SessionSubscription,
    },
    AttachmentFailed(String),
}

fn replace_session_listing(
    active: &mut Option<(SessionListRequest, tokio::task::JoinHandle<()>)>,
    commands: SessionCommandClient,
    request: SessionListRequest,
    results: tokio::sync::mpsc::UnboundedSender<SessionPickerResult>,
) {
    replace_listing(active, request, |request| {
        spawn_session_listing(commands, request, results)
    });
}

fn replace_listing<Request: Clone>(
    active: &mut Option<(Request, tokio::task::JoinHandle<()>)>,
    request: Request,
    spawn: impl FnOnce(Request) -> tokio::task::JoinHandle<()>,
) {
    if let Some((_, task)) = active.take() {
        task.abort();
    }
    let task = spawn(request.clone());
    *active = Some((request, task));
}

fn finish_listing<Request: PartialEq>(
    active: &mut Option<(Request, tokio::task::JoinHandle<()>)>,
    completed: &Request,
) {
    if active
        .as_ref()
        .is_some_and(|(request, _)| request == completed)
    {
        *active = None;
    }
}

fn spawn_session_listing(
    commands: SessionCommandClient,
    request: SessionListRequest,
    results: tokio::sync::mpsc::UnboundedSender<SessionPickerResult>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let result = match commands
            .list_sessions(request.scope.workspace_filter())
            .await
        {
            Ok(sessions) => SessionPickerResult::Listed { request, sessions },
            Err(error) => SessionPickerResult::ListingFailed {
                request,
                error: error.to_string(),
            },
        };
        let _ = results.send(result);
    })
}

fn spawn_session_attachment(
    commands: SessionCommandClient,
    session_id: SessionId,
    results: tokio::sync::mpsc::UnboundedSender<SessionPickerResult>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let result = async {
            let mut subscription = commands.attach_session(session_id).await?;
            let event = subscription
                .next()
                .await
                .ok_or_else(|| anyhow!("target Session subscription ended before hydration"))??;
            let SessionEvent::Snapshot(snapshot) = event else {
                return Err(anyhow!("target Session updated before hydration"));
            };
            Ok::<_, anyhow::Error>(SessionPickerResult::Attached {
                snapshot: Box::new(snapshot),
                subscription,
            })
        }
        .await
        .unwrap_or_else(|error| SessionPickerResult::AttachmentFailed(error.to_string()));
        let _ = results.send(result);
    })
}

enum SessionOperation {
    DeleteSession {
        session_id: SessionId,
    },
    PromotePrompt {
        session_id: SessionId,
        prompt_id: PromptId,
    },
    CancelPrompt {
        session_id: SessionId,
        prompt_id: PromptId,
    },
    InterruptTurn {
        session_id: SessionId,
        turn_id: TurnId,
    },
}

impl SessionOperation {
    async fn run(self, commands: SessionCommandClient) -> SubmissionResult {
        match self {
            Self::DeleteSession { session_id } => match commands.delete_session(session_id).await {
                Ok(()) => SubmissionResult::OperationSucceeded,
                Err(error) => SubmissionResult::SessionDeletionFailed {
                    session_id,
                    error: error.to_string(),
                },
            },
            Self::PromotePrompt {
                session_id,
                prompt_id,
            } => operation_result(
                commands
                    .promote_prompt(session_id, prompt_id)
                    .await
                    .map(|_| ()),
            ),
            Self::CancelPrompt {
                session_id,
                prompt_id,
            } => operation_result(
                commands
                    .cancel_prompt(session_id, prompt_id)
                    .await
                    .map(|_| ()),
            ),
            Self::InterruptTurn {
                session_id,
                turn_id,
            } => operation_result(
                commands
                    .interrupt_turn(session_id, turn_id)
                    .await
                    .map(|_| ()),
            ),
        }
    }
}

fn operation_result(result: anyhow::Result<()>) -> SubmissionResult {
    match result {
        Ok(()) => SubmissionResult::OperationSucceeded,
        Err(error) => SubmissionResult::OperationFailed(error.to_string()),
    }
}

fn spawn_session_operation(
    commands: SessionCommandClient,
    operation: SessionOperation,
    results: tokio::sync::mpsc::UnboundedSender<SubmissionResult>,
) {
    tokio::spawn(async move {
        let result = operation.run(commands).await;
        let _ = results.send(result);
    });
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

async fn wait_for_reconnect_grace(grace: &mut Option<Pin<Box<tokio::time::Sleep>>>) {
    match grace {
        Some(grace) => grace.as_mut().await,
        None => pending().await,
    }
}

fn terminal_is_too_small(area: Rect) -> bool {
    area.width < MINIMUM_TERMINAL_WIDTH || area.height < MINIMUM_TERMINAL_HEIGHT
}

fn render_terminal_too_small(frame: &mut Frame<'_>, theme: &Theme) {
    let full_message = "Terminal too small";
    let message = if frame.area().width < full_message.width() as u16 {
        "Too small"
    } else {
        full_message
    };
    let message_height = if frame.area().width < message.width() as u16 {
        2
    } else {
        1
    };
    let area = centered_rect(frame.area(), frame.area().width, message_height);
    frame.render_widget(
        Paragraph::new(message)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .style(theme.feedback.warning),
        area,
    );
}

fn horizontal_padding(width: u16) -> u16 {
    if width < NARROW_TERMINAL_WIDTH { 1 } else { 2 }
}

fn horizontally_inset(area: Rect, padding: u16) -> Rect {
    let padding = padding.min(area.width / 2);
    Rect::new(
        area.x.saturating_add(padding),
        area.y,
        area.width.saturating_sub(padding.saturating_mul(2)),
        area.height,
    )
}

fn render_slot(
    frame: &mut Frame<'_>,
    area: Rect,
    slot: RenderedSlot<Line<'static>>,
    theme: &Theme,
) {
    let rows = slot
        .failures
        .into_iter()
        .map(|failure| {
            Line::styled(
                format!("Extension error · {}: {}", failure.slot, failure.message),
                theme.feedback.error,
            )
        })
        .chain(slot.content)
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(rows), area);
}

fn connection_status_text(state: &TuiState, detail: ResponsiveDetail) -> String {
    if detail.shows_secondary() {
        return status_text(state);
    }
    if state.fatal_error.is_some() {
        "Connection failed".to_owned()
    } else if state.manually_stopped {
        "Server stopped".to_owned()
    } else if state.recovery.is_some() {
        "Recovering".to_owned()
    } else if state.identity.is_some() {
        "Connected".to_owned()
    } else {
        "Connecting".to_owned()
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
    let connection = match &state.identity {
        Some(identity) => format!("Connected | {}", server_identity_text(identity)),
        None => "Connecting to Chidori server...".to_owned(),
    };
    if matches!(
        state
            .session
            .as_ref()
            .map(|session| session.snapshot().session.status),
        Some(SessionStatus::Active)
    ) {
        let interrupt = binding_label(&CommandId::RequestInterrupt);
        let active = if matches!(
            state.command_mode,
            CommandMode::InterruptConfirmation { .. }
        ) {
            format!("Active · {interrupt} again to interrupt")
        } else {
            format!("Active · {interrupt} interrupt")
        };
        return format!("{active} | {connection}");
    }
    connection
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ratatui::{
        Terminal,
        backend::TestBackend,
        buffer::{Buffer, Cell},
        style::{Color, Style},
    };

    use super::slots::{Placement, RenderSlots, TestContribution};
    use super::{Application, ApplicationEvent, SessionEvent};
    use crate::protocol::{
        ModelAvailability, Session, SessionId, SessionRevision, SessionSnapshot, SessionStatus,
        Workspace,
    };

    fn rendered_buffer(application: &Application) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).expect("create test terminal");
        terminal
            .draw(|frame| application.render(frame))
            .expect("render headless TUI application");
        terminal.backend().buffer().clone()
    }

    fn rendered_rows(application: &Application) -> Vec<String> {
        let buffer = rendered_buffer(application);
        buffer
            .content()
            .chunks(buffer.area.width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect()
    }

    fn text_cell<'a>(buffer: &'a Buffer, needle: &str) -> &'a Cell {
        for (y, row) in rendered_rows_from_buffer(buffer).into_iter().enumerate() {
            if let Some(byte_offset) = row.find(needle) {
                let x = row[..byte_offset].chars().count() as u16;
                return buffer.cell((x, y as u16)).expect("text cell is in bounds");
            }
        }
        panic!("rendered frame did not contain {needle:?}");
    }

    fn rendered_rows_from_buffer(buffer: &Buffer) -> Vec<String> {
        buffer
            .content()
            .chunks(buffer.area.width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect()
    }

    #[test]
    fn named_slots_compose_in_order_and_isolate_failed_contributions() {
        let slots = RenderSlots::testing([
            TestContribution::home_footer(Placement::Prepend, Ok("prepend")),
            TestContribution::home_footer(Placement::Replace, Err("replacement failed")),
            TestContribution::home_footer(Placement::Append, Ok("append one")),
            TestContribution::home_footer(Placement::Append, Ok("append two")),
        ]);
        let application = Application {
            slots,
            ..Application::default()
        };

        let rows = rendered_rows(&application);
        let prepend = rows.iter().position(|row| row.contains("prepend")).unwrap();
        let default = rows
            .iter()
            .position(|row| row.contains("Agent unavailable"))
            .unwrap();
        let append_one = rows
            .iter()
            .position(|row| row.contains("append one"))
            .unwrap();
        let append_two = rows
            .iter()
            .position(|row| row.contains("append two"))
            .unwrap();
        let failure = rows
            .iter()
            .position(|row| row.contains("Extension error · home.footer"))
            .unwrap();

        assert!(prepend < default);
        assert!(default < append_one);
        assert!(append_one < append_two);
        assert!(failure < prepend);

        let slots = RenderSlots::testing([
            TestContribution::home_footer(Placement::Prepend, Ok("prepend")),
            TestContribution::home_footer(Placement::Replace, Ok("replacement one")),
            TestContribution::home_footer(Placement::Replace, Ok("replacement two")),
            TestContribution::home_footer(Placement::Append, Ok("append")),
        ]);
        let application = Application {
            slots,
            ..Application::default()
        };
        let screen = rendered_rows(&application).join("\n");
        assert!(screen.contains("prepend"));
        assert!(screen.contains("replacement two"));
        assert!(screen.contains("append"));
        assert!(!screen.contains("replacement one"));
        assert!(!screen.contains("Agent unavailable"));
    }

    #[test]
    fn session_slots_render_with_their_typed_session_context() {
        let session_id = SessionId::new();
        let slots = RenderSlots::testing([
            TestContribution::session_composer_top(Placement::Append, Ok("composer top")),
            TestContribution::prompt_footer_status(Placement::Prepend, Ok("status extension"))
                .styled(Style::default().fg(Color::LightMagenta)),
            TestContribution::prompt_footer_status(Placement::Append, Err("status failed")),
            TestContribution::prompt_footer_context(Placement::Append, Ok("context extension")),
            TestContribution::prompt_footer(Placement::Append, Ok("footer extension")),
        ]);
        let mut application = Application {
            slots,
            ..Application::default()
        };
        application
            .handle_event(ApplicationEvent::Session(SessionEvent::Snapshot(
                SessionSnapshot {
                    session: Session {
                        id: session_id,
                        workspace: Workspace {
                            path: PathBuf::from("/workspace"),
                        },
                        agent_selection: None,
                        agent_selection_availability: ModelAvailability::Available,
                        status: SessionStatus::Idle,
                    },
                    revision: SessionRevision::INITIAL,
                    prompts: Vec::new(),
                    turns: Vec::new(),
                    messages: Vec::new(),
                    activities: Vec::new(),
                    transcript: Vec::new(),
                },
            )))
            .expect("hydrate test Application");

        let buffer = rendered_buffer(&application);
        let screen = rendered_rows_from_buffer(&buffer).join("\n");
        assert!(screen.contains("composer top"));
        assert!(screen.contains("status extension · idle"));
        assert!(screen.contains("Agent unavailable · context extension"));
        assert!(screen.contains("footer extension"));
        assert!(screen.contains("Extension error · prompt.footer.status: status failed"));
        assert_eq!(
            text_cell(&buffer, "status extension").fg,
            Color::LightMagenta
        );
        assert_ne!(text_cell(&buffer, "idle").fg, Color::LightMagenta);
    }
}
