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
        Activity, ActivityStatus, AdmitPromptRequest, AgentSelection, AgentSelectionOperationId,
        CreateSessionRequest, InitialPrompt, MessageId, ModelCatalog, PromptDelivery, PromptId,
        PromptStatus, ServerIdentity, SessionChange, SessionId, SessionListItem, SessionSnapshot,
        ShutdownReason, TurnId, TurnStatus, UpdateAgentSelectionRequest, Workspace,
    },
    theme::Theme,
};

use super::{
    commands::{CommandAutocomplete, SemanticCommandId},
    composer::{ComposerKey, ComposerMemory},
    keymap::{
        command_for_autocomplete_event, command_for_interrupt_confirmation_event,
        command_for_leader_event, command_for_model_options_event, command_for_model_picker_event,
        command_for_queued_prompt_event, command_for_session_picker_event,
        command_for_terminal_event,
    },
    model_options::{ModelOptions, ReasoningCycle, cycle_reasoning_effort},
    model_picker::{ModelPicker, ModelPickerAction},
    render::render_with_slots,
    session_picker::SessionPicker,
    slots::RenderSlots,
    transcript::{MessageStart, TranscriptCache, TranscriptFolds, UnitKey, UnitStart},
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
    /// How this client presents each foldable Activity. Folds are view state,
    /// so they live here rather than in the Session and are dropped whenever
    /// the Session's interaction is.
    pub(super) folds: RefCell<TranscriptFolds>,
}

