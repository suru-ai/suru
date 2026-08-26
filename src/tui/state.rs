//! Application and view state: the Session projection the TUI renders, the
//! events it accepts, and the transitions it asks the runtime to perform.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::{Result, anyhow};
use crossterm::event::{Event as InputEvent, KeyEventKind, MouseEventKind};
use ratatui::{Frame, layout::Position, style::Style};

use crate::{
    managed_client::{ManagedEvent, RecoveryStatus, SessionEvent, SessionProjection},
    protocol::{
        Activity, ActivityStatus, AdmitPromptRequest, AgentSelection, AgentSelectionOperationId,
        CreateSessionRequest, EffectiveSettings, FoldPosture, InitialPrompt, MessageId,
        ModelCatalog, PromptDelivery, PromptId, PromptStatus, ServerIdentity, SessionChange,
        SessionId, SessionListItem, SessionSnapshot, SessionStatus, SettingMutation,
        SettingsSnapshot, ShutdownReason, SkillCatalog, SkillCatalogRequest, TurnId, TurnStatus,
        UpdateAgentSelectionRequest, Workspace,
    },
    settings::SettingChoiceSurface,
    theme::Theme,
};

use super::{
    commands::{SemanticCommandId, SemanticInvocation, SemanticSubject},
    completion::{CompletionConfirmation, CompletionMode, ComposerCompletion},
    composer::{ComposerKey, ComposerMemory},
    keymap::{
        command_for_completion_event, command_for_interrupt_confirmation_event,
        command_for_leader_event, command_for_model_options_event, command_for_model_picker_event,
        command_for_numeric_editor_event, command_for_queued_prompt_event,
        command_for_session_picker_event, command_for_settings_panel_event,
        command_for_sidebar_event, command_for_terminal_event,
    },
    model_options::{ModelOptions, ReasoningCycle, cycle_reasoning_effort},
    model_picker::{ModelPicker, ModelPickerAction, ModelPickerPurpose},
    notice::{LandingNotice, Notice},
    render::render_with_slots,
    session_picker::SessionPicker,
    settings_panel::{AvailabilityRead, SettingsPanel},
    sidebar::Sidebar,
    slots::RenderSlots,
    transcript::{
        FoldStep, MessageStart, TranscriptCache, TranscriptFolds, TranscriptGroups,
        TranscriptTurnFolds, UnitKey, UnitStart,
    },
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

    /// Widens this scope to every Workspace, or narrows it back to the one
    /// the client runs in — the flip a reader makes when a listing scoped to
    /// where they stand is too narrow, or too wide, for what they are after.
    pub(super) fn toggled(&self, current_workspace: &Path) -> Self {
        match self {
            Self::CurrentWorkspace(_) => Self::AllWorkspaces,
            Self::AllWorkspaces => Self::CurrentWorkspace(current_workspace.to_owned()),
        }
    }

    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::CurrentWorkspace(_) => "Current Workspace",
            Self::AllWorkspaces => "All Workspaces",
        }
    }
}

