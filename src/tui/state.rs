//! Application and view state: the Session projection the TUI renders, the
//! events it accepts, and the transitions it asks the runtime to perform.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::{Result, anyhow};
use crossterm::event::Event as InputEvent;
use ratatui::{Frame, style::Style};

use crate::{
    managed_client::{ManagedEvent, RecoveryStatus, SessionEvent, SessionProjection},
    protocol::{
        AdmitPromptRequest, AgentSelection, AgentSelectionOperationId, CreateSessionRequest,
        InitialPrompt, MessageId, ModelCatalog, PromptDelivery, PromptId, PromptStatus,
        ServerIdentity, SessionChange, SessionId, SessionListItem, SessionSnapshot,
        ShutdownReason,
        TurnId, TurnStatus, UpdateAgentSelectionRequest, Workspace,
    },
    theme::Theme,
};

use super::{
    commands::{CommandAutocomplete, SemanticCommandId},
    composer::{ComposerKey, ComposerMemory},
    keymap::{
        command_for_autocomplete_event, command_for_interrupt_confirmation_event,
        command_for_model_options_event, command_for_model_picker_event,
        command_for_queued_prompt_event, command_for_session_picker_event,
        command_for_leader_event, command_for_terminal_event,
    },
    model_options::{ModelOptions, ReasoningCycle, cycle_reasoning_effort},
    model_picker::{ModelPicker, ModelPickerAction},
    render::render_with_slots,
    session_picker::SessionPicker,
    slots::RenderSlots,
    transcript::{MessageStart, TranscriptCache},
};

/// Rows scrolled per mouse wheel tick, matching common terminal conventions.
const WHEEL_SCROLL_ROWS: usize = 3;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionListScope {
    CurrentWorkspace(PathBuf),
    AllWorkspaces,
}