impl Default for SessionInteraction {
    fn default() -> Self {
        Self {
            follow_latest: Cell::new(true),
            anchor: Cell::new(None),
            viewport: RefCell::new(None),
            folds: RefCell::new(TranscriptFolds::default()),
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
    pub(super) unit_starts: Vec<UnitStart>,
    /// Terminal row the first projected transcript row was drawn on, with the
    /// count of rows below it, so a pointer position maps back to a transcript
    /// row without re-deriving the frame's layout.
    pub(super) content_top: u16,
    pub(super) content_rows: u16,
}

impl TranscriptViewport {
    /// The transcript row drawn at `screen_row`, or `None` when that terminal
    /// row belongs to another part of the frame.
    fn transcript_row(&self, screen_row: u16) -> Option<usize> {
        let offset = screen_row.checked_sub(self.content_top)?;
        (offset < self.content_rows)
            .then(|| self.scroll_position.saturating_add(usize::from(offset)))
    }

    /// The projected unit drawn at `screen_row`, with the transcript row the
    /// pointer landed on. Units are recorded in row order, so this is a binary
    /// search rather than a scan of the transcript.
    fn unit_at(&self, screen_row: u16) -> Option<(UnitStart, usize)> {
        let row = self.transcript_row(screen_row)?;
        let index = self
            .unit_starts
            .partition_point(|start| start.row <= row)
            .checked_sub(1)?;
        let start = self.unit_starts[index];
        start.contains(row).then_some((start, row))
    }

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

    /// The attached Session's interaction state, created if this is the first
    /// thing to reach for it.
    fn current_interaction(&mut self) -> Option<&SessionInteraction> {
        let session_id = self.session.as_ref().map(SessionProjection::session_id)?;
        Some(self.session_interactions.entry(session_id).or_default())
    }

    /// Toggles the Fold of the unit drawn at `screen_row`, which today is
    /// always the one Activity that unit projects. A unit holding content back
    /// expands wherever it is clicked; one already showing everything folds
    /// again only from its header line, so pointing at output never hides what
    /// is under the pointer. A unit with nothing to hide answers neither, so an
    /// idle click never records a Fold that does not exist.
    fn toggle_fold_at(&mut self, screen_row: u16) {
        let Some(interaction) = self.current_interaction() else {
            return;
        };
        let Some((start, row)) = interaction
            .viewport
            .borrow()
            .as_ref()
            .and_then(|viewport| viewport.unit_at(screen_row))
        else {
            return;
        };
        let UnitKey::Activity(activity_id) = start.key else {
            return;
        };
        let mut folds = interaction.folds.borrow_mut();
        if start.hides_content {
            folds.expand(activity_id);
        } else if start.is_header(row) && !folds.is_folded(activity_id) {
            folds.fold(activity_id);
        }
    }

    /// Flips the Session view between folded-by-default and expanded-by-default.
    fn toggle_fold_posture(&mut self) {
        if let Some(interaction) = self.current_interaction() {
            interaction.folds.borrow_mut().toggle_posture();
        }
    }

    /// Expands every Activity still Active in `turn_id`. An interrupted Turn
    /// leaves its work half-done, and the reader was already watching it, so
    /// the Fold must not hide what they were reading.
    fn expand_active_activities(&mut self, turn_id: TurnId) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let active = session
            .snapshot()
            .activities
            .iter()
            .filter(|activity| {
                activity.turn_id() == turn_id && activity.status() == Some(ActivityStatus::Active)
            })
            .map(Activity::id)
            .collect::<Vec<_>>();
        let Some(interaction) = self.current_interaction() else {
            return;
        };
        let mut folds = interaction.folds.borrow_mut();
        for activity_id in active {
            folds.expand(activity_id);
        }
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

/// Which neighbour a queued Prompt selection command moves to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QueuedPromptStep {
    Previous,
    Next,
}

/// Whether the Model catalog listing behind a result has finished, so pending
/// Model Options either wait for more of it or give up with a message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CatalogListing {
    InFlight,
    Complete,
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
    ToggleTranscriptFoldAt { screen_row: u16 },
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
            ApplicationEvent::Command(command) => self.handle_command(command),
            ApplicationEvent::ReconnectGraceElapsed => Ok(self.elapse_reconnect_grace()),
            ApplicationEvent::Managed(event) => self.handle_managed_event(event),
            ApplicationEvent::Session(event) => {
                self.state.apply_session(event)?;
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::SessionSubscriptionEnded => Ok(self
                .session_id()
                .map_or(ApplicationTransition::Continue, |session_id| {
                    ApplicationTransition::SubscribeSession(session_id)
                })),
            ApplicationEvent::SessionCreated(snapshot) => {
                self.state.apply_created_session(snapshot)?;
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::SessionAttached(snapshot) => self.attach_session(snapshot),
            ApplicationEvent::SessionAttachmentFailed(error) => {
                let request = self.state.session_picker.fail_attachment(error);
                Ok(ApplicationTransition::ListSessions(request))
            }
            ApplicationEvent::SessionDeletionFailed { session_id, error } => {
                self.state.session_picker.fail_deletion(session_id, error);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::SessionOperationFailed(error) => {
                self.state.submission_error = Some(error);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::PromptAdmissionSucceeded(prompt_id) => {
                self.state.acknowledge_pending_submission(prompt_id);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::PromptAdmissionFailed { prompt_id, error } => {
                self.state.fail_pending_submission(prompt_id, error);
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
                Ok(self.load_model_catalog(&request, catalog, CatalogListing::InFlight))
            }
            ApplicationEvent::ModelsRefreshed { request, catalog } => {
                Ok(self.load_model_catalog(&request, catalog, CatalogListing::Complete))
            }
            ApplicationEvent::ModelListingFailed { request, error } => {
                Ok(self.fail_model_catalog(&request, error))
            }
            ApplicationEvent::LandingAgentSelectionConfirmed(selection) => {
                Ok(self.confirm_landing_agent_selection(selection))
            }
            ApplicationEvent::LandingAgentSelectionConfirmationFailed(error) => {
                Ok(self.fail_landing_agent_selection(error))
            }
            ApplicationEvent::AgentSelectionUpdated {
                operation_id,
                selection,
            } => self.accept_agent_selection_update(operation_id, selection),
            ApplicationEvent::AgentSelectionUpdateFailed {
                operation_id,
                error,
            } => self.fail_agent_selection_update(operation_id, error),
        }
    }

    /// Routes a command to the handler for the surface it acts on. This match
    /// is exhaustive, so a newly added [`CommandId`] has to be routed to a
    /// surface before it compiles; the handler it lands in then ignores
    /// anything outside its own group.
    fn handle_command(&mut self, command: CommandId) -> Result<ApplicationTransition> {
        if self.state.reconnect_overlay_visible || self.defers_for_agent_selection(&command) {
            return Ok(ApplicationTransition::Continue);
        }
        match command {
            CommandId::SubmitSteer => Ok(self.submit_prompt(PromptDelivery::Steer)),
            CommandId::SubmitQueue => Ok(self.submit_prompt(PromptDelivery::Queue)),
            CommandId::InvokeSemantic(command) => self.invoke_semantic(command),
            command @ (CommandId::ClearOrExit
            | CommandId::InsertText(_)
            | CommandId::PasteText(_)
            | CommandId::InsertNewline
            | CommandId::DeleteBackward
            | CommandId::DeleteForward
            | CommandId::MoveCursorLeft
            | CommandId::MoveCursorRight
            | CommandId::HistoryPrevious
            | CommandId::HistoryNext) => Ok(self.handle_composer_command(command)),
            command @ (CommandId::ScrollTranscriptPageUp
            | CommandId::ScrollTranscriptPageDown
            | CommandId::ScrollTranscriptLinesUp
            | CommandId::ScrollTranscriptLinesDown
            | CommandId::FollowLatest
            | CommandId::ToggleTranscriptFoldAt { .. }) => {
                Ok(self.handle_transcript_command(command))
            }
            command @ (CommandId::SelectPreviousAutocomplete
            | CommandId::SelectNextAutocomplete
            | CommandId::DismissAutocomplete
            | CommandId::SelectAutocomplete) => self.handle_autocomplete_command(command),
            command @ (CommandId::InsertSessionSearch(_)
            | CommandId::DeleteSessionSearchBackward
            | CommandId::SelectPreviousSession
            | CommandId::SelectNextSession
            | CommandId::PagePreviousSessions
            | CommandId::PageNextSessions
            | CommandId::ToggleSessionScope
            | CommandId::SelectSession
            | CommandId::CloseSessionPicker) => Ok(self.handle_session_picker_command(command)),
            command @ (CommandId::InsertModelSearch(_)
            | CommandId::DeleteModelSearchBackward
            | CommandId::SelectPreviousModel
            | CommandId::SelectNextModel
            | CommandId::PagePreviousModels
            | CommandId::PageNextModels
            | CommandId::SelectModel
            | CommandId::CloseModelPicker) => self.handle_model_picker_command(command),
            command @ (CommandId::BeginLeader
            | CommandId::OpenQueuedPrompts
            | CommandId::SelectPreviousQueuedPrompt
            | CommandId::SelectNextQueuedPrompt
            | CommandId::PromoteSelectedPrompt
            | CommandId::CancelSelectedPrompt
            | CommandId::RequestInterrupt
            | CommandId::ConfirmInterrupt
            | CommandId::CloseCommandMode) => Ok(self.handle_command_mode_command(command)),
        }
    }

    /// Commands that begin a Turn or retarget the Agent wait for an in-flight
    /// Agent Selection update instead of racing it.
    fn defers_for_agent_selection(&self, command: &CommandId) -> bool {
        self.state.selection_update_pending()
            && matches!(
                command,
                CommandId::SubmitSteer
                    | CommandId::SubmitQueue
                    | CommandId::SelectSession
                    | CommandId::SelectModel
                    | CommandId::InvokeSemantic(
                        SemanticCommandId::SessionList
                            | SemanticCommandId::SessionNew
                            | SemanticCommandId::ModelOptionsApply
                    )
            )
    }

    /// Handles the composer editing commands routed here; any other command
    /// leaves the composer untouched.
    fn handle_composer_command(&mut self, command: CommandId) -> ApplicationTransition {
        match command {
            CommandId::ClearOrExit => {
                let key = self.state.composer_key();
                if self.state.composers.is_empty(key) {
                    return ApplicationTransition::Exit;
                }
                self.state
                    .edit_composer(|composers, key| composers.clear(key));
            }
            CommandId::InsertText(text) => self
                .state
                .edit_composer(|composers, key| composers.insert(key, &text)),
            CommandId::PasteText(text) => self.state.paste_into_composer(&text),
            CommandId::InsertNewline => self
                .state
                .edit_composer(|composers, key| composers.insert(key, "\n")),
            CommandId::DeleteBackward => self
                .state
                .edit_composer(|composers, key| composers.delete_backward(key)),
            CommandId::DeleteForward => self
                .state
                .edit_composer(|composers, key| composers.delete_forward(key)),
            CommandId::MoveCursorLeft => self
                .state
                .navigate_composer(|composers, key| composers.move_left(key)),
            CommandId::MoveCursorRight => self
                .state
                .navigate_composer(|composers, key| composers.move_right(key)),
            CommandId::HistoryPrevious => self
                .state
                .edit_composer(|composers, key| composers.history_previous(key)),
            CommandId::HistoryNext => self
                .state
                .edit_composer(|composers, key| composers.history_next(key)),
            _ => {}
        }
        ApplicationTransition::Continue
    }

    /// Handles the transcript navigation commands routed here; any other
    /// command leaves the viewport where it is.
    fn handle_transcript_command(&mut self, command: CommandId) -> ApplicationTransition {
        match command {
            CommandId::ScrollTranscriptPageUp => {
                self.state.navigate_transcript_page(TranscriptDirection::Up);
            }
            CommandId::ScrollTranscriptPageDown => {
                self.state
                    .navigate_transcript_page(TranscriptDirection::Down);
            }
            CommandId::ScrollTranscriptLinesUp => {
                self.state
                    .navigate_transcript_lines(TranscriptDirection::Up);
            }
            CommandId::ScrollTranscriptLinesDown => {
                self.state
                    .navigate_transcript_lines(TranscriptDirection::Down);
            }
            CommandId::FollowLatest => self.state.follow_latest(),
            CommandId::ToggleTranscriptFoldAt { screen_row } => {
                self.state.toggle_fold_at(screen_row);
            }
            _ => {}
        }
        ApplicationTransition::Continue
    }

    /// Handles the slash command autocomplete commands routed here; any other
    /// command leaves the suggestion list alone.
    fn handle_autocomplete_command(&mut self, command: CommandId) -> Result<ApplicationTransition> {
        match command {
            CommandId::SelectPreviousAutocomplete => {
                self.state.command_autocomplete.select_previous();
            }
            CommandId::SelectNextAutocomplete => self.state.command_autocomplete.select_next(),
            CommandId::DismissAutocomplete => {
                let key = self.state.composer_key();
                let text = self.state.composers.text(key);
                self.state.command_autocomplete.dismiss_for_text(text);
            }
            CommandId::SelectAutocomplete => return self.accept_autocomplete(),
            _ => {}
        }
        Ok(ApplicationTransition::Continue)
    }

    fn accept_autocomplete(&mut self) -> Result<ApplicationTransition> {
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

    /// Handles the Session picker commands routed here; any other command
    /// leaves the picker alone.
    fn handle_session_picker_command(&mut self, command: CommandId) -> ApplicationTransition {
        match command {
            CommandId::InsertSessionSearch(text) => {
                self.edit_session_picker(|picker| picker.insert(&text));
            }
            CommandId::DeleteSessionSearchBackward => {
                self.edit_session_picker(SessionPicker::delete_backward);
            }
            CommandId::SelectPreviousSession => {
                self.edit_session_picker(SessionPicker::select_previous);
            }
            CommandId::SelectNextSession => self.edit_session_picker(SessionPicker::select_next),
            CommandId::PagePreviousSessions => {
                self.edit_session_picker(SessionPicker::page_previous);
            }
            CommandId::PageNextSessions => self.edit_session_picker(SessionPicker::page_next),
            CommandId::ToggleSessionScope => {
                if self.state.session_picker.is_busy() {
                    return ApplicationTransition::Continue;
                }
                let request = self.state.session_picker.toggle_scope();
                return ApplicationTransition::ListSessions(request);
            }
            CommandId::SelectSession => {
                return self.state.session_picker.begin_attachment().map_or(
                    ApplicationTransition::Continue,
                    ApplicationTransition::AttachSession,
                );
            }
            CommandId::CloseSessionPicker => self.edit_session_picker(SessionPicker::close),
            _ => {}
        }
        ApplicationTransition::Continue
    }

    /// Handles the Model picker commands routed here; any other command leaves
    /// the picker alone.
    fn handle_model_picker_command(&mut self, command: CommandId) -> Result<ApplicationTransition> {
        match command {
            CommandId::InsertModelSearch(text) => self.state.model_picker.insert(&text),
            CommandId::DeleteModelSearchBackward => self.state.model_picker.delete_backward(),
            CommandId::SelectPreviousModel => self.state.model_picker.select_previous(),
            CommandId::SelectNextModel => self.state.model_picker.select_next(),
            CommandId::PagePreviousModels => self.state.model_picker.page_previous(),
            CommandId::PageNextModels => self.state.model_picker.page_next(),
            CommandId::CloseModelPicker => self.state.model_picker.close(),
            CommandId::SelectModel => return self.choose_model(),
            _ => {}
        }
        Ok(ApplicationTransition::Continue)
    }

    fn choose_model(&mut self) -> Result<ApplicationTransition> {
        match self.state.model_picker.choose() {
            Some(ModelPickerAction::Retry) => Ok(self.state.model_picker.begin_retry().map_or(
                ApplicationTransition::Continue,
                ApplicationTransition::ListModels,
            )),
            Some(ModelPickerAction::Select(model)) => {
                self.state.model_picker.close();
                if model.options.is_empty() {
                    return self.apply_agent_selection(model.default_agent_selection());
                }
                let current = self.state.agent_selection().cloned();
                self.state.model_options.open(model, current.as_ref());
                Ok(ApplicationTransition::Continue)
            }
            None => Ok(ApplicationTransition::Continue),
        }
    }

    /// Handles the leader, queued Prompt, and interrupt confirmation commands
    /// routed here; any other command leaves the command mode alone.
    fn handle_command_mode_command(&mut self, command: CommandId) -> ApplicationTransition {
        match command {
            CommandId::BeginLeader => self.state.command_mode = CommandMode::Leader,
            CommandId::CloseCommandMode => self.state.command_mode = CommandMode::Composer,
            CommandId::OpenQueuedPrompts => self.open_queued_prompts(),
            CommandId::SelectPreviousQueuedPrompt => {
                self.select_queued_prompt(QueuedPromptStep::Previous);
            }
            CommandId::SelectNextQueuedPrompt => {
                self.select_queued_prompt(QueuedPromptStep::Next);
            }
            CommandId::PromoteSelectedPrompt => {
                return self.apply_to_selected_prompt(|session_id, prompt_id| {
                    ApplicationTransition::PromotePrompt {
                        session_id,
                        prompt_id,
                    }
                });
            }
            CommandId::CancelSelectedPrompt => {
                return self.apply_to_selected_prompt(|session_id, prompt_id| {
                    ApplicationTransition::CancelPrompt {
                        session_id,
                        prompt_id,
                    }
                });
            }
            CommandId::RequestInterrupt => {
                if let Some(turn_id) = self.state.active_turn_id() {
                    self.state.command_mode = CommandMode::InterruptConfirmation { turn_id };
                }
            }
            CommandId::ConfirmInterrupt => return self.confirm_interrupt(),
            _ => {}
        }
        ApplicationTransition::Continue
    }

    fn open_queued_prompts(&mut self) {
        let Some(session_id) = self.session_id() else {
            self.state.command_mode = CommandMode::Composer;
            return;
        };
        self.state.command_mode =
            self.state
                .queued_prompts(session_id)
                .first()
                .map_or(CommandMode::Composer, |prompt| CommandMode::QueuedPrompts {
                    selected: prompt.id,
                });
    }

    fn select_queued_prompt(&mut self, step: QueuedPromptStep) {
        let (Some(session_id), CommandMode::QueuedPrompts { selected }) =
            (self.session_id(), self.state.command_mode)
        else {
            return;
        };
        let queued = self.state.queued_prompts(session_id);
        let Some(index) = queued.iter().position(|prompt| prompt.id == selected) else {
            return;
        };
        let next = match step {
            QueuedPromptStep::Previous => index.saturating_sub(1),
            QueuedPromptStep::Next => (index + 1).min(queued.len().saturating_sub(1)),
        };
        self.state.command_mode = CommandMode::QueuedPrompts {
            selected: queued[next].id,
        };
    }

    /// Leaves the queued Prompt mode, handing the selected Prompt to `act`.
    fn apply_to_selected_prompt(
        &mut self,
        act: impl FnOnce(SessionId, PromptId) -> ApplicationTransition,
    ) -> ApplicationTransition {
        let (Some(session_id), CommandMode::QueuedPrompts { selected }) =
            (self.session_id(), self.state.command_mode)
        else {
            return ApplicationTransition::Continue;
        };
        self.state.command_mode = CommandMode::Composer;
        act(session_id, selected)
    }

    fn confirm_interrupt(&mut self) -> ApplicationTransition {
        let (Some(session_id), CommandMode::InterruptConfirmation { turn_id }) =
            (self.session_id(), self.state.command_mode)
        else {
            return ApplicationTransition::Continue;
        };
        self.state.command_mode = CommandMode::Composer;
        self.state.expand_active_activities(turn_id);
        ApplicationTransition::InterruptTurn {
            session_id,
            turn_id,
        }
    }

    fn submit_prompt(&mut self, delivery: PromptDelivery) -> ApplicationTransition {
        if self.state.pending_submission.is_some() {
            return ApplicationTransition::Continue;
        }
        let key = self.state.composer_key();
        if self.state.composers.text(key).trim().is_empty() {
            self.state.submission_error =
                Some("Prompt must contain non-whitespace text".to_owned());
            return ApplicationTransition::Continue;
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
            return ApplicationTransition::AdmitPrompt {
                session_id,
                request: AdmitPromptRequest { prompt, delivery },
            };
        }
        self.state.pending_submission = Some(PendingSubmission {
            source: key,
            target: SubmissionTarget::CreateSession,
            prompt: prompt.clone(),
        });
        ApplicationTransition::CreateSession(CreateSessionRequest {
            agent_selection: self.state.landing_agent_selection.clone(),
            workspace: Workspace {
                path: self.state.workspace.clone(),
            },
            prompt,
        })
    }

    fn elapse_reconnect_grace(&mut self) -> ApplicationTransition {
        if self.state.recovery.is_some() {
            self.state.reconnect_overlay_visible = true;
        }
        ApplicationTransition::Continue
    }

    fn handle_managed_event(&mut self, event: ManagedEvent) -> Result<ApplicationTransition> {
        match event {
            ManagedEvent::Fatal(error) => Err(anyhow!(error)),
            event @ ManagedEvent::ServerShutdown(_) => {
                self.state.apply(event);
                Ok(ApplicationTransition::Exit)
            }
            event => {
                let had_session = self.state.session.is_some();
                self.state.apply(event);
                self.state.reconcile_command_mode();
                if had_session && self.state.session.is_none() {
                    Ok(ApplicationTransition::SessionEnded)
                } else {
                    Ok(ApplicationTransition::Continue)
                }
            }
        }
    }

    fn attach_session(&mut self, snapshot: SessionSnapshot) -> Result<ApplicationTransition> {
        let closes_picker = self.state.session_picker.attaching_to(snapshot.session.id);
        self.state.apply_attached_session(snapshot)?;
        if closes_picker {
            self.state.session_picker.close();
        }
        Ok(ApplicationTransition::Continue)
    }

    fn load_model_catalog(
        &mut self,
        request: &ModelListRequest,
        catalog: ModelCatalog,
        listing: CatalogListing,
    ) -> ApplicationTransition {
        let accepted = self.state.model_picker.is_active_request(request);
        let current = self.state.agent_selection().cloned();
        self.state
            .model_picker
            .load(request, catalog, current.as_ref());
        if listing == CatalogListing::Complete {
            self.state.model_picker.finish(request);
        }
        if accepted {
            self.reconcile_model_options(listing);
        }
        ApplicationTransition::Continue
    }

    fn fail_model_catalog(
        &mut self,
        request: &ModelListRequest,
        error: String,
    ) -> ApplicationTransition {
        let accepted = self.state.model_picker.is_active_request(request);
        self.state.model_picker.fail(request, error);
        if accepted {
            self.reconcile_model_options(CatalogListing::Complete);
        }
        ApplicationTransition::Continue
    }

    fn confirm_landing_agent_selection(
        &mut self,
        selection: AgentSelection,
    ) -> ApplicationTransition {
        if self.state.pending_landing_agent_selection.take().is_none() {
            return ApplicationTransition::Continue;
        }
        self.state.confirmed_landing_agent_selection = Some(selection.clone());
        self.state.submission_error = None;
        if let Some(queued) = self.state.queued_landing_agent_selection.take()
            && queued != selection
        {
            return self.begin_landing_agent_selection_confirmation(queued);
        }
        self.state.landing_agent_selection = Some(selection);
        ApplicationTransition::Continue
    }

    fn fail_landing_agent_selection(&mut self, error: String) -> ApplicationTransition {
        if self.state.pending_landing_agent_selection.take().is_none() {
            return ApplicationTransition::Continue;
        }
        if let Some(queued) = self.state.queued_landing_agent_selection.take() {
            return self.begin_landing_agent_selection_confirmation(queued);
        }
        self.state.landing_agent_selection = self.state.confirmed_landing_agent_selection.clone();
        self.state.submission_error = Some(error);
        ApplicationTransition::Continue
    }

    /// Retires the in-flight Agent Selection update named by `operation_id`,
    /// reporting the Session it targeted. `None` means a newer operation has
    /// already replaced it, so the result is stale.
    fn take_pending_agent_selection(
        &mut self,
        operation_id: AgentSelectionOperationId,
    ) -> Option<SessionId> {
        let settles = self
            .state
            .pending_agent_selection
            .as_ref()
            .is_some_and(|pending| pending.operation_id == operation_id);
        settles.then(|| {
            self.state
                .pending_agent_selection
                .take()
                .expect("matching pending Agent Selection exists")
                .session_id
        })
    }

    fn accept_agent_selection_update(
        &mut self,
        operation_id: AgentSelectionOperationId,
        selection: AgentSelection,
    ) -> Result<ApplicationTransition> {
        let Some(session_id) = self.take_pending_agent_selection(operation_id) else {
            return Ok(ApplicationTransition::Continue);
        };
        self.state.submission_error = None;
        if let Some((queued_session, queued)) = self.state.queued_agent_selection.take()
            && queued_session == session_id
            && queued != selection
        {
            self.state.confirmed_agent_selection = Some((session_id, selection));
            return self.begin_agent_selection_update(queued_session, queued);
        }
        self.state.confirmed_agent_selection = Some((session_id, selection));
        Ok(ApplicationTransition::Continue)
    }

    fn fail_agent_selection_update(
        &mut self,
        operation_id: AgentSelectionOperationId,
        error: String,
    ) -> Result<ApplicationTransition> {
        let Some(session_id) = self.take_pending_agent_selection(operation_id) else {
            return Ok(ApplicationTransition::Continue);
        };
        if let Some((queued_session, queued)) = self.state.queued_agent_selection.take()
            && queued_session == session_id
        {
            // A newer queued selection supersedes this failure.
            return self.begin_agent_selection_update(queued_session, queued);
        }
        // Keep any prior confirmed acceptance: it is newer authoritative state
        // than the snapshot base.
        self.state.submission_error = Some(error);
        Ok(ApplicationTransition::Continue)
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
            command @ (SemanticCommandId::ModelOptions
            | SemanticCommandId::ModelOptionsPrevious
            | SemanticCommandId::ModelOptionsNext
            | SemanticCommandId::ModelOptionsSelect
            | SemanticCommandId::ModelOptionsApply
            | SemanticCommandId::ModelOptionsCancel
            | SemanticCommandId::ModelOptionReasoningCycle) => {
                self.handle_model_options_command(command)
            }
            SemanticCommandId::SessionList => {
                let request = self.state.session_picker.open();
                self.state.command_mode = CommandMode::Composer;
                Ok(ApplicationTransition::ListSessions(request))
            }
            SemanticCommandId::TranscriptFoldsToggle => {
                self.state.toggle_fold_posture();
                self.state.command_mode = CommandMode::Composer;
                Ok(ApplicationTransition::Continue)
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

    /// Handles the Model Options commands routed here; any other semantic
    /// command leaves the options editor alone.
    fn handle_model_options_command(
        &mut self,
        command: SemanticCommandId,
    ) -> Result<ApplicationTransition> {
        match command {
            SemanticCommandId::ModelOptions => return Ok(self.open_model_options()),
            SemanticCommandId::ModelOptionsPrevious => self.state.model_options.select_previous(),
            SemanticCommandId::ModelOptionsNext => self.state.model_options.select_next(),
            SemanticCommandId::ModelOptionsSelect => self.state.model_options.choose(),
            SemanticCommandId::ModelOptionsCancel => self.state.model_options.close(),
            SemanticCommandId::ModelOptionsApply => return self.apply_model_options(),
            SemanticCommandId::ModelOptionReasoningCycle => {
                return self.cycle_model_reasoning_effort();
            }
            _ => {}
        }
        Ok(ApplicationTransition::Continue)
    }

    /// Opens the options editor for the current Model, refreshing the catalog
    /// first so the choices on screen are the ones the Provider still offers.
    fn open_model_options(&mut self) -> ApplicationTransition {
        self.state.pending_model_options = true;
        self.state.submission_error = None;
        let request = self.state.model_picker.begin_refresh();
        self.reconcile_model_options(CatalogListing::InFlight);
        self.state.command_mode = CommandMode::Composer;
        ApplicationTransition::ListModels(request)
    }

    fn apply_model_options(&mut self) -> Result<ApplicationTransition> {
        if self.state.model_options.is_choice_picker_open() {
            return Ok(ApplicationTransition::Continue);
        }
        let Some(selection) = self.state.model_options.apply() else {
            return Ok(ApplicationTransition::Continue);
        };
        self.state.model_options.close();
        self.apply_agent_selection(selection)
    }

    /// Advances the current Model's reasoning effort one step, refreshing the
    /// catalog instead when no concrete Model is cached to cycle through.
    fn cycle_model_reasoning_effort(&mut self) -> Result<ApplicationTransition> {
        let current = self.state.agent_selection().cloned();
        let Some(model) = self
            .state
            .model_picker
            .cached_model_for_options(current.as_ref())
        else {
            self.state.submission_error =
                Some("No concrete Model is loaded yet; use /models to choose one".to_owned());
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

    fn reconcile_model_options(&mut self, listing: CatalogListing) {
        if self.state.model_options.is_open() {
            let current_model = self
                .state
                .model_options
                .model()
                .map(|model| (model.provider.clone(), model.id.clone()));
            if let Some((provider, model)) = current_model {
                if let Some(refreshed) = self.state.model_picker.cached_model(&provider, &model) {
                    self.state.model_options.refresh(refreshed);
                } else if listing == CatalogListing::Complete {
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
            if listing == CatalogListing::Complete {
                self.state.pending_model_options = false;
                self.state.submission_error =
                    Some("No concrete Model is available; use /models to choose one".to_owned());
            }
            return;
        };
        if model.options.is_empty() {
            if listing == CatalogListing::Complete {
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