/// Which surface a Session listing answers. Two of them list Sessions at once —
/// the picker over the main view and the Sidebar beside it — so every request
/// names its own, and neither surface can take the other's reply for one of
/// its own.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionListSurface {
    Picker,
    Sidebar,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionListRequest {
    surface: SessionListSurface,
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
    pub(super) fn new(surface: SessionListSurface, id: u64, scope: SessionListScope) -> Self {
        Self { surface, id, scope }
    }

    pub fn scope(&self) -> &SessionListScope {
        &self.scope
    }

    pub fn surface(&self) -> SessionListSurface {
        self.surface
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
    /// Which Groups this client has expanded, held beside the Folds because
    /// the disclosure axes are view state of the same locality.
    pub(super) groups: RefCell<TranscriptGroups>,
    /// Which settled Turns this client has expanded out of their Turn Fold,
    /// the third axis of that same locality: per-Session, in memory, and gone
    /// with the Session's interaction.
    pub(super) turns: RefCell<TranscriptTurnFolds>,
}

impl SessionInteraction {
    /// A Session view the reader has not touched yet. Only the Fold axis takes
    /// its opening posture from a Setting; the Group and Turn Fold axes have
    /// no Setting of their own and always open closed.
    fn opening_at(fold_posture: FoldPosture) -> Self {
        Self {
            follow_latest: Cell::new(true),
            anchor: Cell::new(None),
            viewport: RefCell::new(None),
            folds: RefCell::new(TranscriptFolds::opening_at(fold_posture)),
            groups: RefCell::new(TranscriptGroups::default()),
            turns: RefCell::new(TranscriptTurnFolds::default()),
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
    /// count of rows below it and the horizontal Session Content Column, so a
    /// pointer position maps back to a transcript row without re-deriving the
    /// frame's layout and clicks in centered gutters reach nothing.
    pub(super) content_top: u16,
    pub(super) content_rows: u16,
    pub(super) content_left: u16,
    pub(super) content_width: u16,
}

impl TranscriptViewport {
    /// The transcript row drawn at `screen_row`, or `None` when that terminal
    /// row belongs to another part of the frame.
    fn transcript_row(&self, position: Position) -> Option<usize> {
        let column = position.x.checked_sub(self.content_left)?;
        if column >= self.content_width {
            return None;
        }
        let offset = position.y.checked_sub(self.content_top)?;
        (offset < self.content_rows)
            .then(|| self.scroll_position.saturating_add(usize::from(offset)))
    }

    /// The projected unit drawn at `screen_row`, with the transcript row the
    /// pointer landed on. Units are recorded in row order, so this is a binary
    /// search rather than a scan of the transcript.
    fn unit_at(&self, position: Position) -> Option<(UnitStart, usize)> {
        let row = self.transcript_row(position)?;
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
    /// The effective value of every Setting, as the server last pushed it.
    /// Client Settings govern presentation from here. The snapshot leads the
    /// lifecycle stream, so it is in hand before any Session view opens.
    settings: EffectiveSettings,
    /// The dotted keys a Config Document pins, from the same snapshot, so the
    /// settings panel can tell the reader's own choice from a built-in default.
    pinned_settings: Vec<String>,
    /// What the Landing has to say about the configuration problems startup
    /// found. It is a Notice, not state the run depends on: the reader's next
    /// interaction takes it away for good.
    landing_notice: LandingNotice,
    pub(super) transcript_cache: TranscriptCache,
    /// Bumped whenever the Session projection is replaced wholesale, so the
    /// transcript cache never trusts a revision across snapshot swaps.
    pub(super) transcript_generation: u64,
    /// Which Spinner frame is showing, advanced by the run loop's tick and
    /// read only at draw time — never by the transcript projection (ADR 0009).
    pub(super) spinner_frame: usize,
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
    pub(super) composer_completion: ComposerCompletion,
    skill_catalog: Option<(SkillCatalogRequest, SkillCatalog)>,
    pending_model_options: bool,
    pub(super) model_options: ModelOptions,
    pub(super) model_picker: ModelPicker,
    pub(super) session_picker: SessionPicker,
    pub(super) sidebar: Sidebar,
    pub(super) settings_panel: SettingsPanel,
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
            settings: EffectiveSettings::default(),
            pinned_settings: Vec::new(),
            landing_notice: LandingNotice::default(),
            transcript_cache: TranscriptCache::default(),
            transcript_generation: 0,
            spinner_frame: 0,
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
            composer_completion: ComposerCompletion::default(),
            skill_catalog: None,
            pending_model_options: false,
            model_options: ModelOptions::default(),
            model_picker: ModelPicker::default(),
            session_picker: SessionPicker::new(workspace.clone()),
            sidebar: Sidebar::new(workspace),
            settings_panel: SettingsPanel::default(),
        }
    }

    fn sync_composer_completion(&mut self) {
        let key = self.composer_key();
        let catalog = self.current_skill_catalog().cloned();
        self.composers.resolve_skills(key, catalog.as_ref());
        let bound_skills = self.composers.valid_skill_ids(key);
        self.composer_completion.sync(
            self.composers.text(key),
            self.composers.cursor(key),
            catalog.as_ref(),
            &bound_skills,
        );
    }

    fn current_skill_catalog(&self) -> Option<&SkillCatalog> {
        let request = self.skill_catalog_request()?;
        self.skill_catalog
            .as_ref()
            .filter(|(loaded, _)| *loaded == request)
            .map(|(_, catalog)| catalog)
    }

    fn skill_catalog_request(&self) -> Option<SkillCatalogRequest> {
        let selection = self.agent_selection()?;
        let workspace = self.session.as_ref().map_or_else(
            || self.workspace.clone(),
            |session| session.snapshot().session.workspace.path.clone(),
        );
        let workspace = std::fs::canonicalize(&workspace).unwrap_or(workspace);
        Some(SkillCatalogRequest {
            provider: selection.provider.clone(),
            workspace: Workspace { path: workspace },
        })
    }

    fn load_skill_catalog(&mut self, request: SkillCatalogRequest, catalog: SkillCatalog) {
        if self.skill_catalog_request().as_ref() == Some(&request) {
            self.skill_catalog = Some((request, catalog));
            self.sync_composer_completion();
        }
    }

    fn skill_catalog_retry_request(&self) -> Option<SkillCatalogRequest> {
        if !self.composer_completion.is_skill_completion() {
            return None;
        }
        matches!(
            self.current_skill_catalog()?.status,
            crate::protocol::SkillCatalogStatus::Stale { .. }
                | crate::protocol::SkillCatalogStatus::Unavailable { .. }
        )
        .then(|| self.skill_catalog_request())
        .flatten()
    }

    fn edit_composer(&mut self, edit: impl FnOnce(&mut ComposerMemory, ComposerKey)) {
        let key = self.composer_key();
        edit(&mut self.composers, key);
        self.submission_error = None;
        self.sync_composer_completion();
    }

    fn navigate_composer(&mut self, navigate: impl FnOnce(&mut ComposerMemory, ComposerKey)) {
        let key = self.composer_key();
        navigate(&mut self.composers, key);
        self.sync_composer_completion();
    }

    fn paste_into_composer(&mut self, text: &str) {
        self.edit_composer_without_completion(|composers, key| composers.insert(key, text));
    }

    fn restore_composer_history(&mut self, restore: impl FnOnce(&mut ComposerMemory, ComposerKey)) {
        self.edit_composer_without_completion(restore);
    }

    fn edit_composer_without_completion(
        &mut self,
        edit: impl FnOnce(&mut ComposerMemory, ComposerKey),
    ) {
        let key = self.composer_key();
        edit(&mut self.composers, key);
        self.submission_error = None;
        self.sync_composer_completion();
        self.composer_completion.dismiss_active();
    }

    pub fn apply(&mut self, event: ManagedEvent) {
        match event {
            ManagedEvent::Connecting => {
                self.skill_catalog = None;
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
                    self.sync_composer_completion();
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
            ManagedEvent::SettingsSnapshot(snapshot) => self.adopt_settings(snapshot),
            ManagedEvent::SkillCatalogUpdated(catalog) => {
                let request = SkillCatalogRequest {
                    provider: catalog.provider.clone(),
                    workspace: catalog.workspace.clone(),
                };
                self.load_skill_catalog(request, catalog);
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
                self.sidebar.remove(deleted.session_id);
                self.remove_deleted_session(deleted.session_id);
            }
            ManagedEvent::SessionTitleChanged(retitled) => {
                self.session_picker.retitle(
                    retitled.session_id,
                    retitled.title.clone(),
                    retitled.emoji.clone(),
                );
                self.sidebar
                    .retitle(retitled.session_id, retitled.title, retitled.emoji);
            }
            ManagedEvent::SessionSettlementChanged(settled) => {
                self.session_picker
                    .settle(settled.session_id, settled.settled_at);
                self.sidebar.settle(settled.session_id, settled.settled_at);
            }
            ManagedEvent::SessionCatalogReconciled(snapshot) => {
                self.session_picker.retain_catalog(&snapshot.session_ids);
                self.sidebar.retain_catalog(&snapshot.session_ids);
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

    /// Takes a freshly pushed effective-settings snapshot as the whole truth
    /// about what every Setting is worth, which is what makes an edit's round
    /// trip — and not the keystroke that started it — move a settings row.
    fn adopt_settings(&mut self, snapshot: SettingsSnapshot) {
        // Session views already open keep the posture they opened at: a
        // default is what a view starts from, not something that reaches back
        // and moves what the reader is looking at.
        self.settings = snapshot.settings;
        self.pinned_settings = snapshot.pinned;
        // The Sidebar's own Settings are the ones that act on arrival rather
        // than on the next view opened, because the frame they govern is
        // already on screen.
        self.sidebar.adopt_settings(&self.settings.sidebar);
        self.landing_notice.receive(&snapshot.diagnostics);
    }

    pub(super) fn settings(&self) -> &EffectiveSettings {
        &self.settings
    }

    pub(super) fn pinned_settings(&self) -> &[String] {
        &self.pinned_settings
    }

    pub(super) fn landing_notice(&self) -> Option<&Notice> {
        self.landing_notice.showing()
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
        self.sync_composer_completion();
    }

    fn apply_session(&mut self, event: SessionEvent) -> Result<()> {
        if self.session_events_blocked {
            return Ok(());
        }
        // What the event means for the view, read before the event is consumed.
        // A Snapshot replaces the Session wholesale, so it always resettles the
        // agent selection; it reports no Turn beginning, because it carries a
        // Session's Turns rather than the news of one starting.
        let (selection_changed, turn_began) = match &event {
            SessionEvent::Snapshot(_) => (true, false),
            SessionEvent::Updated(update) => (
                update
                    .changes
                    .iter()
                    .any(|change| matches!(change, SessionChange::AgentSelectionChanged { .. })),
                update
                    .changes
                    .iter()
                    .any(|change| matches!(change, SessionChange::TurnAdded { .. })),
            ),
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
        if turn_began {
            self.refold_expanded_turns();
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
        self.ensure_interaction(snapshot.session.id);
        self.submission_error = None;
        self.session = Some(SessionProjection::new(snapshot));
        self.transcript_generation = self.transcript_generation.wrapping_add(1);
        self.sync_composer_completion();
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

    /// One Session's interaction state, opened at the posture the Settings ask
    /// for if this is the first thing to reach for it. Rendering reads it back
    /// expecting it to be there, so hydrating a Session view calls this before
    /// the first frame.
    fn ensure_interaction(&mut self, session_id: SessionId) -> &mut SessionInteraction {
        let fold_posture = self.settings.transcript.default_fold_posture;
        self.session_interactions
            .entry(session_id)
            .or_insert_with(|| SessionInteraction::opening_at(fold_posture))
    }

    /// The attached Session's interaction state, created if this is the first
    /// thing to reach for it.
    fn current_interaction(&mut self) -> Option<&SessionInteraction> {
        let session_id = self.session.as_ref().map(SessionProjection::session_id)?;
        Some(self.ensure_interaction(session_id))
    }

    /// Toggles the disclosure of the unit drawn at `screen_row`: the Fold of
    /// a unit projecting a single Activity, or the expansion of a Group,
    /// answering with the semantic command a unit driven by one asks for. A
    /// binary Fold and a Group speak one grammar: a unit holding content back
    /// expands wherever it is clicked; one already showing everything closes
    /// again only from its header line, so pointing at content never hides
    /// what is under the pointer. A settled command's staged Fold opens one
    /// step at a time instead: its folded row opens to the Peek, the Peek's
    /// fold marker opens the rest, and the header folds it back from either
    /// step. Clicks on revealed output do nothing, keeping that surface free
    /// for text selection. A unit with nothing to hide answers no click, so an
    /// idle click never records state that changes nothing. An expanded
    /// Group's members are their own units, which is why a member click
    /// toggles that member's Fold and never the Group.
    ///
    /// A Turn Fold's marker is not toggled here: the pointer resolves to the
    /// Turn it stands for and the caller invokes that Turn's semantic command,
    /// so a click, a keybinding, and a future plugin all reach one behavior.
    fn toggle_disclosure_at(&mut self, position: Position) -> Option<SemanticInvocation> {
        let interaction = self.current_interaction()?;
        let (start, row) = interaction
            .viewport
            .borrow()
            .as_ref()
            .and_then(|viewport| viewport.unit_at(position))?;
        match start.key {
            UnitKey::Activity(activity_id) => {
                let mut folds = interaction.folds.borrow_mut();
                match start.step {
                    Some(FoldStep::Folded) => {
                        if start.hides_content {
                            folds.set_step(activity_id, FoldStep::Peek);
                        }
                    }
                    Some(FoldStep::Peek) => {
                        if start.marker_row == Some(row) {
                            folds.set_step(activity_id, FoldStep::Expanded);
                        } else if start.is_header(row) {
                            folds.set_step(activity_id, FoldStep::Folded);
                        }
                    }
                    Some(FoldStep::Expanded) => {
                        if start.is_header(row) {
                            folds.set_step(activity_id, FoldStep::Folded);
                        }
                    }
                    None => {
                        if start.hides_content {
                            folds.expand(activity_id);
                        } else if start.is_header(row) && !folds.is_folded(activity_id) {
                            folds.fold(activity_id);
                        }
                    }
                }
            }
            UnitKey::Group(group_id) => {
                let mut groups = interaction.groups.borrow_mut();
                if start.hides_content {
                    groups.expand(group_id);
                } else if start.is_header(row) && !groups.is_collapsed(group_id) {
                    groups.collapse(group_id);
                }
            }
            UnitKey::TurnFold(turn_id) => {
                return Some(SemanticCommandId::TranscriptTurnToggle.on_turn(turn_id));
            }
            UnitKey::Message(_) | UnitKey::Provisional(_) => {}
        }
        None
    }

    /// Flips the Session view between folded-by-default and expanded-by-default.
    fn toggle_fold_posture(&mut self) {
        if let Some(interaction) = self.current_interaction() {
            interaction.folds.borrow_mut().toggle_posture();
        }
    }

    /// Flips the Session view between collapsed-by-default and
    /// expanded-by-default Groups.
    fn toggle_group_posture(&mut self) {
        if let Some(interaction) = self.current_interaction() {
            interaction.groups.borrow_mut().toggle_posture();
        }
    }

    /// Flips the Session view between folded-by-default and
    /// expanded-by-default Turn Folds.
    fn toggle_turn_posture(&mut self) {
        if let Some(interaction) = self.current_interaction() {
            interaction.turns.borrow_mut().toggle_posture();
        }
    }

    /// Flips one settled Turn between its marker and the work behind it.
    fn toggle_turn_fold(&mut self, turn_id: TurnId) {
        if let Some(interaction) = self.current_interaction() {
            interaction.turns.borrow_mut().toggle(turn_id);
        }
    }

    /// Keeps the reader's place in the Turn they just interrupted. They were
    /// watching that work, so neither disclosure axis may close over it: the
    /// Turn Fold the Turn is about to settle into opens ahead of the Settle,
    /// and every Activity still Active opens behind it.
    fn keep_interrupted_turn_open(&mut self, turn_id: TurnId) {
        if let Some(interaction) = self.current_interaction() {
            interaction.turns.borrow_mut().expand(turn_id);
        }
        self.expand_active_activities(turn_id);
    }

    /// Folds back the Turns the reader had opened, which is what a newer Turn
    /// beginning means for the ones before it. A Turn Fold's expansion is
    /// deliberately not sticky the way a per-entry Fold's override is: the fold
    /// exists to compress past work, so moving on tidies the old expansions
    /// away — including the one an interrupt opened, which lasts only until the
    /// reader's next Turn starts.
    fn refold_expanded_turns(&mut self) {
        if let Some(interaction) = self.current_interaction() {
            interaction.turns.borrow_mut().refold_expanded_turns();
        }
    }

    /// Opens every Activity still Active in `turn_id`. An interrupted Turn
    /// leaves its work half-done, and the reader was already watching it, so
    /// the Fold must not hide what they were reading. A command opens to its
    /// Peek — the tail the reader was watching stream — while everything else
    /// expands in full.
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
            .map(|activity| (activity.id(), matches!(activity, Activity::Command { .. })))
            .collect::<Vec<_>>();
        let Some(interaction) = self.current_interaction() else {
            return;
        };
        let mut folds = interaction.folds.borrow_mut();
        for (activity_id, is_command) in active {
            if is_command {
                folds.set_step(activity_id, FoldStep::Peek);
            } else {
                folds.expand(activity_id);
            }
        }
    }

    fn follow_latest(&mut self) {
        let Some(session_id) = self.session.as_ref().map(SessionProjection::session_id) else {
            return;
        };
        let interaction = self.ensure_interaction(session_id);
        interaction.follow_latest.set(true);
        interaction.anchor.set(None);
    }

    /// Whether the composer has the keys. The Sidebar is the one surface that
    /// takes them without opening over the composer, so a composer that has
    /// them is simply one the Sidebar is not driving.
    pub(super) fn composer_focused(&self) -> bool {
        !self.sidebar.has_focus()
    }

    pub(super) fn composer_border_style(&self, theme: &Theme) -> Style {
        if self.composers.skill_issue(self.composer_key()).is_some() {
            theme.form_field.invalid
        } else if self.composer_focused() {
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
    SkillsListed {
        request: SkillCatalogRequest,
        catalog: SkillCatalog,
    },
    SkillListingFailed {
        request: SkillCatalogRequest,
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
    /// The effective settings an accepted edit left in force.
    SettingMutated(SettingsSnapshot),
    SettingMutationFailed(String),
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
    ToggleTranscriptDisclosureAt {
        position: Position,
    },
    BeginLeader,
    OpenQueuedPrompts,
    SelectPreviousQueuedPrompt,
    SelectNextQueuedPrompt,
    PromoteSelectedPrompt,
    CancelSelectedPrompt,
    RequestInterrupt,
    ConfirmInterrupt,
    CloseCommandMode,
    SelectPreviousCompletion,
    SelectNextCompletion,
    DismissCompletion,
    ConfirmSelectedCompletion,
    ActivateCompletion(CompletionMode),
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
    /// Where in the settings panel the reader pointed, which the panel resolves
    /// against the geometry the frame in force drew.
    FocusSettingsPanelAt {
        column: u16,
        screen_row: u16,
    },
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
    /// The open Session, set aside as done for now.
    SettleSession(SessionId),
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
    RefreshSkills(SkillCatalogRequest),
    ConfirmLandingAgentSelection(AgentSelection),
    UpdateAgentSelection {
        session_id: SessionId,
        request: UpdateAgentSelectionRequest,
    },
    /// One Setting's typed edit, on its way to the server that owns the file.
    MutateSetting(SettingMutation),
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
                if self.state.sidebar.is_attaching() {
                    // The Sidebar keeps its list and the reader keeps the keys:
                    // the refusal is drawn above the rows they are still on.
                    self.state.sidebar.fail_attachment(error);
                    Ok(ApplicationTransition::Continue)
                } else {
                    let request = self.state.session_picker.fail_attachment(error);
                    Ok(ApplicationTransition::ListSessions(request))
                }
            }
            ApplicationEvent::SessionDeletionFailed { session_id, error } => {
                self.state.session_picker.fail_deletion(session_id, error);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::SessionOperationFailed(error) => {
                self.state.submission_error = Some(error);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::SettingMutated(snapshot) => {
                self.state.adopt_settings(snapshot);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::SettingMutationFailed(error) => {
                self.state.settings_panel.report_failure(error);
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
                match request.surface() {
                    SessionListSurface::Picker => {
                        let current = self.session_id();
                        self.state.session_picker.load(&request, sessions, current);
                    }
                    SessionListSurface::Sidebar => {
                        let current = self.session_id();
                        self.state.sidebar.load(&request, sessions, current);
                    }
                }
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::SessionListingFailed { request, error } => {
                match request.surface() {
                    SessionListSurface::Picker => {
                        self.state.session_picker.fail_listing(&request, error);
                    }
                    SessionListSurface::Sidebar => self.state.sidebar.fail_listing(&request, error),
                }
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
            ApplicationEvent::SkillsListed { request, catalog } => {
                self.state.load_skill_catalog(request, catalog);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::SkillListingFailed { request, error: _ } => {
                if self.state.skill_catalog_request().as_ref() == Some(&request) {
                    self.state.skill_catalog = None;
                    self.state.sync_composer_completion();
                }
                Ok(ApplicationTransition::Continue)
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
        // Every command is an interaction, whatever surface raised it and even
        // where an overlay is about to swallow it: the reader looked away from
        // the Notice either way.
        self.state.landing_notice.dismiss();
        if self.state.reconnect_overlay_visible || self.defers_for_agent_selection(&command) {
            return Ok(ApplicationTransition::Continue);
        }
        match command {
            CommandId::SubmitSteer => Ok(self.submit_prompt(PromptDelivery::Steer)),
            CommandId::SubmitQueue => Ok(self.submit_prompt(PromptDelivery::Queue)),
            CommandId::InvokeSemantic(command) => self.invoke_semantic(command),
            CommandId::ActivateCompletion(mode) => {
                self.state.composer_completion.activate(mode);
                Ok(ApplicationTransition::Continue)
            }
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
            | CommandId::ToggleTranscriptDisclosureAt { .. }) => {
                self.handle_transcript_command(command)
            }
            command @ (CommandId::SelectPreviousCompletion
            | CommandId::SelectNextCompletion
            | CommandId::DismissCompletion
            | CommandId::ConfirmSelectedCompletion) => self.handle_completion_command(command),
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
            CommandId::FocusSettingsPanelAt { column, screen_row } => {
                let read =
                    self.state
                        .settings_panel
                        .focus_at(column, screen_row, &self.state.settings);
                Ok(self.answer_availability_read(read))
            }
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
        let may_retry_skills =
            matches!(&command, CommandId::InsertText(text) if text.as_str() == "$");
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
                .restore_composer_history(|composers, key| composers.history_previous(key)),
            CommandId::HistoryNext => self
                .state
                .restore_composer_history(|composers, key| composers.history_next(key)),
            _ => {}
        }
        if may_retry_skills && let Some(request) = self.state.skill_catalog_retry_request() {
            return ApplicationTransition::RefreshSkills(request);
        }
        ApplicationTransition::Continue
    }

    /// Handles the transcript navigation commands routed here; any other
    /// command leaves the viewport where it is.
    fn handle_transcript_command(&mut self, command: CommandId) -> Result<ApplicationTransition> {
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
            CommandId::ToggleTranscriptDisclosureAt { position } => {
                if let Some(invocation) = self.state.toggle_disclosure_at(position) {
                    return self.invoke_semantic(invocation);
                }
            }
            _ => {}
        }
        Ok(ApplicationTransition::Continue)
    }

    /// Handles composer completion commands; any other command leaves the
    /// suggestion list alone.
    fn handle_completion_command(&mut self, command: CommandId) -> Result<ApplicationTransition> {
        match command {
            CommandId::SelectPreviousCompletion => {
                self.state.composer_completion.select_previous();
            }
            CommandId::SelectNextCompletion => self.state.composer_completion.select_next(),
            CommandId::DismissCompletion => {
                self.state.composer_completion.dismiss_active();
            }
            CommandId::ConfirmSelectedCompletion => return self.accept_completion(),
            _ => {}
        }
        Ok(ApplicationTransition::Continue)
    }

    fn accept_completion(&mut self) -> Result<ApplicationTransition> {
        let Some(confirmation) = self.state.composer_completion.selected_confirmation() else {
            return Ok(ApplicationTransition::Continue);
        };
        match confirmation {
            CompletionConfirmation::InvokeSemantic(command) => {
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
                self.state.sync_composer_completion();
                self.invoke_semantic(command)
            }
            CompletionConfirmation::Insert {
                replacement,
                canonical,
            } => {
                let inserted = format!("{canonical} ");
                self.state.edit_composer(|composers, key| {
                    composers.replace(key, replacement, &inserted);
                });
                Ok(ApplicationTransition::Continue)
            }
            CompletionConfirmation::InsertSkill { replacement, skill } => {
                self.state.edit_composer(|composers, key| {
                    composers.insert_skill(key, replacement, &skill);
                });
                Ok(ApplicationTransition::Continue)
            }
        }
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

    /// Handles the Sidebar commands routed here; any other command leaves the
    /// Sidebar alone. A Sidebar the reader has closed is not one they are
    /// driving, so nothing routed here acts on a list nobody can see.
    fn handle_sidebar_command(&mut self, command: SemanticCommandId) -> ApplicationTransition {
        if !self.state.sidebar.is_revealed() {
            return ApplicationTransition::Continue;
        }
        match command {
            SemanticCommandId::SidebarPrevious => self.state.sidebar.select_previous(),
            SemanticCommandId::SidebarNext => self.state.sidebar.select_next(),
            SemanticCommandId::SidebarLeave => self.state.sidebar.leave(),
            SemanticCommandId::SidebarAttach => {
                let current = self.session_id();
                return self.state.sidebar.activate(current).map_or(
                    ApplicationTransition::Continue,
                    ApplicationTransition::AttachSession,
                );
            }
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
        let purpose = self.state.model_picker.purpose();
        match self.state.model_picker.choose() {
            Some(ModelPickerAction::Retry) => Ok(self.state.model_picker.begin_retry().map_or(
                ApplicationTransition::Continue,
                ApplicationTransition::ListModels,
            )),
            Some(ModelPickerAction::Select(model)) => {
                self.state.model_picker.close();
                // A Model chosen for a Setting pins the Provider and Model the
                // reader picked and nothing else, and no Options editor opens
                // over it. The picker asked them for a Model, so the pin claims
                // a Model: the Options are filled in from whatever that Model
                // defaults to wherever the Selection is resolved, which keeps
                // the pin following a Model that changes its own defaults
                // rather than freezing today's.
                if let ModelPickerPurpose::Setting(pin) = purpose {
                    return Ok(ApplicationTransition::MutateSetting(pin(AgentSelection {
                        provider: model.provider,
                        model: model.id,
                        options: Vec::new(),
                    })));
                }
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
        self.state.keep_interrupted_turn_open(turn_id);
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
        self.state.sync_composer_completion();
        if let Some(error) = self.state.composers.skill_issue(key) {
            self.state.submission_error = Some(error.to_owned());
            self.state.composer_completion.dismiss_active();
            return ApplicationTransition::Continue;
        }
        let prompt = self.state.composers.begin_submission(key);
        self.state.sync_composer_completion();
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
                    Ok(self.state.sidebar.take_listing_request().map_or(
                        ApplicationTransition::Continue,
                        ApplicationTransition::ListSessions,
                    ))
                }
            }
        }
    }

    fn attach_session(&mut self, snapshot: SessionSnapshot) -> Result<ApplicationTransition> {
        let closes_picker = self.state.session_picker.attaching_to(snapshot.session.id);
        let answers_sidebar = self.state.sidebar.attaching_to(snapshot.session.id);
        self.state.apply_attached_session(snapshot)?;
        if closes_picker {
            self.state.session_picker.close();
        }
        if answers_sidebar {
            self.state.sidebar.finish_attachment();
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
        self.state.settings_panel.adopt_catalog(request, &catalog);
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
        self.state
            .settings_panel
            .report_read_failure(request, &error);
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

    /// Runs one semantic command against what it names. Every surface that can
    /// drive the view — a keybinding, a slash command, a click, and one day a
    /// plugin — arrives here, so a behavior is defined once and invoked by
    /// its ID rather than reimplemented per input.
    fn invoke_semantic(
        &mut self,
        invocation: impl Into<SemanticInvocation>,
    ) -> Result<ApplicationTransition> {
        let invocation = invocation.into();
        let command = invocation.id;
        if self.state.selection_update_pending()
            && matches!(
                command,
                SemanticCommandId::SessionList | SemanticCommandId::SessionNew
            )
        {
            return Ok(ApplicationTransition::Continue);
        }
        // The Model picker is the one overlay that can sit above the settings
        // panel. Semantic invocations obey the same ownership as terminal
        // input: an editor hidden beneath that newer overlay accepts nothing.
        if self.state.model_picker.is_open()
            && matches!(
                command,
                SemanticCommandId::SettingsNumericInsert(_)
                    | SemanticCommandId::SettingsNumericDeleteBackward
                    | SemanticCommandId::SettingsNumericApply
                    | SemanticCommandId::SettingsNumericCancel
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
                let request = self.state.model_picker.open(
                    current.as_ref(),
                    provider_scope,
                    ModelPickerPurpose::AgentSelection,
                );
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
            command @ (SemanticCommandId::SettingsOpen
            | SemanticCommandId::SettingsPrevious
            | SemanticCommandId::SettingsNext
            | SemanticCommandId::SettingsTabPrevious
            | SemanticCommandId::SettingsTabNext
            | SemanticCommandId::SettingsRowOpen
            | SemanticCommandId::SettingsValueCycle
            | SemanticCommandId::SettingsReset
            | SemanticCommandId::SettingsClose
            | SemanticCommandId::SettingsNumericInsert(_)
            | SemanticCommandId::SettingsNumericDeleteBackward
            | SemanticCommandId::SettingsNumericApply
            | SemanticCommandId::SettingsNumericCancel) => {
                Ok(self.handle_settings_panel_command(command))
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
            SemanticCommandId::TranscriptGroupsToggle => {
                self.state.toggle_group_posture();
                self.state.command_mode = CommandMode::Composer;
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::TranscriptTurnsToggle => {
                self.state.toggle_turn_posture();
                self.state.command_mode = CommandMode::Composer;
                Ok(ApplicationTransition::Continue)
            }
            // The Turn is the command's subject, so an invocation that names
            // none has no Turn to flip and leaves the view where it is.
            SemanticCommandId::TranscriptTurnToggle => {
                if let SemanticSubject::Turn(turn_id) = invocation.subject {
                    self.state.toggle_turn_fold(turn_id);
                }
                Ok(ApplicationTransition::Continue)
            }
            // The command acts on the Session the reader is in, so on the
            // Landing there is nothing to set aside and the view stays put.
            SemanticCommandId::SessionSettle => {
                self.state.command_mode = CommandMode::Composer;
                Ok(self
                    .state
                    .session
                    .as_ref()
                    .map(SessionProjection::session_id)
                    .map_or(
                        ApplicationTransition::Continue,
                        ApplicationTransition::SettleSession,
                    ))
            }
            command @ (SemanticCommandId::SidebarPrevious
            | SemanticCommandId::SidebarNext
            | SemanticCommandId::SidebarAttach
            | SemanticCommandId::SidebarLeave) => Ok(self.handle_sidebar_command(command)),
            SemanticCommandId::SidebarToggle => {
                self.state.sidebar.toggle();
                self.state.command_mode = CommandMode::Composer;
                Ok(self.state.sidebar.take_listing_request().map_or(
                    ApplicationTransition::Continue,
                    ApplicationTransition::ListSessions,
                ))
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
                self.state.sync_composer_completion();
                Ok(if detached {
                    ApplicationTransition::DetachSession
                } else {
                    ApplicationTransition::Continue
                })
            }
        }
    }

    /// Handles the settings panel commands routed here; any other semantic
    /// command leaves the panel alone.
    ///
    /// An edit is write-through: the mutation goes out the moment the reader
    /// chooses, because a Setting is one value rather than a transaction, and
    /// the row moves when the refreshed snapshot comes back.
    fn handle_settings_panel_command(
        &mut self,
        command: SemanticCommandId,
    ) -> ApplicationTransition {
        let mutation = match command {
            SemanticCommandId::SettingsOpen => {
                self.state.settings_panel.open();
                self.state.command_mode = CommandMode::Composer;
                None
            }
            SemanticCommandId::SettingsPrevious => {
                self.state.settings_panel.select_previous();
                None
            }
            SemanticCommandId::SettingsNext => {
                self.state.settings_panel.select_next();
                None
            }
            SemanticCommandId::SettingsTabPrevious => {
                let read = self
                    .state
                    .settings_panel
                    .select_previous_tab(&self.state.settings);
                return self.answer_availability_read(read);
            }
            SemanticCommandId::SettingsTabNext => {
                let read = self
                    .state
                    .settings_panel
                    .select_next_tab(&self.state.settings);
                return self.answer_availability_read(read);
            }
            SemanticCommandId::SettingsRowOpen => {
                // A row opening onto a choosing surface leaves the panel where
                // it is and shows that surface over it, so the reader lands
                // back on the same row once they have chosen.
                if let Some(surface) = self.state.settings_panel.open_row() {
                    match surface {
                        SettingChoiceSurface::AgentSelection { current, pin } => {
                            let pinned = current(&self.state.settings);
                            let request = self.state.model_picker.open(
                                pinned.as_ref(),
                                None,
                                ModelPickerPurpose::Setting(pin),
                            );
                            return ApplicationTransition::ListModels(request);
                        }
                        SettingChoiceSurface::Numeric(choice) => {
                            self.state
                                .settings_panel
                                .open_numeric_editor(choice, &self.state.settings);
                        }
                    }
                }
                None
            }
            SemanticCommandId::SettingsValueCycle => {
                self.state.settings_panel.cycle(&self.state.settings)
            }
            SemanticCommandId::SettingsReset => self.state.settings_panel.reset(),
            SemanticCommandId::SettingsClose => {
                self.state.settings_panel.close();
                None
            }
            SemanticCommandId::SettingsNumericInsert(digit) => {
                self.state.settings_panel.insert_numeric_digit(digit);
                None
            }
            SemanticCommandId::SettingsNumericDeleteBackward => {
                self.state.settings_panel.delete_numeric_backward();
                None
            }
            SemanticCommandId::SettingsNumericApply => {
                self.state.settings_panel.apply_numeric_edit()
            }
            SemanticCommandId::SettingsNumericCancel => {
                self.state.settings_panel.cancel_numeric_edit();
                None
            }
            _ => None,
        };
        mutation.map_or(
            ApplicationTransition::Continue,
            ApplicationTransition::MutateSetting,
        )
    }

    /// Sends out the catalog listing an Availability read the panel has begun
    /// is waiting on. The request is minted through the Model picker, so the
    /// one listing answers every surface that reads the catalog rather than
    /// each of them asking the Providers separately.
    fn answer_availability_read(&mut self, read: AvailabilityRead) -> ApplicationTransition {
        match read {
            AvailabilityRead::Begun => {
                let request = self.state.model_picker.begin_refresh();
                self.state.settings_panel.await_listing(request.clone());
                ApplicationTransition::ListModels(request)
            }
            AvailabilityRead::None => ApplicationTransition::Continue,
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
        self.note_interaction(&event);
        self.command_for_terminal_input(event)
            .map_or(Ok(ApplicationTransition::Continue), |command| {
                self.handle_event(ApplicationEvent::Command(command))
            })
    }

    /// Records that the reader touched the terminal, whether or not the active
    /// input mode makes a command of it: an unbound key and a click on nothing
    /// are still interactions, and the Landing's Notice is dismissed by any of
    /// them. A resize, a focus change, and the mouse merely passing over the
    /// window are the terminal's doing rather than the reader's, so they leave
    /// the Notice standing. Reports whether anything on screen changed, so a
    /// caller that draws on demand knows to redraw.
    pub fn note_interaction(&mut self, event: &InputEvent) -> bool {
        is_reader_interaction(event) && self.state.landing_notice.dismiss()
    }

    /// Translates a terminal event through the active input mode. `None` means
    /// the event changes nothing, so callers can skip redrawing.
    pub fn command_for_terminal_input(&self, event: InputEvent) -> Option<CommandId> {
        // The Model picker is tested first because it is the one surface that
        // can now open over another: a settings panel row opens it, so while it
        // is up it is the newer surface, drawn over the panel, and every key
        // belongs to it until the reader is done choosing. The order among the
        // rest is unchanged and carries no meaning — no two of them are ever
        // open at once.
        if self.state.model_picker.is_open() {
            return command_for_model_picker_event(event);
        }
        if self.state.settings_panel.numeric_editor_is_open() {
            return command_for_numeric_editor_event(event);
        }
        if self.state.settings_panel.is_open() {
            return command_for_settings_panel_event(event);
        }
        if self.state.model_options.is_open() {
            return command_for_model_options_event(event);
        }
        if self.state.session_picker.is_open() {
            return command_for_session_picker_event(event);
        }
        // The Sidebar comes after every overlay and before the composer's own
        // surfaces: it stands beside the main view rather than over it, so an
        // overlay a reader opened is still the newer surface and owns the keys,
        // while a completion list left standing over the composer does not.
        if self.state.sidebar.has_focus() {
            return command_for_sidebar_event(event);
        }
        if self.state.composer_completion.is_visible()
            && let Some(command) = command_for_completion_event(event.clone())
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

    pub(super) fn skill_catalog_request(&self) -> Option<SkillCatalogRequest> {
        self.state.skill_catalog_request()
    }

    pub(super) fn has_skill_catalog_for(&self, request: &SkillCatalogRequest) -> bool {
        self.state
            .skill_catalog
            .as_ref()
            .is_some_and(|(loaded, _)| loaded == request)
    }

    pub(super) fn is_recovering(&self) -> bool {
        self.state.recovery.is_some()
    }

    /// Whether anything on screen is animating a Spinner, so the run loop
    /// ticks only while one shows and an idle TUI schedules zero wakeups.
    pub(super) fn wants_spinner(&self) -> bool {
        // A Provider's Availability being read is live work like any other, and
        // the row showing it animates only while the tick is armed.
        self.state.settings_panel.is_reading(&self.state.settings)
            || self.state.session.as_ref().is_some_and(|session| {
                let snapshot = session.snapshot();
                snapshot.session.status == SessionStatus::Active
                    || snapshot
                        .activities
                        .iter()
                        .any(|activity| activity.status() == Some(ActivityStatus::Active))
            })
    }

    /// Advances the Spinner one frame. Called from the run loop's tick, which
    /// only exists while [`Self::wants_spinner`] holds.
    pub(super) fn advance_spinner(&mut self) {
        self.state.spinner_frame = self.state.spinner_frame.wrapping_add(1);
    }
}

/// Whether a terminal event is the reader acting rather than the terminal
/// reporting: a key press, a click, or a paste is theirs; a resize, a focus
/// change, and the mouse merely passing over the window are not.
fn is_reader_interaction(event: &InputEvent) -> bool {
    match event {
        InputEvent::Key(key) => key.kind == KeyEventKind::Press,
        InputEvent::Mouse(mouse) => !matches!(mouse.kind, MouseEventKind::Moved),
        InputEvent::Paste(_) => true,
        InputEvent::Resize(..) | InputEvent::FocusGained | InputEvent::FocusLost => false,
    }
}