impl SessionListScope {
    pub(super) fn workspace_filter(&self) -> Option<&Path> {
        match self {
            Self::CurrentWorkspace(workspace) => Some(workspace),
            Self::AllWorkspaces => None,
        }
    }

    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::CurrentWorkspace(_) => "Current Workspace",
            Self::AllWorkspaces => "All Workspaces",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionListRequest {
    id: u64,
    pub(super) scope: SessionListScope,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelListRequest {
    sequence: u64,
}

impl ModelListRequest {
    pub(super) const fn new(sequence: u64) -> Self {
        Self { sequence }
    }
}

impl SessionListRequest {
    pub(super) fn new(id: u64, scope: SessionListScope) -> Self {
        Self { id, scope }
    }

    pub fn scope(&self) -> &SessionListScope {
        &self.scope
    }
}



#[derive(Clone, Debug)]
pub(super) struct SessionInteraction {
    pub(super) follow_latest: Cell<bool>,
    pub(super) anchor: Cell<Option<TranscriptAnchor>>,
    /// Rendering records the latest terminal geometry so semantic page commands can use it.
    pub(super) viewport: RefCell<Option<TranscriptViewport>>,
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
pub(super) struct TranscriptAnchor {
    pub(super) message_id: MessageId,
    pub(super) screen_row: isize,
}

#[derive(Clone, Debug)]
pub(super) struct TranscriptViewport {
    pub(super) height: usize,
    pub(super) scroll_position: usize,
    pub(super) maximum_scroll: usize,
    pub(super) message_starts: Vec<MessageStart>,
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
    pub(super) identity: Option<ServerIdentity>,
    pub(super) recovery: Option<RecoveryStatus>,
    /// Manual stop preserves the last confirmed identity as useful final context.
    pub(super) manually_stopped: bool,
    pub(super) fatal_error: Option<String>,
    pub(super) workspace: PathBuf,
    pub(super) composers: ComposerMemory,
    session_interactions: HashMap<SessionId, SessionInteraction>,
    pub(super) transcript_cache: TranscriptCache,
    /// Bumped whenever the Session projection is replaced wholesale, so the
    /// transcript cache never trusts a revision across snapshot swaps.
    pub(super) transcript_generation: u64,
    pub(super) composer_focused: bool,
    pub(super) submission_error: Option<String>,
    pub(super) session: Option<SessionProjection>,
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
    pub(super) reconnect_overlay_visible: bool,
    pending_submission: Option<PendingSubmission>,
    failed_submissions: HashMap<PromptId, FailedSubmission>,
    pending_steers: Vec<PendingSteer>,
    pub(super) command_mode: CommandMode,
    pub(super) command_autocomplete: CommandAutocomplete,
    pending_model_options: bool,
    pub(super) model_options: ModelOptions,
    pub(super) model_picker: ModelPicker,
    pub(super) session_picker: SessionPicker,
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
pub(super) enum CommandMode {
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
    pub(super) fn new(workspace: impl AsRef<Path>) -> Self {
        let workspace = workspace.as_ref().to_owned();
        Self {
            identity: None,
            recovery: None,
            manually_stopped: false,
            fatal_error: None,
            workspace: workspace.clone(),
            composers: ComposerMemory::default(),
            session_interactions: HashMap::new(),
            transcript_cache: TranscriptCache::default(),
            transcript_generation: 0,
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
        self.transcript_generation = self.transcript_generation.wrapping_add(1);
        self.sync_command_autocomplete();
    }

    fn composer_key(&self) -> ComposerKey {
        self.session
            .as_ref()
            .map_or(ComposerKey::Landing, |session| {
                ComposerKey::Session(session.session_id())
            })
    }

    pub(super) fn agent_selection(&self) -> Option<&AgentSelection> {
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

    pub(super) fn session_interaction(&self, session_id: SessionId) -> Option<&SessionInteraction> {
        self.session_interactions.get(&session_id)
    }

    fn navigate_transcript_page(&mut self, direction: TranscriptDirection) {
        self.navigate_transcript(direction, None);
    }

    fn navigate_transcript_lines(&mut self, direction: TranscriptDirection) {
        self.navigate_transcript(direction, Some(WHEEL_SCROLL_ROWS));
    }

    fn navigate_transcript(&mut self, direction: TranscriptDirection, rows: Option<usize>) {
        let Some(session_id) = self.session.as_ref().map(SessionProjection::session_id) else {
            return;
        };
        let Some(interaction) = self.session_interactions.get_mut(&session_id) else {
            return;
        };
        let Some(viewport) = interaction.viewport.get_mut().as_mut() else {
            return;
        };
        let step = rows.unwrap_or(viewport.height).max(1);
        let target = match direction {
            TranscriptDirection::Up => viewport.scroll_position.saturating_sub(step),
            TranscriptDirection::Down => viewport
                .scroll_position
                .saturating_add(step)
                .min(viewport.maximum_scroll),
        };
        // Record the target eagerly so a burst of scroll events handled between
        // two frames compounds instead of re-deriving from a stale viewport.
        viewport.scroll_position = target;
        if target >= viewport.maximum_scroll {
            interaction.follow_latest.set(true);
            interaction.anchor.set(None);
            return;
        }
        let anchor = viewport.anchor_at(target);
        interaction.follow_latest.set(false);
        interaction.anchor.set(anchor);
    }

    fn follow_latest(&mut self) {
        let Some(session_id) = self.session.as_ref().map(SessionProjection::session_id) else {
            return;
        };
        let interaction = self.session_interactions.entry(session_id).or_default();
        interaction.follow_latest.set(true);
        interaction.anchor.set(None);
    }

    pub(super) fn composer_border_style(&self, theme: &Theme) -> Style {
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

    pub(super) fn provisional_prompts(&self, session_id: SessionId) -> Vec<&InitialPrompt> {
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

    pub(super) fn queued_prompts(&self, session_id: SessionId) -> Vec<QueuedPrompt<'_>> {
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
pub(super) struct QueuedPrompt<'a> {
    pub(super) id: PromptId,
    pub(super) text: &'a str,
}

pub struct Application {
    pub(super) state: TuiState,
    pub(super) slots: RenderSlots,
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
    ScrollTranscriptLinesUp,
    ScrollTranscriptLinesDown,
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
            ApplicationEvent::Command(CommandId::ScrollTranscriptLinesUp) => {
                self.state
                    .navigate_transcript_lines(TranscriptDirection::Up);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Command(CommandId::ScrollTranscriptLinesDown) => {
                self.state
                    .navigate_transcript_lines(TranscriptDirection::Down);
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
            SemanticCommandId::ApplicationExit => Ok(ApplicationTransition::Exit),
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
        self.command_for_terminal_input(event)
            .map_or(Ok(ApplicationTransition::Continue), |command| {
                self.handle_event(ApplicationEvent::Command(command))
            })
    }

    /// Translates a terminal event through the active input mode. `None` means
    /// the event changes nothing, so callers can skip redrawing.
    pub fn command_for_terminal_input(&self, event: InputEvent) -> Option<CommandId> {
        if self.state.model_options.is_open() {
            return command_for_model_options_event(event);
        }
        if self.state.model_picker.is_open() {
            return command_for_model_picker_event(event);
        }
        if self.state.session_picker.is_open() {
            return command_for_session_picker_event(event);
        }
        if self.state.command_autocomplete.is_visible()
            && let Some(command) = command_for_autocomplete_event(event.clone())
        {
            return Some(command);
        }
        match self.state.command_mode {
            CommandMode::Composer => command_for_terminal_event(event),
            CommandMode::Leader => command_for_leader_event(event),
            CommandMode::QueuedPrompts { .. } => command_for_queued_prompt_event(event),
            CommandMode::InterruptConfirmation { .. } => {
                command_for_interrupt_confirmation_event(event)
            }
        }
    }

    pub(super) fn session_id(&self) -> Option<SessionId> {
        self.state
            .session
            .as_ref()
            .map(SessionProjection::session_id)
    }

    pub(super) fn is_recovering(&self) -> bool {
        self.state.recovery.is_some()
    }
}
