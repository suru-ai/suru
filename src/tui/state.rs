//! Application and view state: the Session projection the TUI renders, the
//! events it accepts, and the transitions it asks the runtime to perform.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use crossterm::event::{
    Event as InputEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::{Frame, layout::Position, style::Style};

use crate::{
    managed_client::{ManagedEvent, RecoveryStatus, SessionEvent, SessionProjection},
    protocol::{
        Activity, ActivityId, ActivityStatus, AdmitPromptRequest, AgentSelection,
        AgentSelectionOperationId, CreateSessionRequest, EffectiveSettings, FoldPosture,
        InitialPrompt, MessageId, ModelCatalog, Outlook, PromptDelivery, PromptId, PromptStatus,
        ResolveWorkspaceRequest, ServerIdentity, SessionChange, SessionId, SessionListItem,
        SessionReference, SessionSnapshot, SettingMutation, SettingsSnapshot, ShutdownReason,
        SkillCatalog, SkillCatalogRequest, TextSelectionCopy, TurnId, TurnStatus,
        UpdateAgentSelectionRequest, Workspace,
    },
    provider::built_in_providers,
    settings::SettingChoiceSurface,
    terminal::TerminalFacts,
    theme::{Theme, ThemeCatalog},
};

use super::{
    commands::{SemanticCommandId, SemanticInvocation, SemanticSubject},
    completion::{CompletionConfirmation, CompletionMode, ComposerCompletion},
    composer::{ComposerKey, ComposerMemory},
    connect_overlay::ConnectOverlay,
    keymap::{
        command_for_completion_event, command_for_connect_overlay_event,
        command_for_interrupt_confirmation_event, command_for_leader_event,
        command_for_model_options_event, command_for_model_picker_event,
        command_for_numeric_editor_event, command_for_queued_prompt_event,
        command_for_serve_overlay_event, command_for_session_picker_event,
        command_for_settings_panel_event, command_for_sidebar_event,
        command_for_sidebar_menu_event, command_for_subagent_picker_event,
        command_for_subagent_view_event, command_for_terminal_event,
        command_for_theme_picker_event, command_for_workspace_picker_event,
    },
    model_options::{ModelOptions, ReasoningCycle, cycle_reasoning_effort},
    model_picker::{ModelPicker, ModelPickerAction, ModelPickerPurpose},
    notice::{ApplicationNotice, Notice},
    render::render_with_slots,
    selection::{
        SelectionCell, SelectionFrame, SelectionGranularity, SelectionSurface, TextSelection,
    },
    serve_overlay::ServeOverlay,
    session_picker::{SessionPicker, SessionPickerListing},
    settings_panel::{AvailabilityRead, SettingsPanel},
    sidebar::{Sidebar, SidebarActivation, SidebarPress},
    slots::RenderSlots,
    subagent_picker::{SubagentPicker, working_subagents},
    theme_picker::ThemePicker,
    transcript::{
        FoldDisclosure, FoldStep, MessageStart, TranscriptCache, TranscriptDisclosure,
        TranscriptFolds, TranscriptGroups, TranscriptTurnFolds, TranscriptView, UnitKey, UnitStart,
    },
    workspace_picker::WorkspacePicker,
};

/// Rows scrolled per mouse wheel tick, matching common terminal conventions.
const WHEEL_SCROLL_ROWS: usize = 3;
const INTERRUPT_CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(5);
/// How long an optimistic Session shell stays visually quiet before it asks
/// the reader to wait. This is fixed presentation behavior, not a Setting.
const OPEN_SESSION_LOADING_DELAY: Duration = Duration::from_millis(300);
/// How long after one left press a second one at the same place continues
/// the click count rather than starting it over. Fixed presentation
/// behavior, not a Setting; injectable for tests.
const CLICK_INTERVAL: Duration = Duration::from_millis(500);
/// How far, in cells on either axis, a press may land from the previous one
/// and still continue the click count.
const CLICK_SLOP: u16 = 1;
/// Click counts stop growing here: a fourth rapid click keeps what the third
/// selected.
const CLICK_COUNT_LIMIT: u8 = 3;

#[derive(Clone)]
struct PresentationClock(Arc<dyn Fn() -> Instant + Send + Sync>);

impl PresentationClock {
    fn now(&self) -> Instant {
        (self.0)()
    }
}

impl Default for PresentationClock {
    fn default() -> Self {
        Self(Arc::new(Instant::now))
    }
}

impl std::fmt::Debug for PresentationClock {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PresentationClock(..)")
    }
}

#[derive(Clone, Copy, Debug, Default)]
enum OpeningLoadingState {
    #[default]
    Inactive,
    WaitingUntil(Instant),
    Visible,
}

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
    #[cfg(test)]
    pub(super) fn toggled(&self, current_workspace: &Path) -> Self {
        match self {
            Self::CurrentWorkspace(_) => Self::AllWorkspaces,
            Self::AllWorkspaces => Self::CurrentWorkspace(current_workspace.to_owned()),
        }
    }
}

/// Which surface a Session listing answers. Several of them list Sessions at
/// once — the two pickers over the main view and the Sidebar beside it — so
/// every request names its own, and no surface can take another's reply for
/// one of its own.
///
/// The Workspace Picker lists Sessions for what they say about where work is
/// rooted rather than to offer the Sessions themselves, but it asks the same
/// question of the same server, so it asks through the same listing.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionListSurface {
    SessionPicker,
    WorkspacePicker,
    Sidebar,
}

/// One surface's request for the local Server's paired Remotes, used before
/// an Everywhere listing can ask each resulting Origin for its Sessions.
/// The surface is part of the identity because the Sidebar and Session picker
/// own independent scope and request sequences.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EverywhereListRequest {
    surface: SessionListSurface,
    id: u64,
}

impl EverywhereListRequest {
    pub(super) const fn new(surface: SessionListSurface, id: u64) -> Self {
        Self { surface, id }
    }

    pub fn surface(self) -> SessionListSurface {
        self.surface
    }

    pub(super) const fn id(self) -> u64 {
        self.id
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum WorkspaceResolutionSurface {
    Outlook,
    WorkspacePicker,
    Sidebar,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionListRequest {
    surface: SessionListSurface,
    id: u64,
    outlook: Outlook,
    pub(super) scope: SessionListScope,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelListRequest {
    sequence: u64,
    outlook: Outlook,
}

impl ModelListRequest {
    pub(super) const fn new(sequence: u64, outlook: Outlook) -> Self {
        Self { sequence, outlook }
    }

    pub fn outlook(&self) -> &Outlook {
        &self.outlook
    }
}

impl SessionListRequest {
    pub(super) fn new(
        surface: SessionListSurface,
        id: u64,
        outlook: Outlook,
        scope: SessionListScope,
    ) -> Self {
        Self {
            surface,
            id,
            outlook,
            scope,
        }
    }

    pub fn scope(&self) -> &SessionListScope {
        &self.scope
    }

    pub fn surface(&self) -> SessionListSurface {
        self.surface
    }

    pub fn outlook(&self) -> &Outlook {
        &self.outlook
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
    left_press: Option<LeftPress>,
    last_click: Option<LastClick>,
    click_interval: Duration,
    pub(super) text_selection: Cell<Option<TextSelection>>,
    pub(super) selection_frames: RefCell<Vec<SelectionFrame>>,
    pub(super) selection_overlay_area: Cell<Option<ratatui::layout::Rect>>,
    pub(super) outlook: Outlook,
    outlook_workspaces: HashMap<Outlook, PathBuf>,
    workspace_resolution_sequence: u64,
    pending_workspace_resolutions: HashMap<WorkspaceResolutionSurface, u64>,
    pub(super) identity: Option<ServerIdentity>,
    pub(super) recovery: Option<RecoveryStatus>,
    /// Manual stop preserves the last confirmed identity as useful final context.
    pub(super) manually_stopped: bool,
    pub(super) fatal_error: Option<String>,
    pub(super) workspace: PathBuf,
    pub(super) composers: ComposerMemory,
    pub(super) questionnaires: super::questionnaire::QuestionnairePanels,
    session_interactions: HashMap<SessionReference, SessionInteraction>,
    /// The effective value of every Setting, as the server last pushed it.
    /// Client Settings govern presentation from here. The snapshot leads the
    /// lifecycle stream, so it is in hand before any Session view opens.
    settings: EffectiveSettings,
    /// The first frame waits for the server's authoritative Settings rather
    /// than briefly painting built-in defaults before the leading snapshot.
    settings_received: bool,
    /// The dotted keys a Config Document pins, from the same snapshot, so the
    /// settings panel can tell the reader's own choice from a built-in default.
    pinned_settings: Vec<String>,
    /// What the Landing has to say about the configuration problems startup
    /// found. It is a Notice, not state the run depends on: the reader's next
    /// interaction takes it away for good.
    application_notice: ApplicationNotice,
    pub(super) transcript_cache: TranscriptCache,
    /// Bumped whenever the Session projection is replaced wholesale, so the
    /// transcript cache never trusts a revision across snapshot swaps.
    pub(super) transcript_generation: u64,
    /// Which Spinner frame is showing, advanced by the run loop's tick and
    /// read only at draw time — never by the transcript projection (ADR 0009).
    pub(super) spinner_frame: usize,
    /// Whether the last frame actually drew current-Session animation. A
    /// Working Indicator that scrolled away cannot justify 32ms redraws.
    pub(super) session_animation_on_screen: Cell<bool>,
    /// When each visible Active Command first appeared to this client. Time
    /// stays out of transcript projection; the spinner tick reads these ages
    /// and writes Fold overrides only when a threshold is crossed.
    active_commands_started_at: HashMap<ActivityId, Instant>,
    presentation_clock: PresentationClock,
    pub(super) submission_error: Option<String>,
    /// The Session the main view has open, and `None` on the Landing.
    ///
    /// This is the route the reader chose rather than a question about what
    /// has loaded: opening a Session sets it on the frame they ask, and the
    /// projection below catches up when the snapshot lands. Every reading of
    /// *which* Session the reader is in comes from here — the Sidebar's open
    /// highlight, the composer the keys write into, and what the Sidebar's
    /// own Enter takes to be the Session already open.
    pub(super) route: Option<SessionReference>,
    /// The optimistic shell's fixed quiet period and visible feedback. A
    /// reached deadline remains `WaitingUntil` until its wakeup wins selection,
    /// so another ready event cannot silently cancel the redraw it is owed.
    opening_loading: OpeningLoadingState,
    /// The Session the main view draws from, present only once the route has
    /// hydrated. A route without one is a Session still loading, and
    /// everything a Session is read for — its header, Transcript, Working
    /// state, usage, footer, and Prompt delivery — stands down until the
    /// snapshot arrives rather than answering for the Session left behind.
    pub(super) session: Option<SessionProjection>,
    /// Why the open route could not hydrate, if its newest attachment failed.
    ///
    /// This is client presentation, not Session history: it never enters a
    /// snapshot, Activity, Turn, or protocol event. Keeping it beside the
    /// optimistic route lets that route retain its composer while the main
    /// content speaks the refusal in the Transcript's visual language.
    pub(super) opening_error: Option<String>,
    /// The origin-qualified reference of the projection above, and so `Some`
    /// exactly when it is.
    pub(super) session_reference: Option<SessionReference>,
    outlook_landing_selections: HashMap<Outlook, Option<AgentSelection>>,
    landing_agent_selection: Option<AgentSelection>,
    confirmed_landing_agent_selection: Option<AgentSelection>,
    pending_landing_agent_selection: Option<AgentSelection>,
    queued_landing_agent_selection: Option<AgentSelection>,
    pending_agent_selection: Option<PendingAgentSelection>,
    /// Newest complete Agent Selection awaiting the in-flight request; rapid
    /// cycles coalesce here so transport stays serialized per Session.
    queued_agent_selection: Option<(SessionReference, AgentSelection)>,
    confirmed_agent_selection: Option<(SessionReference, AgentSelection)>,
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
    pub(super) theme_picker: ThemePicker,
    pub(super) session_picker: SessionPicker,
    pub(super) workspace_picker: WorkspacePicker,
    pub(super) subagent_picker: SubagentPicker,
    pub(super) connect_overlay: ConnectOverlay,
    pub(super) serve_overlay: ServeOverlay,
    pub(super) sidebar: Sidebar,
    pub(super) settings_panel: SettingsPanel,
}

enum OutlookTurn {
    Deliberate,
    SessionRow { fallback_workspace: PathBuf },
}

#[derive(Clone, Debug)]
struct PendingSubmission {
    source: ComposerKey,
    target: SubmissionTarget,
    prompt: InitialPrompt,
}

#[derive(Clone, Debug)]
struct PendingAgentSelection {
    session: SessionReference,
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
    session: SessionReference,
    prompt: InitialPrompt,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SubmissionTarget {
    CreateSession,
    AdmitPrompt(SessionReference, PromptDelivery),
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
        /// The active Turn the interrupt will reach, kept so the reader's
        /// place in it survives the settle — or `None` when only working
        /// Subagents keep the Session going and the interrupt is theirs.
        turn_id: Option<TurnId>,
        armed_at: Instant,
    },
}

impl Default for TuiState {
    fn default() -> Self {
        Self::new(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }
}

impl TuiState {
    pub(super) fn new(workspace: impl AsRef<Path>) -> Self {
        // Taken as the one reading at launch, so a spelling reached through a
        // symlink — or Windows's own `current_dir`, which never matches the
        // canonical form — cannot narrow a current-Workspace scope to a
        // Workspace none of this client's Sessions match.
        let workspace = workspace_reading(workspace.as_ref());
        Self {
            left_press: None,
            last_click: None,
            click_interval: CLICK_INTERVAL,
            text_selection: Cell::new(None),
            selection_frames: RefCell::new(Vec::new()),
            selection_overlay_area: Cell::new(None),
            outlook: Outlook::Local,
            outlook_workspaces: HashMap::from([(Outlook::Local, workspace.clone())]),
            workspace_resolution_sequence: 0,
            pending_workspace_resolutions: HashMap::new(),
            identity: None,
            recovery: None,
            manually_stopped: false,
            fatal_error: None,
            workspace: workspace.clone(),
            composers: ComposerMemory::default(),
            questionnaires: super::questionnaire::QuestionnairePanels::default(),
            session_interactions: HashMap::new(),
            settings: EffectiveSettings::default(),
            settings_received: false,
            pinned_settings: Vec::new(),
            application_notice: ApplicationNotice::default(),
            transcript_cache: TranscriptCache::default(),
            transcript_generation: 0,
            spinner_frame: 0,
            session_animation_on_screen: Cell::new(false),
            active_commands_started_at: HashMap::new(),
            presentation_clock: PresentationClock::default(),
            submission_error: None,
            route: None,
            opening_loading: OpeningLoadingState::Inactive,
            session: None,
            opening_error: None,
            session_reference: None,
            outlook_landing_selections: HashMap::from([(Outlook::Local, None)]),
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
            theme_picker: ThemePicker::default(),
            session_picker: SessionPicker::new(workspace.clone()),
            workspace_picker: WorkspacePicker::new(workspace.clone()),
            subagent_picker: SubagentPicker::default(),
            connect_overlay: ConnectOverlay::default(),
            serve_overlay: ServeOverlay::default(),
            sidebar: Sidebar::new(workspace),
            settings_panel: SettingsPanel::default(),
        }
    }

    /// Takes the Workspace this client works in, which the reader moved by
    /// choosing one in the Workspace Picker or by naming a directory in the
    /// Sidebar.
    ///
    /// It is where the Sessions they make next are rooted, the Workspace the
    /// Skill Catalog answers for, what current-Workspace scope comes to mean,
    /// and the base a relative path is read from, so every surface holding a
    /// reading of it is given the new one here rather than being left to
    /// answer for a Workspace the reader has left.
    ///
    /// Adoption is all this is. The Sidebar's own scope is the reader's to
    /// choose, and the path entry re-points it separately, so nothing here
    /// rearranges a column they configured.
    fn adopt_workspace(&mut self, workspace: PathBuf) {
        self.outlook_workspaces
            .insert(self.outlook.clone(), workspace.clone());
        self.workspace = workspace.clone();
        self.session_picker.adopt_workspace(workspace.clone());
        self.workspace_picker.adopt_workspace(workspace.clone());
        self.sidebar.adopt_workspace(workspace);
    }

    /// Stops waiting on a Session being opened, on every surface that can be
    /// waiting. The work itself is the run loop's to let go of; this is the
    /// presentation that went with it.
    fn abandon_pending_attachment(&mut self) {
        self.sidebar.abandon_attachment();
        self.session_picker.abandon_attachment();
    }

    /// Carries the reader into `target` at once, before anything of it has
    /// loaded.
    ///
    /// Opening a Session is asynchronous, and waiting on it would leave the
    /// reader looking at the Session they have just left. So the route moves
    /// now: the main view is the target's from this frame, the Sidebar's
    /// highlight goes with it, and the composer the keys write into is the
    /// target's own — a draft typed into it is kept under that Session and no
    /// other.
    ///
    /// Everything a Session is read for goes with the route rather than
    /// standing in for the target. The projection the reader was reading is
    /// let go of rather than redrawn under a Session it is not, and no
    /// projection is invented from the listing summary that named the target:
    /// a summary says a Title and a timestamp, which is not a Session. The
    /// shell holds the target's composer and nothing else until the snapshot
    /// lands.
    ///
    /// The Session left behind keeps its own stream until then — the run loop
    /// holds it open so the target's arrival is the one moment the client
    /// swaps — and what it sends is taken and dropped rather than drawn,
    /// because a projection under this route would be the wrong Session.
    fn open_session_route(&mut self, target: SessionReference) {
        self.text_selection.set(None);
        self.left_press = None;
        self.session = None;
        self.session_reference = None;
        self.route = Some(target);
        self.opening_error = None;
        let deadline = self
            .presentation_clock
            .now()
            .checked_add(OPEN_SESSION_LOADING_DELAY)
            .expect("the opening loading delay fits a monotonic clock");
        self.opening_loading = OpeningLoadingState::WaitingUntil(deadline);
        self.session_events_blocked = true;
        // The picker browses the Session that was open, and the reader has
        // left it; the Subagents it offered are not the target's.
        self.subagent_picker.close();
        self.command_mode = CommandMode::Composer;
        self.submission_error = None;
        self.sync_composer_completion();
    }

    /// Leaves the Session the main view had open, for the Landing or for
    /// wherever the client is going next, and answers whether there was one.
    ///
    /// The route goes with the projection. A Session still loading is one the
    /// reader is on their way to, and leaving is them saying they are not
    /// going after all — so it is left behind exactly as a hydrated one is.
    fn leave_session_route(&mut self) -> bool {
        self.text_selection.set(None);
        self.left_press = None;
        self.session = None;
        self.session_reference = None;
        self.opening_error = None;
        self.stop_opening_loading();
        self.route.take().is_some()
    }

    fn stop_opening_loading(&mut self) {
        self.opening_loading = OpeningLoadingState::Inactive;
    }

    /// Keeps the optimistic route open while replacing its progress feedback
    /// with the client-local reason it could not hydrate.
    fn fail_opening_session(&mut self, error: String) {
        self.stop_opening_loading();
        self.opening_error = Some(format!("Could not load Session: {error}"));
    }

    fn turn_outlook(&mut self, outlook: Outlook) {
        self.turn_outlook_with(outlook, OutlookTurn::Deliberate);
    }

    /// Turns toward the Origin of a Session already present in a merged
    /// listing, then carries the reader into its optimistic route. The row's
    /// Workspace is authoritative when this Client has not visited that
    /// Outlook before; unlike a deliberate Connect turn, no resolution is
    /// needed because the listing already came from that Origin.
    fn turn_outlook_for_session(&mut self, target: SessionReference, workspace: PathBuf) {
        let remembers_workspace = self.outlook_workspaces.contains_key(&target.origin);
        self.turn_outlook_with(
            target.origin.clone(),
            OutlookTurn::SessionRow {
                fallback_workspace: workspace,
            },
        );
        if !remembers_workspace {
            self.outlook_workspaces
                .insert(target.origin.clone(), self.workspace.clone());
        }
        self.open_session_route(target);
    }

    fn turn_outlook_with(&mut self, outlook: Outlook, turn: OutlookTurn) {
        if self.outlook == outlook {
            return;
        }
        let (fallback_workspace, adopt_sidebar): (PathBuf, fn(&mut Sidebar, Outlook)) = match turn {
            OutlookTurn::Deliberate => (PathBuf::from("."), Sidebar::adopt_outlook),
            OutlookTurn::SessionRow { fallback_workspace } => {
                (fallback_workspace, Sidebar::adopt_outlook_from_row)
            }
        };
        self.outlook_landing_selections
            .insert(self.outlook.clone(), self.landing_agent_selection.clone());
        self.outlook = outlook.clone();
        self.pending_workspace_resolutions.clear();
        self.workspace = self
            .outlook_workspaces
            .get(&outlook)
            .cloned()
            .unwrap_or(fallback_workspace);
        self.leave_session_route();
        self.session_events_blocked = true;
        self.pending_submission = None;
        self.pending_steers.clear();
        self.pending_agent_selection = None;
        self.queued_agent_selection = None;
        self.confirmed_agent_selection = None;
        self.landing_agent_selection = self
            .outlook_landing_selections
            .get(&outlook)
            .cloned()
            .flatten();
        self.confirmed_landing_agent_selection = self.landing_agent_selection.clone();
        self.pending_landing_agent_selection = None;
        self.queued_landing_agent_selection = None;
        self.skill_catalog = None;
        self.model_picker.adopt_outlook(outlook.clone());
        self.model_options = ModelOptions::default();
        self.pending_model_options = false;
        self.session_picker.adopt_workspace(self.workspace.clone());
        self.workspace_picker
            .adopt_workspace(self.workspace.clone());
        self.sidebar.adopt_workspace(self.workspace.clone());
        self.session_picker.adopt_outlook(outlook.clone());
        self.workspace_picker.adopt_outlook(outlook.clone());
        adopt_sidebar(&mut self.sidebar, outlook);
        self.command_mode = CommandMode::Composer;
        self.submission_error = None;
        self.sync_composer_completion();
    }

    /// The union of Remote catalog streams required by independently scoped
    /// Session surfaces. One surface narrowing must not release another
    /// surface's Everywhere interest.
    fn catalog_origins(&self) -> HashSet<Outlook> {
        let mut origins = self.sidebar.catalog_origins();
        origins.extend(self.session_picker.catalog_origins());
        origins
    }

    fn begin_workspace_resolution(&mut self, surface: WorkspaceResolutionSurface) -> u64 {
        self.workspace_resolution_sequence = self.workspace_resolution_sequence.wrapping_add(1);
        if surface != WorkspaceResolutionSurface::Outlook {
            self.pending_workspace_resolutions
                .remove(&WorkspaceResolutionSurface::Outlook);
        }
        let id = self.workspace_resolution_sequence;
        self.pending_workspace_resolutions.insert(surface, id);
        id
    }

    fn accept_workspace_resolution(
        &mut self,
        surface: WorkspaceResolutionSurface,
        id: u64,
    ) -> bool {
        if self.pending_workspace_resolutions.get(&surface) != Some(&id) {
            return false;
        }
        self.pending_workspace_resolutions.remove(&surface);
        true
    }

    fn cancel_workspace_resolution(&mut self, surface: WorkspaceResolutionSurface) -> bool {
        self.pending_workspace_resolutions
            .remove(&surface)
            .is_some()
    }

    fn sync_composer_completion(&mut self) {
        let key = self.composer_key();
        let catalog = self.current_skill_catalog().cloned();
        self.composers.resolve_skills(key.clone(), catalog.as_ref());
        let bound_skills = self.composers.valid_skill_ids(key.clone());
        self.composer_completion.sync(
            self.composers.text(key.clone()),
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
        // A Session still loading says neither the Agent a Catalog is asked
        // for nor the Workspace it is asked about, and the Landing it has
        // already left answers for neither. Skill resolution waits with every
        // other act that needs the Session.
        if self.open_session_is_loading() {
            return None;
        }
        let selection = self.agent_selection()?;
        let workspace = self.session.as_ref().map_or_else(
            || self.workspace.clone(),
            |session| session.snapshot().session.workspace.path.clone(),
        );
        let workspace = match self.outlook {
            Outlook::Local => workspace_reading(&workspace),
            Outlook::Remote(_) => workspace,
        };
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
        self.reconcile_questionnaire_catalog(&Outlook::Local, &event);
        // The managed catalog stream belongs to the local Server. A Remote
        // Outlook has its own main view and pickers, but Everywhere still
        // keeps the local Origin's Sidebar rows live in the background.
        if self.outlook != Outlook::Local
            && (event.moves_the_session_catalog()
                || matches!(&event, ManagedEvent::SkillCatalogUpdated(_)))
        {
            if event.moves_the_session_catalog() {
                if self.sidebar.includes_origin(&Outlook::Local) {
                    self.apply_sidebar_catalog_event(&Outlook::Local, &event);
                }
                if self.session_picker.includes_origin(&Outlook::Local) {
                    self.apply_session_picker_catalog_event(&Outlook::Local, &event);
                }
            }
            return;
        }
        self.apply_managed_event(event);
    }

    fn apply_origin_catalog(&mut self, outlook: &Outlook, event: ManagedEvent) {
        self.reconcile_questionnaire_catalog(outlook, &event);
        if self.sidebar.includes_origin(outlook) {
            match &event {
                ManagedEvent::Recovering(_) => self.sidebar.mark_origin_recovering(outlook.clone()),
                ManagedEvent::RemoteRecovered | ManagedEvent::SessionCatalogReconciled(_) => {
                    self.sidebar.mark_origin_catalog_current(outlook)
                }
                _ => {}
            }
        }
        if outlook == &self.outlook {
            self.apply_managed_event(event);
        } else if event.moves_the_session_catalog() {
            if self.sidebar.includes_origin(outlook) {
                self.apply_sidebar_catalog_event(outlook, &event);
            }
            if self.session_picker.includes_origin(outlook) {
                self.apply_session_picker_catalog_event(outlook, &event);
            }
        }
    }

    fn reconcile_questionnaire_catalog(&mut self, origin: &Outlook, event: &ManagedEvent) {
        match event {
            ManagedEvent::SessionStandingInputsChanged(changed) => {
                self.questionnaires.reconcile_subagents(
                    &SessionReference::new(origin.clone(), changed.session_id),
                    &changed.inputs.subagent_questionnaires,
                );
                self.questionnaires.reconcile_available(
                    &SessionReference::new(origin.clone(), changed.session_id),
                    changed.inputs.pending_questionnaires_revision,
                    &changed.inputs.pending_questionnaires,
                    &changed.inputs.submitting_questionnaires,
                )
            }
            ManagedEvent::SessionDeleted(deleted) => self
                .questionnaires
                .discard_session(&SessionReference::new(origin.clone(), deleted.session_id)),
            ManagedEvent::SessionCatalogReconciled(snapshot) => self
                .questionnaires
                .retain_origin(origin, &snapshot.session_ids),
            _ => {}
        }
    }

    fn settle_remote_failure(&mut self, message: String) {
        self.recovery = None;
        self.reconnect_overlay_visible = false;
        self.submission_error = Some(message);
    }

    fn apply_managed_event(&mut self, event: ManagedEvent) {
        // Every change the session-catalog stream reports leaves the Sidebar
        // asking the server for its listing again. What the change says is
        // taken in place below, so the frame is right before the answer lands;
        // the ask is what carries everything the change does not say — a new
        // Session's Title and Workspace, and the last activity the server
        // moves as a Session is unsettled, most of all.
        let outlook = self.outlook.clone();
        self.apply_sidebar_catalog_event(&outlook, &event);
        if event.moves_the_session_catalog() {
            self.session_picker.catch_up_origin(outlook);
        }
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
                self.outlook_landing_selections
                    .insert(Outlook::Local, health.landing_agent_selection.clone());
                let replaced_server = self
                    .identity
                    .as_ref()
                    .is_some_and(|identity| identity.instance_id != health.instance_id);
                if replaced_server {
                    self.questionnaires.discard_origin(&self.outlook);
                    if let Some(reference) = self.session_reference.clone() {
                        self.composers.recover_session_to_landing(reference.clone());
                        self.session_interactions.remove(&reference);
                        self.pending_steers
                            .retain(|steer| steer.session != reference);
                    }
                    self.leave_session_route();
                    self.pending_agent_selection = None;
                    self.queued_agent_selection = None;
                    self.confirmed_agent_selection = None;
                    self.session_events_blocked = true;
                    self.submission_error =
                        Some("Session ended because the shared server was replaced".to_owned());
                    self.sync_composer_completion();
                }
                if self.session.is_none() && self.outlook == Outlook::Local {
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
            ManagedEvent::RemoteRecovered => {
                self.recovery = None;
                self.reconnect_overlay_visible = false;
            }
            ManagedEvent::RemoteFailed { message, .. } => self.settle_remote_failure(message),
            ManagedEvent::ServerShutdown(shutdown) => {
                self.stop_opening_loading();
                if shutdown.reason == ShutdownReason::Manual {
                    self.recovery = None;
                    self.reconnect_overlay_visible = false;
                    self.manually_stopped = true;
                    self.fatal_error = None;
                }
            }
            // A creation says an id and nothing a row is drawn from, so there
            // is nothing to take in place: the catch-up above is the whole of
            // the Sidebar's answer to it.
            ManagedEvent::SessionCatalogInvalidated { .. } | ManagedEvent::SessionCreated(_) => {}
            ManagedEvent::SessionDeleted(deleted) => {
                self.session_picker.remove(deleted.session_id);
                self.remove_deleted_session(deleted.session_id);
            }
            ManagedEvent::SessionTitleChanged(retitled) => {
                self.session_picker.retitle(
                    retitled.session_id,
                    retitled.title.clone(),
                    retitled.emoji.clone(),
                );
            }
            ManagedEvent::SessionSettlementChanged(settled) => {
                self.session_picker
                    .settle(settled.session_id, settled.settled_at);
            }
            // Pending Questionnaires update the open picker immediately, just
            // as they update the Sidebar, without moving its selection.
            ManagedEvent::SessionStandingInputsChanged(changed) => {
                self.session_picker.set_standing_inputs(
                    self.outlook.clone(),
                    changed.session_id,
                    changed.inputs.clone(),
                );
            }
            ManagedEvent::SessionWorkingChanged(_) => {}
            // Neither listing surface states a Session's total yet — the
            // footer of the Session in view reads its own — so the roll-up
            // the catalog announces moves nothing this client draws.
            ManagedEvent::SessionUsageChanged(_) => {}
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

    /// Applies the part of a catalog event that belongs to the merged Sidebar.
    /// The Origin is explicit because Session ids are only unique within it.
    fn apply_sidebar_catalog_event(&mut self, outlook: &Outlook, event: &ManagedEvent) {
        if !event.moves_the_session_catalog() {
            return;
        }
        self.sidebar.catch_up_origin(outlook.clone());
        match event {
            ManagedEvent::SessionCatalogInvalidated { .. }
            | ManagedEvent::SessionCreated(_)
            | ManagedEvent::SessionUsageChanged(_) => {}
            ManagedEvent::SessionDeleted(deleted) => {
                self.sidebar
                    .remove_origin(outlook.clone(), deleted.session_id);
            }
            ManagedEvent::SessionTitleChanged(retitled) => self.sidebar.retitle_origin(
                outlook.clone(),
                retitled.session_id,
                retitled.title.clone(),
                retitled.emoji.clone(),
            ),
            ManagedEvent::SessionSettlementChanged(settled) => {
                self.sidebar
                    .settle_origin(outlook.clone(), settled.session_id, settled.settled_at)
            }
            ManagedEvent::SessionWorkingChanged(working) => self.sidebar.set_working_origin(
                outlook.clone(),
                working.session_id,
                working.working_since,
            ),
            ManagedEvent::SessionStandingInputsChanged(changed) => {
                self.sidebar.set_standing_inputs_origin(
                    outlook.clone(),
                    changed.session_id,
                    changed.inputs.clone(),
                )
            }
            ManagedEvent::SessionCatalogReconciled(snapshot) => self
                .sidebar
                .retain_origin_catalog(outlook.clone(), &snapshot.session_ids),
            ManagedEvent::Connecting
            | ManagedEvent::Connected(_)
            | ManagedEvent::SettingsSnapshot(_)
            | ManagedEvent::SkillCatalogUpdated(_)
            | ManagedEvent::Recovering(_)
            | ManagedEvent::RemoteRecovered
            | ManagedEvent::RemoteFailed { .. }
            | ManagedEvent::ServerShutdown(_)
            | ManagedEvent::Fatal(_) => {}
        }
    }

    /// Applies the catalog facts a Session picker already on screen can take
    /// in place for one Origin. Creation and Usage carry no row presentation,
    /// and Working is not drawn by this picker.
    fn apply_session_picker_catalog_event(&mut self, outlook: &Outlook, event: &ManagedEvent) {
        if event.moves_the_session_catalog() {
            self.session_picker.catch_up_origin(outlook.clone());
        }
        match event {
            ManagedEvent::SessionCatalogInvalidated { .. }
            | ManagedEvent::SessionCreated(_)
            | ManagedEvent::SessionWorkingChanged(_)
            | ManagedEvent::SessionUsageChanged(_) => {}
            ManagedEvent::SessionStandingInputsChanged(changed) => {
                self.session_picker.set_standing_inputs(
                    outlook.clone(),
                    changed.session_id,
                    changed.inputs.clone(),
                );
            }
            ManagedEvent::SessionDeleted(deleted) => self
                .session_picker
                .remove_origin(outlook.clone(), deleted.session_id),
            ManagedEvent::SessionTitleChanged(retitled) => self.session_picker.retitle_origin(
                outlook.clone(),
                retitled.session_id,
                retitled.title.clone(),
                retitled.emoji.clone(),
            ),
            ManagedEvent::SessionSettlementChanged(settled) => self.session_picker.settle_origin(
                outlook.clone(),
                settled.session_id,
                settled.settled_at,
            ),
            ManagedEvent::SessionCatalogReconciled(snapshot) => self
                .session_picker
                .retain_origin_catalog(outlook.clone(), &snapshot.session_ids),
            ManagedEvent::Connecting
            | ManagedEvent::Connected(_)
            | ManagedEvent::SettingsSnapshot(_)
            | ManagedEvent::SkillCatalogUpdated(_)
            | ManagedEvent::Recovering(_)
            | ManagedEvent::RemoteRecovered
            | ManagedEvent::RemoteFailed { .. }
            | ManagedEvent::ServerShutdown(_)
            | ManagedEvent::Fatal(_) => {}
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
        self.settings_received = true;
        self.pinned_settings = snapshot.pinned;
        if self
            .settings
            .transcript
            .command_auto_expand
            .after_millis()
            .is_none()
        {
            for interaction in self.session_interactions.values() {
                interaction.folds.borrow_mut().clear_automatic_promotions();
            }
        }
        // The Settings the Sidebar and the session picker draw under are the
        // ones that act on arrival rather than on the next view opened,
        // because the frames they govern may be on screen already.
        self.sidebar.adopt_settings(&self.settings);
        self.session_picker.adopt_settings(&self.settings);
        self.application_notice.receive(&snapshot.diagnostics);
    }

    pub(super) fn settings(&self) -> &EffectiveSettings {
        &self.settings
    }

    pub(super) fn pinned_settings(&self) -> &[String] {
        &self.pinned_settings
    }

    pub(super) fn application_notice(&self) -> Option<&Notice> {
        self.application_notice.showing()
    }

    fn remove_deleted_session(&mut self, deleted_session_id: SessionId) {
        self.questionnaires.discard_session(&SessionReference {
            origin: self.outlook.clone(),
            session_id: deleted_session_id,
        });
        let Some(reference) = self
            .session_reference
            .as_ref()
            .filter(|reference| reference.session_id == deleted_session_id)
            .cloned()
        else {
            return;
        };
        self.composers.discard_session(reference.clone());
        self.session_interactions.remove(&reference);
        self.pending_steers
            .retain(|steer| steer.session != reference);
        if self.pending_submission.as_ref().is_some_and(|submission| {
            matches!(
                &submission.target,
                SubmissionTarget::AdmitPrompt(session, _)
                    if session == &reference
            )
        }) {
            self.pending_submission = None;
        }
        self.failed_submissions.retain(|_, submission| {
            !matches!(
                &submission.target,
                SubmissionTarget::AdmitPrompt(session, _)
                    if session == &reference
            )
        });
        if self
            .pending_agent_selection
            .as_ref()
            .is_some_and(|pending| pending.session == reference)
        {
            self.pending_agent_selection = None;
        }
        if self
            .queued_agent_selection
            .as_ref()
            .is_some_and(|(session, _)| session == &reference)
        {
            self.queued_agent_selection = None;
        }
        if self
            .confirmed_agent_selection
            .as_ref()
            .is_some_and(|(session, _)| session == &reference)
        {
            self.confirmed_agent_selection = None;
        }
        self.leave_session_route();
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
            SessionEvent::Snapshot(snapshot) => self.hydrate_session(*snapshot),
            SessionEvent::Updated(update) => {
                let Some(session) = self.session.as_mut() else {
                    return Err(anyhow!("Session update arrived before its snapshot"));
                };
                session.apply(update)?;
            }
        }
        self.reconcile_active_command_starts();
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
        self.reconcile_subagent_picker();
        Ok(())
    }

    /// The root Session to report Viewed when this event Settles one of its
    /// Turns or replaces it with a recovery snapshot. A fresh snapshot closes
    /// the gap while the subscription was down; neither case applies after
    /// navigation has moved away or while a Subagent is open.
    fn open_root_viewed_by(&self, event: &SessionEvent) -> Option<SessionReference> {
        if self.session_events_blocked
            || self
                .session
                .as_ref()
                .is_none_or(|session| session.snapshot().session.is_subagent())
        {
            return None;
        }
        let viewed = match event {
            SessionEvent::Snapshot(_) => true,
            SessionEvent::Updated(update) => update.changes.iter().any(|change| match change {
                SessionChange::TurnAdded { turn } => turn.status.is_terminal(),
                SessionChange::TurnStatusChanged { status, .. } => status.is_terminal(),
                _ => false,
            }),
        };
        viewed.then(|| self.session_reference.clone()).flatten()
    }

    fn apply_created_session(&mut self, snapshot: SessionSnapshot) -> Result<()> {
        self.session_events_blocked = false;
        self.apply_session(SessionEvent::snapshot(snapshot))
    }

    fn apply_attached_session(&mut self, snapshot: SessionSnapshot) -> Result<()> {
        let reference = SessionReference::new(self.outlook.clone(), snapshot.session.id);
        for questionnaire in super::questionnaire::pending(&snapshot) {
            self.questionnaires
                .reconcile_submission(&reference, questionnaire.id, &snapshot);
        }
        self.session_events_blocked = false;
        self.apply_session(SessionEvent::snapshot(snapshot))
    }

    fn hydrate_session(&mut self, snapshot: SessionSnapshot) {
        self.text_selection.set(None);
        self.left_press = None;
        // The picker browses the Session that was open, so a swap to another
        // one takes it away rather than leaving it standing over rows it
        // never offered.
        if self.session.as_ref().map(SessionProjection::session_id) != Some(snapshot.session.id) {
            self.subagent_picker.close();
        }
        let reference = SessionReference::new(self.outlook.clone(), snapshot.session.id);
        self.ensure_interaction(reference.clone());
        self.submission_error = None;
        // The snapshot is the route as much as the projection: a Session
        // reached without an optimistic opening — one just created, or one
        // picked from an overlay — arrives as both at once.
        self.route = Some(reference.clone());
        self.stop_opening_loading();
        self.opening_error = None;
        self.session_reference = Some(reference);
        self.session = Some(SessionProjection::new(snapshot));
        self.transcript_generation = self.transcript_generation.wrapping_add(1);
        self.sync_composer_completion();
    }

    /// Records the first client-side sighting of every Active Command and
    /// forgets clocks for Commands that settled or left the visible Session.
    fn reconcile_active_command_starts(&mut self) {
        let active = self
            .session
            .as_ref()
            .into_iter()
            .flat_map(|session| &session.snapshot().activities)
            .filter_map(|activity| match activity {
                Activity::Command {
                    id,
                    status: ActivityStatus::Active,
                    ..
                } => Some(*id),
                _ => None,
            })
            .collect::<HashSet<_>>();
        self.active_commands_started_at
            .retain(|activity_id, _| active.contains(activity_id));
        let now = self.presentation_clock.now();
        for activity_id in active.iter().copied() {
            self.active_commands_started_at
                .entry(activity_id)
                .or_insert(now);
        }
        if let Some(interaction) = self.current_interaction() {
            interaction
                .folds
                .borrow_mut()
                .retain_automatic_promotions(&active);
        }
    }

    fn promote_aged_commands(&mut self) {
        let Some(threshold_ms) = self.settings.transcript.command_auto_expand.after_millis() else {
            return;
        };
        let now = self.presentation_clock.now();
        let ready = self
            .active_commands_started_at
            .iter()
            .filter_map(|(activity_id, started_at)| {
                (now.saturating_duration_since(*started_at).as_millis() >= u128::from(threshold_ms))
                    .then_some(*activity_id)
            })
            .collect::<Vec<_>>();
        let Some(interaction) = self.current_interaction() else {
            return;
        };
        let mut folds = interaction.folds.borrow_mut();
        for activity_id in ready {
            // Peek is the existing live-tail presentation for an Active
            // Command; Expanded remains reserved for manual disclosure.
            folds.auto_promote(activity_id);
        }
    }

    /// The composer the keys write into, which is the one belonging to the
    /// Session the main view has open — loaded or not. A draft is the
    /// reader's, not the snapshot's, so it is taken and kept under the target
    /// from the first moment.
    fn composer_key(&self) -> ComposerKey {
        self.route
            .clone()
            .map_or(ComposerKey::Landing, ComposerKey::Session)
    }

    fn reference_in_current_origin(&self, session_id: SessionId) -> Option<SessionReference> {
        self.session_reference
            .as_ref()
            .map(|current| SessionReference::new(current.origin.clone(), session_id))
    }

    /// The parent of the open Session, present exactly while the reader is in
    /// a Subagent's Session. Every property of viewing one keys off this one
    /// reading: Escape returning to the parent, the composer standing down,
    /// and the input mode that keeps Prompt delivery out of reach.
    pub(super) fn open_subagent_parent(&self) -> Option<SessionId> {
        self.session
            .as_ref()
            .and_then(|session| session.snapshot().session.parent)
    }

    /// The child Sessions of the open Session's working Subagents, in the
    /// order they spawned — the entries the Subagent Picker browses.
    fn working_subagent_ids(&self) -> Vec<SessionId> {
        self.session.as_ref().map_or_else(Vec::new, |session| {
            working_subagents(session.snapshot())
                .iter()
                .map(|subagent| subagent.session_id)
                .collect()
        })
    }

    /// Opens the Subagent Picker over the open Session's working Subagents.
    /// With nothing to browse the ask leaves the view put, which is what
    /// keeps the key carrying it inert.
    pub(super) fn open_subagent_picker(&mut self) {
        let working = self.working_subagent_ids();
        self.subagent_picker.open_over(&working);
    }

    fn move_subagent_selection(&mut self, distance: isize) {
        let working = self.working_subagent_ids();
        self.subagent_picker.move_selection(&working, distance);
    }

    /// The Subagent the picker stands on, provided it is still working — the
    /// only kind of entry the picker offers to open.
    fn selected_working_subagent(&self) -> Option<SessionId> {
        let selected = self.subagent_picker.selected()?;
        self.working_subagent_ids()
            .contains(&selected)
            .then_some(selected)
    }

    fn reconcile_subagent_picker(&mut self) {
        let working = self.working_subagent_ids();
        self.subagent_picker.reconcile(&working);
    }

    /// Whether the open Session's Provider offers stopping one working
    /// Subagent on its own — the affordance the Subagent Picker's rows carry.
    /// Read off the Session's own settled Selection, because the Subagents on
    /// offer run under it whatever Selection edit may be pending.
    pub(super) fn subagent_stop_offered(&self) -> bool {
        self.session
            .as_ref()
            .and_then(|session| session.snapshot().session.agent_selection.as_ref())
            .is_some_and(|selection| {
                built_in_providers().iter().any(|provider| {
                    provider.id == selection.provider && provider.supports_subagent_stop
                })
            })
    }

    fn composer_down_is_inert(&self) -> bool {
        self.composers.down_is_inert(self.composer_key())
    }

    pub(super) fn agent_selection(&self) -> Option<&AgentSelection> {
        let Some(projection) = self.session.as_ref() else {
            return self.landing_agent_selection.as_ref();
        };
        let session = self
            .session_reference
            .as_ref()
            .expect("a Session projection carries its reference");
        self.queued_agent_selection
            .as_ref()
            .filter(|(queued_session, _)| queued_session == session)
            .map(|(_, selection)| selection)
            .or_else(|| {
                self.pending_agent_selection
                    .as_ref()
                    .filter(|pending| &pending.session == session)
                    .map(|pending| &pending.selection)
            })
            .or_else(|| {
                self.confirmed_agent_selection
                    .as_ref()
                    .filter(|(confirmed_session, _)| confirmed_session == session)
                    .map(|(_, selection)| selection)
            })
            .or(projection.snapshot().session.agent_selection.as_ref())
    }

    /// Captures presentation before and after every Application event. The
    /// first capture freezes an ID fallback before a catalog response can
    /// populate the picker; the second adopts friendly metadata only when the
    /// event genuinely changed the Agent Selection.
    fn remember_agent_selection_presentation(&mut self) {
        let selection = self.agent_selection().cloned();
        self.model_picker.remember_selection(selection.as_ref());
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
        let destination = ComposerKey::Session(
            self.session_reference
                .clone()
                .expect("a Session snapshot carries its reference"),
        );
        let pending = self
            .pending_submission
            .take()
            .expect("pending submission was just observed");
        if let SubmissionTarget::AdmitPrompt(session, PromptDelivery::Steer) = &pending.target
            && authoritative.status == PromptStatus::Pending
        {
            self.track_pending_steer(session.clone(), pending.prompt.clone());
        }
        self.composers
            .admission_reconciled(pending.source, destination, &pending.prompt);
        self.submission_error = None;
    }

    fn acknowledge_pending_submission(&mut self, prompt_id: PromptId) {
        let detached = self.pending_submission.as_ref().is_some_and(|pending| {
            pending.prompt.id == prompt_id
                && matches!(
                    &pending.target,
                    SubmissionTarget::AdmitPrompt(session, _)
                        if self.session_reference.as_ref() != Some(session)
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
        self.composers.admission_reconciled(
            pending.source.clone(),
            pending.source,
            &pending.prompt,
        );
        self.submission_error = None;
    }

    pub(super) fn session_interaction(
        &self,
        session: &SessionReference,
    ) -> Option<&SessionInteraction> {
        self.session_interactions.get(session)
    }

    /// One set of projection inputs serves both drawing and input-time
    /// validation of content coordinates.
    pub(super) fn transcript_view(
        &self,
        theme: &Theme,
        width: u16,
    ) -> Option<std::cell::Ref<'_, TranscriptView>> {
        let snapshot = self.session.as_ref()?.snapshot();
        let interaction = self.session_interaction(self.session_reference.as_ref()?)?;
        Some(self.transcript_cache.view(
            self.transcript_generation,
            snapshot,
            &self.provisional_prompts(snapshot.session.id),
            TranscriptDisclosure {
                folds: &interaction.folds.borrow(),
                groups: &interaction.groups.borrow(),
                turns: &interaction.turns.borrow(),
                reasoning_visibility: self.settings().transcript.reasoning_visibility,
            },
            theme,
            width,
        ))
    }

    fn navigate_transcript_page(&mut self, direction: TranscriptDirection) {
        self.navigate_transcript(direction, None);
    }

    fn navigate_transcript_lines(&mut self, direction: TranscriptDirection) {
        self.navigate_transcript(direction, Some(WHEEL_SCROLL_ROWS));
    }

    fn navigate_transcript(&mut self, direction: TranscriptDirection, rows: Option<usize>) {
        let Some(session) = self.session_reference.clone() else {
            return;
        };
        let Some(interaction) = self.session_interactions.get_mut(&session) else {
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
    fn ensure_interaction(&mut self, session: SessionReference) -> &mut SessionInteraction {
        let fold_posture = self.settings.transcript.default_fold_posture;
        self.session_interactions
            .entry(session)
            .or_insert_with(|| SessionInteraction::opening_at(fold_posture))
    }

    /// The attached Session's interaction state, created if this is the first
    /// thing to reach for it.
    fn current_interaction(&mut self) -> Option<&SessionInteraction> {
        let session = self.session_reference.clone()?;
        Some(self.ensure_interaction(session))
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
        if let UnitKey::Activity(activity_id) = start.key
            && let Some(id) = self.session.as_ref().and_then(|session| {
                session
                    .snapshot()
                    .activities
                    .iter()
                    .find_map(|activity| match activity {
                        crate::protocol::Activity::Questionnaire {
                            id, questionnaire, ..
                        } if *id == activity_id
                            && self
                                .pending_questionnaires()
                                .any(|q| q.id == questionnaire.id) =>
                        {
                            Some(questionnaire.id)
                        }
                        _ => None,
                    })
            })
        {
            return Some(SemanticCommandId::QuestionnaireOpen.on_questionnaire(id));
        }
        let interaction = self.current_interaction()?;
        match start.key {
            UnitKey::Activity(activity_id) => {
                let mut folds = interaction.folds.borrow_mut();
                match start.fold {
                    FoldDisclosure::Staged(FoldStep::Folded) => {
                        if start.hides_content {
                            folds.set_step(activity_id, FoldStep::Peek);
                        }
                    }
                    FoldDisclosure::Staged(FoldStep::Peek) => {
                        if start.marker_row == Some(row) {
                            folds.set_step(activity_id, FoldStep::Expanded);
                        } else if start.is_header(row) {
                            folds.set_step(activity_id, FoldStep::Folded);
                        }
                    }
                    FoldDisclosure::Staged(FoldStep::Expanded) => {
                        if start.is_header(row) {
                            folds.set_step(activity_id, FoldStep::Folded);
                        }
                    }
                    FoldDisclosure::Binary { folded } => {
                        if start.hides_content {
                            folds.expand(activity_id);
                        } else if start.is_header(row) && !folded {
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
            // The whole row is the way into the child Session it names, so any
            // press on it resolves to the open command rather than to a Fold.
            UnitKey::Subagent(session_id) => {
                return self
                    .reference_in_current_origin(session_id)
                    .map(|session| SemanticCommandId::SubagentOpen.on_session(session));
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
        let Some(session) = self.session_reference.clone() else {
            return;
        };
        let interaction = self.ensure_interaction(session);
        interaction.follow_latest.set(true);
        interaction.anchor.set(None);
    }

    /// Whether the composer has the keys. The Sidebar is the one surface that
    /// takes them without opening over the composer, so a composer that has
    /// them is simply one the Sidebar is not driving.
    pub(super) fn composer_focused(&self) -> bool {
        !self.sidebar.has_focus()
    }

    /// The Session the main view has open, and `None` on the Landing. It is
    /// the route the reader is looking at rather than a question about what
    /// has loaded, which is why the Sidebar's open highlight is derived from
    /// it and from nothing else.
    pub(super) fn open_session(&self) -> Option<SessionId> {
        self.route.as_ref().map(|route| route.session_id)
    }

    pub(super) fn open_session_reference(&self) -> Option<&SessionReference> {
        self.route.as_ref()
    }

    /// Whether activating the open Sidebar row means retry rather than merely
    /// handing the keys back. Only a failed optimistic shell has that meaning:
    /// an attachment still in flight must not be duplicated, and a hydrated
    /// Session is already open.
    fn open_session_can_retry(&self) -> bool {
        self.opening_error.is_some()
    }

    /// Whether the Session the main view has open is still on its way: the
    /// reader has been carried into it and its snapshot has not landed. This
    /// is what every act needing a Session — delivering a Prompt, choosing an
    /// Agent for it — waits on, because there is no Session to act on yet.
    pub(super) fn open_session_is_loading(&self) -> bool {
        self.route.is_some() && self.session.is_none()
    }

    /// Whether the optimistic shell has waited long enough to draw its
    /// loading feedback. Time is read only at presentation boundaries, so the
    /// run loop stays idle until the one-shot threshold wakeup.
    pub(super) fn opening_loading_is_visible(&self) -> bool {
        match self.opening_loading {
            OpeningLoadingState::Inactive => false,
            OpeningLoadingState::WaitingUntil(deadline) => {
                self.presentation_clock.now() >= deadline
            }
            OpeningLoadingState::Visible => true,
        }
    }

    /// Whether the Sidebar is the surface the keys actually reach, which is
    /// what its row focus says: a reader driving the Sidebar sees which row
    /// Enter would act on, and one who has opened something over it does not,
    /// because Enter no longer means that row.
    ///
    /// This is the Sidebar's own claim on the keys narrowed by every surface
    /// that outranks it. The Sidebar's context menu is not one of them: it is
    /// part of the column, opened on a row of it, and the row it stands on
    /// goes on saying so.
    pub(super) fn sidebar_owns_input(&self) -> bool {
        self.sidebar.has_focus() && !self.overlay_owns_input()
    }

    fn active_selection_overlay_area(&self) -> Option<ratatui::layout::Rect> {
        self.selection_overlay_area.get().filter(|_| {
            self.selection_frames.borrow().last().is_some_and(|frame| {
                frame.surface.is_overlay() && self.selection_surface_visible(frame.surface)
            })
        })
    }

    /// The top painted overlay owns selection and pointer/key dispatch alike.
    fn top_selection_overlay(&self) -> Option<SelectionSurface> {
        if self.connect_overlay.is_open() {
            Some(SelectionSurface::Connect)
        } else if self.serve_overlay.is_open() {
            Some(SelectionSurface::Serve)
        } else if self.theme_picker.is_open() {
            Some(SelectionSurface::Themes)
        } else if self.model_picker.is_open() {
            Some(SelectionSurface::Models)
        } else if self.model_options.is_open() {
            Some(SelectionSurface::ModelOptions)
        } else if self.settings_panel.numeric_editor_is_open() {
            Some(SelectionSurface::NumericEditor)
        } else if self.settings_panel.is_open() {
            Some(SelectionSurface::Settings)
        } else if self.workspace_picker.is_open() {
            Some(SelectionSurface::Workspaces)
        } else if self.session_picker.is_open() {
            Some(SelectionSurface::Sessions)
        } else if self.sidebar.menu_is_open() {
            Some(SelectionSurface::SidebarMenu)
        } else if self.subagent_picker.is_open() {
            Some(SelectionSurface::Subagents)
        } else if self.composer_completion.is_visible() {
            Some(SelectionSurface::Completions)
        } else {
            None
        }
    }

    fn selection_surface_visible(&self, surface: SelectionSurface) -> bool {
        if self.reconnect_overlay_visible {
            return false;
        }
        if let Some(overlay) = self.top_selection_overlay() {
            return surface == overlay;
        }
        !surface.is_overlay() && !self.overlay_owns_input()
    }

    /// Whether a surface opened over the main view holds the keys. These are
    /// the surfaces [`Application::command_for_terminal_input`] asks before it
    /// asks the Sidebar, so the two readings must agree — which is why that
    /// routing asks through [`Self::sidebar_owns_input`] rather than repeating
    /// the question.
    ///
    /// The Sidebar's own context menu is asked before the Sidebar too, and is
    /// deliberately not here: it is part of the column rather than something
    /// opened over it, and the row it stands on goes on saying which row it
    /// acts upon.
    pub(super) fn overlay_owns_input(&self) -> bool {
        self.theme_picker.is_open()
            || self.model_picker.is_open()
            || self.connect_overlay.is_open()
            || self.serve_overlay.is_open()
            || self.settings_panel.is_open()
            || self.model_options.is_open()
            || self.session_picker.is_open()
            || self.workspace_picker.is_open()
            || self.subagent_picker.is_open()
            || self.questionnaires.is_open(self.session_reference.as_ref())
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
            .admission_failed(pending.source.clone(), &pending.prompt);
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
        let destination = ComposerKey::Session(
            self.session_reference
                .clone()
                .expect("a Session snapshot carries its reference"),
        );
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
            if let SubmissionTarget::AdmitPrompt(session, PromptDelivery::Steer) = &failed.target
                && authoritative.status == PromptStatus::Pending
            {
                self.track_pending_steer(session.clone(), failed.prompt.clone());
            }
            if self.composers.late_admission_reconciled(
                failed.source,
                destination.clone(),
                &failed.prompt,
            ) {
                self.submission_error = None;
            }
        }
    }

    pub(super) fn provisional_prompts(&self, session_id: SessionId) -> Vec<&InitialPrompt> {
        let session = SessionReference::new(self.outlook.clone(), session_id);
        let mut prompts = self
            .pending_steers
            .iter()
            .filter(|steer| steer.session == session)
            .map(|steer| &steer.prompt)
            .collect::<Vec<_>>();
        if let Some(pending) = self.pending_submission.as_ref().filter(|pending| {
            pending.target == SubmissionTarget::AdmitPrompt(session.clone(), PromptDelivery::Steer)
        }) {
            prompts.push(&pending.prompt);
        }
        prompts
    }

    fn track_pending_steer(&mut self, session: SessionReference, prompt: InitialPrompt) {
        if !self
            .pending_steers
            .iter()
            .any(|pending| pending.prompt.id == prompt.id)
        {
            self.pending_steers.push(PendingSteer { session, prompt });
        }
    }

    fn reconcile_pending_steers(&mut self) {
        let Some(snapshot) = self.session.as_ref().map(SessionProjection::snapshot) else {
            return;
        };
        let session = self
            .session_reference
            .as_ref()
            .expect("a Session snapshot carries its reference");
        self.pending_steers.retain(|pending| {
            &pending.session != session
                || snapshot.prompts.iter().any(|prompt| {
                    prompt.id == pending.prompt.id && prompt.status == PromptStatus::Pending
                })
        });
    }

    pub(super) fn queued_prompts(&self, session_id: SessionId) -> Vec<QueuedPrompt<'_>> {
        let session_reference = SessionReference::new(self.outlook.clone(), session_id);
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
            pending.target
                == SubmissionTarget::AdmitPrompt(session_reference.clone(), PromptDelivery::Queue)
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
            CommandMode::InterruptConfirmation { turn_id, armed_at } => {
                // The confirmation stands only while what it would stop is
                // still running: the Turn it named, or — for the interrupt
                // owed to Subagents alone — any Subagent still working.
                let still_running = match turn_id {
                    Some(turn_id) => self.active_turn_id() == Some(turn_id),
                    None => !self.working_subagent_ids().is_empty(),
                };
                let expired = self
                    .presentation_clock
                    .now()
                    .saturating_duration_since(armed_at)
                    >= INTERRUPT_CONFIRMATION_TIMEOUT;
                if !still_running || expired {
                    self.command_mode = CommandMode::Composer;
                }
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

/// The last left press, kept beside the press history so that nothing that
/// clears or copies a Text Selection can reset the click count: only time,
/// distance, and surface do.
#[derive(Clone, Copy, Debug)]
struct LastClick {
    count: u8,
    surface: Option<SelectionSurface>,
    position: Position,
    at: Instant,
}

impl LastClick {
    fn continues(
        &self,
        surface: Option<SelectionSurface>,
        position: Position,
        at: Instant,
        interval: Duration,
    ) -> bool {
        self.surface == surface
            && at.saturating_duration_since(self.at) <= interval
            && self.position.x.abs_diff(position.x) <= CLICK_SLOP
            && self.position.y.abs_diff(position.y) <= CLICK_SLOP
    }
}

#[derive(Clone, Debug)]
struct LeftPress {
    position: Position,
    pointer: Position,
    dragged: bool,
    outside_overlay: bool,
    selection_anchor: Option<(SelectionCell, u64, SelectionSurface)>,
}

pub struct Application {
    pub(super) state: TuiState,
    pub(super) slots: RenderSlots,
    pub(super) terminal_facts: TerminalFacts,
    pub(super) theme: Theme,
    pub(super) config_root: Option<PathBuf>,
    pub(super) theme_catalog: ThemeCatalog,
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
        Self::from_state(TuiState::default(), TerminalFacts::default())
    }
}

#[derive(Debug)]
pub enum ApplicationEvent {
    Command(CommandId),
    /// The run loop's presentation-only wakeup. Kept as an event so headless
    /// rendering tests can drive latency behavior through the same boundary.
    SpinnerTick,
    /// The optimistic shell's quiet-period wakeup. Kept explicit so another
    /// ready event cannot consume the deadline without dirtying a frame.
    OpeningLoadingDelayElapsed,
    ReconnectGraceElapsed,
    Managed(ManagedEvent),
    OriginCatalog {
        outlook: Outlook,
        event: ManagedEvent,
    },
    Session(SessionEvent),
    SessionSubscriptionEnded,
    PromptAdmissionSucceeded {
        session: SessionReference,
        prompt_id: PromptId,
    },
    PromptAdmissionFailed {
        session: SessionReference,
        prompt_id: PromptId,
        error: String,
    },
    SessionCreationFailed {
        prompt_id: PromptId,
        error: String,
    },
    SessionAttached(SessionSnapshot),
    OriginSessionAttached {
        reference: SessionReference,
        snapshot: SessionSnapshot,
    },
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
    OriginSessionAttachmentFailed {
        reference: SessionReference,
        error: String,
    },
    SessionDeletionFailed {
        reference: SessionReference,
        error: String,
    },
    SessionCreated(SessionSnapshot),
    SessionOperationFailed(String),
    QuestionnaireSubmissionReconciled {
        id: crate::protocol::QuestionnaireId,
        session: SessionReference,
        snapshot: Option<SessionSnapshot>,
        error: Option<String>,
    },
    /// The effective settings an accepted edit left in force.
    SettingMutated(SettingsSnapshot),
    SettingMutationFailed(String),
    /// Serving is enabled (where needed) and the machine's current candidate
    /// addresses are ready for the reader to choose among.
    ServingPrepared {
        settings: Option<SettingsSnapshot>,
        candidates: Vec<std::net::SocketAddr>,
    },
    ServingPreparationFailed(String),
    InviteIssued {
        invite: crate::protocol::IssuedInvite,
        peers: Vec<crate::protocol::Peer>,
    },
    ServingOperationFailed(String),
    PeerRemoved(String),
    RemotesListed(Vec<crate::protocol::Remote>),
    RemoteListingFailed(String),
    /// The paired-Remote list requested specifically for an Everywhere
    /// Session surface. It is distinct from the Connect overlay's listing,
    /// which also probes the Remotes it presents.
    EverywhereRemotesListed {
        request: EverywhereListRequest,
        remotes: Vec<crate::protocol::Remote>,
    },
    EverywhereRemoteListingFailed {
        request: EverywhereListRequest,
        error: String,
    },
    InvitePreviewed {
        invite: String,
        preview: crate::protocol::InvitePreview,
    },
    InvitePreviewFailed {
        invite: String,
        error: String,
    },
    RemoteRedeemed(crate::protocol::Remote),
    InviteRedemptionFailed(String),
    RemoteProbed {
        name: String,
        result: Result<crate::protocol::RemoteHealth, String>,
    },
    WorkspaceResolved {
        outlook: Outlook,
        surface: WorkspaceResolutionSurface,
        request_id: u64,
        result: std::result::Result<Workspace, String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandId {
    QuestionnaireInsert(String),
    QuestionnaireDelete,
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
    /// Begins a left-button gesture without acting on the surface beneath it.
    PressAt {
        position: Position,
    },
    ReleaseAt {
        position: Position,
    },
    DragAt {
        position: Position,
    },
    /// A click resolved through the active input mode.
    ClickAt {
        position: Position,
    },
    /// Where the reader asked for a context menu. Only the Sidebar's own rows
    /// offer one; everywhere else the ask puts away whatever menu was up.
    OpenContextMenuAt {
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
    /// What the reader typed into the line the Sidebar has them typing into:
    /// its search box, or the path entry standing open over it.
    InsertSidebarText(String),
    DeleteSidebarTextBackward,
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
    InsertThemeSearch(String),
    DeleteThemeSearchBackward,
    SelectPreviousTheme,
    SelectNextTheme,
    PagePreviousThemes,
    PageNextThemes,
    SelectTheme,
    CloseThemePicker,
    InsertWorkspaceSearch(String),
    DeleteWorkspaceSearchBackward,
    SelectPreviousWorkspace,
    SelectNextWorkspace,
    PagePreviousWorkspaces,
    PageNextWorkspaces,
    SelectWorkspace,
    CloseWorkspacePicker,
    SelectPreviousSubagent,
    SelectNextSubagent,
    OpenSelectedSubagent,
    StopSelectedSubagent,
    CloseSubagentPicker,
    /// Where in the settings panel the reader pointed, which the panel resolves
    /// against the geometry the frame in force drew.
    FocusSettingsPanelAt {
        column: u16,
        screen_row: u16,
    },
    InvokeSemantic(SemanticCommandId),
    InsertText(String),
    PasteText(String),
    InsertConnectText(String),
    DeleteConnectTextBackward,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApplicationTransition {
    SubmitQuestionnaire {
        session: SessionReference,
        id: crate::protocol::QuestionnaireId,
        submission: crate::protocol::QuestionnaireSubmission,
    },
    Continue,
    Exit,
    SessionEnded,
    DetachSession,
    DeleteSession(SessionReference),
    /// A Session set aside as done for now, or brought back off the shelf.
    /// Which of the two is stated rather than toggled, so a client acting on a
    /// listing that has moved on cannot flip a Session it meant to leave
    /// alone.
    SettleSession {
        session: SessionReference,
        settled: bool,
    },
    CreateSession(CreateSessionRequest),
    AdmitPrompt {
        session: SessionReference,
        request: AdmitPromptRequest,
    },
    PromotePrompt {
        session: SessionReference,
        prompt_id: PromptId,
    },
    CancelPrompt {
        session: SessionReference,
        prompt_id: PromptId,
    },
    /// Stop what a Session is doing — its active Turn and the Subagents it
    /// spawned, or the Subagents alone once the Turn has settled. Naming a
    /// Subagent's own Session stops that one Subagent.
    InterruptSession {
        session: SessionReference,
    },
    SubscribeSession(SessionReference),
    /// Report a root Session as Viewed without changing the main-view route.
    /// Produced when its Turn Settles while it is already open.
    ViewSession(SessionReference),
    /// Report a listed root Session as Viewed while beginning its attachment.
    /// Keeping this distinct from `AttachSession` makes Subagent navigation
    /// incapable of accidentally reporting a child as Viewed.
    ViewAndAttachSession(SessionReference),
    AttachSession(SessionReference),
    ListSessions(SessionListRequest),
    /// Reconcile the Remote catalog streams owned by the run loop and dispatch
    /// any initial listings whose Origins established that desired set.
    ReconcileCatalogOrigins {
        catalog_origins: HashSet<Outlook>,
        requests: Vec<SessionListRequest>,
    },
    /// Restart one recovering Remote catalog stream immediately and re-ask
    /// the listing whose cached rows remain on show.
    RetryCatalogOrigin(SessionListRequest),
    ListModels(ModelListRequest),
    RefreshSkills(SkillCatalogRequest),
    ConfirmLandingAgentSelection(AgentSelection),
    UpdateAgentSelection {
        session: SessionReference,
        request: UpdateAgentSelectionRequest,
    },
    /// One Setting's typed edit, on its way to the server that owns the file.
    MutateSetting(SettingMutation),
    /// Prepare the `/serve` surface, enabling the durable Setting first when
    /// it was off before discovering candidate addresses.
    BeginServing {
        enable: bool,
        port: u16,
    },
    /// Issue a fresh Invite containing exactly the addresses the reader chose.
    IssueInvite(crate::protocol::IssueInviteRequest),
    CopyToClipboard(super::ClipboardContent),
    RemovePeer(String),
    BeginConnecting,
    /// Ask the local Server for paired Remotes without opening or refreshing
    /// the Connect overlay.
    ListEverywhereRemotes(EverywhereListRequest),
    PreviewInvite(String),
    RedeemInvite(crate::protocol::RedeemInviteRequest),
    /// Turn to one Server and name the Remote catalog streams the run loop
    /// should keep open after the turn. Keeping the desired set in the
    /// transition lets headless clients observe the same ownership decision
    /// without establishing a network connection.
    TurnOutlook {
        outlook: Outlook,
        catalog_origins: HashSet<Outlook>,
    },
    /// Turn to a foreign listed Session's Origin, report it Viewed, and attach
    /// it, while retaining the independent scopes of the listing surfaces.
    TurnOutlookAndViewAndAttach {
        session: SessionReference,
        catalog_origins: HashSet<Outlook>,
    },
    CancelWorkspaceResolution(WorkspaceResolutionSurface),
    ResolveWorkspace {
        outlook: Outlook,
        surface: WorkspaceResolutionSurface,
        request_id: u64,
        request: ResolveWorkspaceRequest,
    },
}

impl Application {
    pub fn new(workspace: impl AsRef<Path>, terminal_facts: TerminalFacts) -> Self {
        Self::from_state(TuiState::new(workspace), terminal_facts)
    }

    fn from_state(state: TuiState, terminal_facts: TerminalFacts) -> Self {
        let mut application = Self {
            state,
            slots: RenderSlots::builtins(),
            terminal_facts,
            theme: Theme::system(),
            config_root: None,
            theme_catalog: ThemeCatalog::default(),
        };
        application.resolve_theme();
        application
    }

    /// Points this Client at its machine's Config root and reads user Themes
    /// before its first frame. The same root remains in force across Outlook
    /// changes because appearance belongs to the Client, not the Server being
    /// viewed.
    pub fn with_config_root(mut self, config_root: impl AsRef<Path>) -> Self {
        self.config_root = Some(config_root.as_ref().to_path_buf());
        self.refresh_user_themes();
        self.resolve_theme();
        self
    }

    fn refresh_user_themes(&mut self) {
        let (catalog, diagnostics) = ThemeCatalog::scan(self.config_root.as_deref());
        for diagnostic in &diagnostics {
            tracing::warn!(
                path = %diagnostic.file.display(),
                reason = %diagnostic.message,
                "ignored user Theme"
            );
        }
        self.theme_catalog = catalog;
        self.state
            .application_notice
            .receive_theme_diagnostics(diagnostics);
    }

    /// Re-resolves presentation from a fresh reading of the attached
    /// terminal. Startup supplies the first reading; live re-detection may
    /// replace it through the same boundary later.
    pub fn set_terminal_facts(&mut self, terminal_facts: TerminalFacts) {
        if self.terminal_facts == terminal_facts {
            return;
        }
        self.terminal_facts = terminal_facts;
        self.resolve_theme();
    }

    fn resolve_theme(&mut self) {
        let name = self
            .state
            .theme_picker
            .preview()
            .unwrap_or(&self.state.settings.appearance.theme)
            .to_owned();
        self.resolve_theme_name(&name);
    }

    fn resolve_theme_name(&mut self, name: &str) {
        match self.theme_catalog.resolve(
            name,
            self.state.settings.appearance.mode,
            &self.terminal_facts,
        ) {
            Ok(theme) => self.theme = theme,
            Err(_) => {
                self.theme = Theme::system();
                self.state.application_notice.receive_theme_fallback(name);
            }
        }
    }

    pub(super) fn first_frame_ready(&self) -> bool {
        self.state.settings_received
    }

    pub(super) fn pending_workspace_resolution(
        &self,
        surface: WorkspaceResolutionSurface,
    ) -> Option<u64> {
        self.state
            .pending_workspace_resolutions
            .get(&surface)
            .copied()
    }

    /// Injects the clock used by presentation latency. Production uses the
    /// monotonic system clock; tests can advance a deterministic clock without
    /// waiting out the configured delay.
    pub fn with_presentation_clock(
        mut self,
        clock: impl Fn() -> Instant + Send + Sync + 'static,
    ) -> Self {
        self.state.presentation_clock = PresentationClock(Arc::new(clock));
        self
    }

    /// Injects how long after one left press a second one continues the click
    /// count. Production uses the fixed interval; tests shorten it or advance
    /// the presentation clock instead of waiting.
    pub fn with_click_interval(mut self, interval: Duration) -> Self {
        self.state.click_interval = interval;
        self
    }

    /// The one-shot delay remaining before an optimistic Session shell should
    /// reveal `Loading`. A reached but unobserved threshold returns zero; `None`
    /// means the run loop has observed it or the opening was cancelled.
    pub fn opening_loading_wakeup(&self) -> Option<Duration> {
        self.opening_loading_deadline()
            .map(|deadline| deadline.saturating_duration_since(self.state.presentation_clock.now()))
    }

    pub(super) fn opening_loading_deadline(&self) -> Option<Instant> {
        match self.state.opening_loading {
            OpeningLoadingState::WaitingUntil(deadline) => Some(deadline),
            OpeningLoadingState::Inactive | OpeningLoadingState::Visible => None,
        }
    }

    pub fn handle_event(&mut self, event: ApplicationEvent) -> Result<ApplicationTransition> {
        let carries_settings = matches!(
            &event,
            ApplicationEvent::Managed(ManagedEvent::SettingsSnapshot(_))
                | ApplicationEvent::SettingMutated(_)
                | ApplicationEvent::ServingPrepared {
                    settings: Some(_),
                    ..
                }
        );
        let mutation_failed = matches!(&event, ApplicationEvent::SettingMutationFailed(_));
        let theme_name = self.state.settings.appearance.theme.clone();
        self.state.remember_agent_selection_presentation();
        let transition = self.handle_event_inner(event)?;
        if let (Some(session), Some(projection)) =
            (&self.state.session_reference, &self.state.session)
        {
            self.state
                .questionnaires
                .reconcile(session, projection.snapshot());
        }
        if carries_settings || mutation_failed {
            self.state.theme_picker.settle_preview();
        }
        if carries_settings || mutation_failed || self.state.settings.appearance.theme != theme_name
        {
            self.resolve_theme();
        }
        self.state.remember_agent_selection_presentation();
        Ok(transition)
    }

    fn handle_event_inner(&mut self, event: ApplicationEvent) -> Result<ApplicationTransition> {
        match event {
            ApplicationEvent::Command(command) => self.handle_command(command),
            ApplicationEvent::SpinnerTick => {
                self.advance_spinner();
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::OpeningLoadingDelayElapsed => {
                if self.state.opening_loading_is_visible() {
                    self.state.opening_loading = OpeningLoadingState::Visible;
                }
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::ReconnectGraceElapsed => Ok(self.elapse_reconnect_grace()),
            ApplicationEvent::Managed(event) => self.handle_managed_event(event),
            ApplicationEvent::OriginCatalog { outlook, event } => {
                self.handle_origin_catalog(outlook, event)
            }
            ApplicationEvent::Session(event) => {
                if let SessionEvent::Snapshot(snapshot) = &event {
                    let reference =
                        SessionReference::new(self.state.outlook.clone(), snapshot.session.id);
                    for questionnaire in super::questionnaire::pending(snapshot) {
                        self.state.questionnaires.reconcile_submission(
                            &reference,
                            questionnaire.id,
                            snapshot,
                        );
                    }
                }
                let viewed = self.state.open_root_viewed_by(&event);
                self.state.apply_session(event)?;
                Ok(viewed.map_or(
                    ApplicationTransition::Continue,
                    ApplicationTransition::ViewSession,
                ))
            }
            ApplicationEvent::SessionSubscriptionEnded => Ok(self
                .session_reference()
                .map_or(ApplicationTransition::Continue, |session_id| {
                    ApplicationTransition::SubscribeSession(session_id)
                })),
            ApplicationEvent::SessionCreated(snapshot) => {
                self.state.apply_created_session(snapshot)?;
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::SessionAttached(snapshot) => {
                let reference =
                    SessionReference::new(self.state.outlook.clone(), snapshot.session.id);
                self.attach_session(reference, snapshot)
            }
            ApplicationEvent::OriginSessionAttached {
                reference,
                snapshot,
            } => {
                if reference.origin != self.state.outlook
                    || reference.session_id != snapshot.session.id
                {
                    return Ok(ApplicationTransition::Continue);
                }
                self.attach_session(reference, snapshot)
            }
            ApplicationEvent::SessionAttachmentFailed(error) => {
                if self.state.sidebar.is_attaching()
                    || (self.state.route.is_some() && self.state.session.is_none())
                {
                    // The target remains the route and the refusal belongs to
                    // its main content, not to the listing that led there.
                    Ok(self.fail_open_session_attachment(error))
                } else {
                    Ok(Self::session_picker_listing_transition(
                        self.state.session_picker.fail_attachment(error),
                    ))
                }
            }
            ApplicationEvent::OriginSessionAttachmentFailed { reference, error } => {
                if reference.origin != self.state.outlook {
                    return Ok(ApplicationTransition::Continue);
                }
                if self.state.route.as_ref() == Some(&reference) && self.state.session.is_none() {
                    Ok(self.fail_open_session_attachment(error))
                } else {
                    Ok(Self::session_picker_listing_transition(
                        self.state.session_picker.fail_attachment(error),
                    ))
                }
            }
            ApplicationEvent::SessionDeletionFailed { reference, error } => {
                // Whichever surface asked is the one that answers, so the
                // refusal is drawn where the reader was looking.
                if !self.state.sidebar.fail_deletion(&reference, error.clone()) {
                    self.state.session_picker.fail_deletion(&reference, error);
                }
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::QuestionnaireSubmissionReconciled {
                session,
                id,
                snapshot,
                error,
            } => {
                if let Some(snapshot) = snapshot {
                    if self.state.session_reference.as_ref() == Some(&session)
                        && self.state.session.as_ref().is_some_and(|current| {
                            current.snapshot().revision.0 > snapshot.revision.0
                        })
                    {
                        return Ok(ApplicationTransition::Continue);
                    }
                    self.state
                        .questionnaires
                        .reconcile_submission(&session, id, &snapshot);
                    if self.state.session_reference.as_ref() == Some(&session)
                        && self.state.session.as_ref().is_none_or(|current| {
                            current.snapshot().revision.0 <= snapshot.revision.0
                        })
                    {
                        self.state
                            .apply_session(SessionEvent::Snapshot(Box::new(snapshot)))?;
                    }
                }
                if self.state.session_reference.as_ref() == Some(&session) {
                    let confirmed = self.state.session.as_ref().is_some_and(|current| current.snapshot().activities.iter().any(|activity| matches!(activity,
                        Activity::Questionnaire { questionnaire, outcome: crate::protocol::QuestionnaireOutcome::Answered | crate::protocol::QuestionnaireOutcome::Declined, .. } if questionnaire.id == id)));
                    self.state.submission_error = if confirmed { None } else { error };
                }
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
            ApplicationEvent::ServingPrepared {
                settings,
                candidates,
            } => {
                if let Some(settings) = settings {
                    self.state.adopt_settings(settings);
                }
                self.state.serve_overlay.load_candidates(candidates);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::ServingPreparationFailed(error) => {
                self.state.serve_overlay.fail_preparation(error);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::InviteIssued { invite, peers } => {
                self.state.serve_overlay.show_invite(invite, peers);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::ServingOperationFailed(error) => {
                self.state.serve_overlay.fail_operation(error);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::PeerRemoved(peer_id) => {
                self.state.serve_overlay.peer_removed(&peer_id);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::RemotesListed(remotes) => {
                self.state
                    .connect_overlay
                    .load_remotes(remotes, &self.state.outlook);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::RemoteListingFailed(error) => {
                self.state.connect_overlay.fail_invite_entry(error);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::EverywhereRemotesListed { request, remotes } => {
                let requests = match request.surface() {
                    SessionListSurface::SessionPicker => self
                        .state
                        .session_picker
                        .load_everywhere_remotes(request, remotes),
                    SessionListSurface::Sidebar => {
                        self.state.sidebar.load_everywhere_remotes(request, remotes)
                    }
                    SessionListSurface::WorkspacePicker => None,
                };
                Ok(
                    requests.map_or(ApplicationTransition::Continue, |requests| {
                        ApplicationTransition::ReconcileCatalogOrigins {
                            catalog_origins: self.state.catalog_origins(),
                            requests,
                        }
                    }),
                )
            }
            ApplicationEvent::EverywhereRemoteListingFailed { request, error } => {
                match request.surface() {
                    SessionListSurface::SessionPicker => {
                        self.state
                            .session_picker
                            .fail_everywhere_remotes(request, error);
                    }
                    SessionListSurface::Sidebar => {
                        self.state.sidebar.fail_everywhere_remotes(request, error);
                    }
                    SessionListSurface::WorkspacePicker => {}
                }
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::InvitePreviewed { invite, preview } => {
                self.state.connect_overlay.show_preview(invite, preview);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::InvitePreviewFailed { invite, error } => {
                self.state.connect_overlay.fail_preview(invite, error);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::RemoteRedeemed(remote) => {
                self.state.connect_overlay.remote_redeemed(remote);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::InviteRedemptionFailed(error) => {
                self.state.connect_overlay.redemption_failed(error);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::RemoteProbed { name, result } => {
                self.state.connect_overlay.remote_probed(&name, result);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::WorkspaceResolved {
                outlook,
                surface,
                request_id,
                result,
            } => {
                if outlook != self.state.outlook
                    || !self.state.accept_workspace_resolution(surface, request_id)
                {
                    return Ok(ApplicationTransition::Continue);
                }
                match result {
                    Ok(workspace) => {
                        self.state.adopt_workspace(workspace.path.clone());
                        match surface {
                            WorkspaceResolutionSurface::Outlook => {
                                self.state.sidebar.refresh_after_outlook_workspace();
                                Ok(self.take_session_listing_transition())
                            }
                            WorkspaceResolutionSurface::WorkspacePicker => {
                                self.state.workspace_picker.close();
                                Ok(self.open_landing())
                            }
                            WorkspaceResolutionSurface::Sidebar => {
                                match self.state.sidebar.accept_workspace(workspace.path) {
                                    SidebarActivation::CatalogOriginsChanged => {
                                        Ok(ApplicationTransition::ReconcileCatalogOrigins {
                                            catalog_origins: self.state.catalog_origins(),
                                            requests: Vec::new(),
                                        })
                                    }
                                    SidebarActivation::Answered => {
                                        Ok(ApplicationTransition::Continue)
                                    }
                                    activation => unreachable!(
                                        "accepting a resolved Sidebar Workspace cannot yield {activation:?}"
                                    ),
                                }
                            }
                        }
                    }
                    Err(error) => {
                        match surface {
                            WorkspaceResolutionSurface::Outlook => {
                                self.state.submission_error = Some(error);
                            }
                            WorkspaceResolutionSurface::WorkspacePicker => {
                                self.state.workspace_picker.fail_resolution(error);
                            }
                            WorkspaceResolutionSurface::Sidebar => {
                                self.state.sidebar.fail_workspace_resolution(error);
                            }
                        }
                        Ok(ApplicationTransition::Continue)
                    }
                }
            }
            ApplicationEvent::PromptAdmissionSucceeded { session, prompt_id } => {
                if self
                    .state
                    .pending_submission
                    .as_ref()
                    .is_some_and(|pending| {
                        pending.prompt.id == prompt_id
                            && matches!(
                                &pending.target,
                                SubmissionTarget::AdmitPrompt(target, _) if target == &session
                            )
                    })
                {
                    self.state.acknowledge_pending_submission(prompt_id);
                }
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::PromptAdmissionFailed {
                session,
                prompt_id,
                error,
            } => {
                if self
                    .state
                    .pending_submission
                    .as_ref()
                    .is_some_and(|pending| {
                        pending.prompt.id == prompt_id
                            && matches!(
                                &pending.target,
                                SubmissionTarget::AdmitPrompt(target, _) if target == &session
                            )
                    })
                {
                    self.state.fail_pending_submission(prompt_id, error);
                }
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::SessionCreationFailed { prompt_id, error } => {
                if self
                    .state
                    .pending_submission
                    .as_ref()
                    .is_some_and(|pending| {
                        pending.prompt.id == prompt_id
                            && pending.target == SubmissionTarget::CreateSession
                    })
                {
                    self.state.fail_pending_submission(prompt_id, error);
                }
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::SessionsListed { request, sessions } => {
                match request.surface() {
                    SessionListSurface::SessionPicker => {
                        let current = self.state.route.clone();
                        self.state
                            .session_picker
                            .load(&request, sessions, current.as_ref());
                    }
                    SessionListSurface::WorkspacePicker => {
                        self.state.workspace_picker.load(&request, sessions);
                    }
                    SessionListSurface::Sidebar => {
                        let open = self.state.route.clone();
                        self.state.sidebar.load(&request, sessions, open.as_ref());
                    }
                }
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::SessionListingFailed { request, error } => {
                match request.surface() {
                    SessionListSurface::SessionPicker => {
                        self.state.session_picker.fail_listing(&request, error);
                    }
                    SessionListSurface::WorkspacePicker => {
                        self.state.workspace_picker.fail_listing(&request, error);
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
        self.state.application_notice.dismiss();
        if self.state.reconnect_overlay_visible || self.defers_for_agent_selection(&command) {
            return Ok(ApplicationTransition::Continue);
        }
        match command {
            CommandId::ClearOrExit | CommandId::OpenContextMenuAt { .. }
                if self.state.text_selection.get().is_some() =>
            {
                let transition = self.invoke_semantic(SemanticCommandId::TextSelectionCopy)?;
                self.invoke_semantic(SemanticCommandId::TextSelectionClear)?;
                Ok(transition)
            }
            CommandId::QuestionnaireInsert(text) => {
                if let Some(questionnaire) = self.state.open_questionnaire().cloned() {
                    self.state.questionnaires.insert(&questionnaire, &text);
                }
                Ok(ApplicationTransition::Continue)
            }
            CommandId::QuestionnaireDelete => {
                self.state.questionnaires.delete();
                Ok(ApplicationTransition::Continue)
            }
            CommandId::SubmitSteer => Ok(self.submit_prompt(PromptDelivery::Steer)),
            CommandId::SubmitQueue => Ok(self.submit_prompt(PromptDelivery::Queue)),
            CommandId::InvokeSemantic(command) => self.invoke_semantic(command),
            CommandId::InsertConnectText(text) => {
                self.state.connect_overlay.insert(&text);
                Ok(ApplicationTransition::Continue)
            }
            CommandId::DeleteConnectTextBackward => {
                self.state.connect_overlay.delete_backward();
                Ok(ApplicationTransition::Continue)
            }
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
            | CommandId::FollowLatest) => self.handle_transcript_command(command),
            CommandId::PressAt { position } => {
                self.invoke_semantic(SemanticCommandId::TextSelectionClear)?;
                let selection_anchor = self
                    .state
                    .selection_frames
                    .borrow()
                    .iter()
                    .rev()
                    .find(|frame| {
                        frame.area.contains(position)
                            && self.state.selection_surface_visible(frame.surface)
                    })
                    .and_then(|frame| {
                        frame
                            .cell(position)
                            .map(|cell| (cell, frame.epoch(), frame.surface))
                    })
                    .or_else(|| {
                        if !self.state.overlay_owns_input()
                            && !self.state.reconnect_overlay_visible
                            && self.state.active_selection_overlay_area().is_none()
                        {
                            self.state
                                .session_reference
                                .as_ref()
                                .and_then(|session| self.state.session_interaction(session))
                                .and_then(|interaction| {
                                    interaction.viewport.borrow().as_ref().and_then(|viewport| {
                                        let row = viewport.transcript_row(position)?;
                                        (row < self.state.transcript_cache.row_count()).then_some((
                                            SelectionCell {
                                                row,
                                                column: usize::from(
                                                    position.x - viewport.content_left,
                                                ),
                                            },
                                            self.state.transcript_cache.selection_epoch(),
                                            SelectionSurface::Transcript,
                                        ))
                                    })
                                })
                        } else {
                            None
                        }
                    });

                self.state.left_press = Some(LeftPress {
                    position,
                    pointer: position,
                    dragged: false,
                    outside_overlay: self
                        .state
                        .active_selection_overlay_area()
                        .is_some_and(|area| !area.contains(position)),
                    selection_anchor,
                });
                let surface = selection_anchor.map(|(_, _, surface)| surface);
                let now = self.state.presentation_clock.now();
                let count = match self.state.last_click {
                    Some(last)
                        if last.continues(surface, position, now, self.state.click_interval) =>
                    {
                        last.count.saturating_add(1).min(CLICK_COUNT_LIMIT)
                    }
                    _ => 1,
                };
                self.state.last_click = Some(LastClick {
                    count,
                    surface,
                    position,
                    at: now,
                });
                if count >= 2 && surface == Some(SelectionSurface::Transcript) {
                    self.invoke_semantic(SemanticInvocation {
                        id: SemanticCommandId::TextSelectionWord,
                        subject: SemanticSubject::ScreenPosition(position),
                    })?;
                }
                Ok(ApplicationTransition::Continue)
            }
            CommandId::DragAt { position } => self.invoke_semantic(SemanticInvocation {
                id: SemanticCommandId::PointerDrag,
                subject: SemanticSubject::ScreenPosition(position),
            }),
            CommandId::ReleaseAt { position } => {
                if self
                    .state
                    .left_press
                    .as_ref()
                    .is_some_and(|press| press.outside_overlay)
                {
                    let press = self.state.left_press.take().expect("outside press exists");
                    return self.invoke_semantic(SemanticInvocation {
                        id: SemanticCommandId::PointerClick,
                        subject: SemanticSubject::ScreenPosition(press.position),
                    });
                }
                if self
                    .state
                    .left_press
                    .as_ref()
                    .is_some_and(|press| press.dragged)
                {
                    self.refresh_text_selection();
                    self.update_text_selection_drag(position, 0);
                    self.state.left_press = None;
                    return if self.state.settings.text_selection.copy == TextSelectionCopy::Release
                    {
                        self.invoke_semantic(SemanticCommandId::TextSelectionCopy)
                    } else {
                        Ok(ApplicationTransition::Continue)
                    };
                }

                if let Some(press) = self.state.left_press.take()
                    && !press.dragged
                    && press.position == position
                {
                    let click = self.invoke_semantic(SemanticInvocation {
                        id: SemanticCommandId::PointerClick,
                        subject: SemanticSubject::ScreenPosition(position),
                    })?;
                    // A press that marked a word made a selection the way a
                    // drag does, so its release copies the way a drag's does.
                    if self.state.settings.text_selection.copy == TextSelectionCopy::Release
                        && self.state.text_selection.get().is_some_and(|selection| {
                            selection.granularity != SelectionGranularity::Cell
                        })
                    {
                        let copy = self.invoke_semantic(SemanticCommandId::TextSelectionCopy)?;
                        if matches!(copy, ApplicationTransition::CopyToClipboard(_)) {
                            return Ok(copy);
                        }
                    }
                    return Ok(click);
                }
                Ok(ApplicationTransition::Continue)
            }
            CommandId::ClickAt { position } => self.handle_click(position),
            CommandId::OpenContextMenuAt { position } => {
                self.state.sidebar.open_menu_at(position);
                Ok(ApplicationTransition::Continue)
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
            command @ (CommandId::InsertSidebarText(_) | CommandId::DeleteSidebarTextBackward) => {
                Ok(self.handle_sidebar_text_command(command))
            }
            command @ (CommandId::InsertModelSearch(_)
            | CommandId::DeleteModelSearchBackward
            | CommandId::SelectPreviousModel
            | CommandId::SelectNextModel
            | CommandId::PagePreviousModels
            | CommandId::PageNextModels
            | CommandId::SelectModel
            | CommandId::CloseModelPicker) => self.handle_model_picker_command(command),
            command @ (CommandId::InsertThemeSearch(_)
            | CommandId::DeleteThemeSearchBackward
            | CommandId::SelectPreviousTheme
            | CommandId::SelectNextTheme
            | CommandId::PagePreviousThemes
            | CommandId::PageNextThemes
            | CommandId::SelectTheme
            | CommandId::CloseThemePicker) => Ok(self.handle_theme_picker_command(command)),
            command @ (CommandId::InsertWorkspaceSearch(_)
            | CommandId::DeleteWorkspaceSearchBackward
            | CommandId::SelectPreviousWorkspace
            | CommandId::SelectNextWorkspace
            | CommandId::PagePreviousWorkspaces
            | CommandId::PageNextWorkspaces
            | CommandId::SelectWorkspace
            | CommandId::CloseWorkspacePicker) => Ok(self.handle_workspace_picker_command(command)),
            command @ (CommandId::SelectPreviousSubagent
            | CommandId::SelectNextSubagent
            | CommandId::OpenSelectedSubagent
            | CommandId::StopSelectedSubagent
            | CommandId::CloseSubagentPicker) => self.handle_subagent_picker_command(command),
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
    /// Agent Selection update instead of racing it. Choosing a Workspace is one
    /// of them: it opens the Landing, which takes over the Agent Selection from
    /// the Session being left.
    fn defers_for_agent_selection(&self, command: &CommandId) -> bool {
        self.state.selection_update_pending()
            && matches!(
                command,
                CommandId::SubmitSteer
                    | CommandId::SubmitQueue
                    | CommandId::SelectSession
                    | CommandId::SelectWorkspace
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
            // Down serves the composer first — caret movement within the
            // draft, then the history walk — and the Subagent Picker takes
            // exactly the key's one free meaning: Down at rest, which today
            // does nothing. With nothing to browse the open ask leaves the
            // view put, so the key stays as inert as it was.
            CommandId::HistoryNext => {
                if self.state.composer_down_is_inert() {
                    self.state.open_subagent_picker();
                } else {
                    self.state
                        .restore_composer_history(|composers, key| composers.history_next(key));
                }
            }
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
            _ => {}
        }
        Ok(ApplicationTransition::Continue)
    }

    /// Answers a click at one cell of the frame, asking the layers in the
    /// order they were drawn: the Sidebar owns the columns it drew, and a
    /// click it does not claim reaches the composer or Transcript beside it.
    fn handle_click(&mut self, position: Position) -> Result<ApplicationTransition> {
        // The Subagent Picker stands over everything below while it is up, so
        // it answers first: a press on one of its rows opens the Subagent the
        // row names — the same command Enter invokes — and a press anywhere
        // else puts the picker away, as it does for the Sidebar's menu.
        if self.state.subagent_picker.is_open() {
            let pressed = self.state.subagent_picker.hit(position);
            self.state.subagent_picker.close();
            return match pressed {
                Some(session_id) => {
                    let Some(session) = self.state.reference_in_current_origin(session_id) else {
                        return Ok(ApplicationTransition::Continue);
                    };
                    self.invoke_semantic(SemanticCommandId::SubagentOpen.on_session(session))
                }
                None => Ok(ApplicationTransition::Continue),
            };
        }
        let press = self.state.sidebar.press_at(position);
        if press != SidebarPress::Elsewhere {
            return self.answer_sidebar_press(press);
        }
        if let Some(target) = self
            .state
            .composers
            .hit(self.state.composer_key(), position)
        {
            return self.invoke_semantic(SemanticInvocation {
                id: SemanticCommandId::ComposerPlaceCursor,
                subject: SemanticSubject::ComposerCursor(target),
            });
        }
        match self.state.toggle_disclosure_at(position) {
            Some(invocation) => self.invoke_semantic(invocation),
            None => Ok(ApplicationTransition::Continue),
        }
    }

    /// Answers what one press of the Sidebar came to. A press mints no
    /// behavior of its own, so all that is left is to invoke the command it
    /// named.
    fn answer_sidebar_press(&mut self, press: SidebarPress) -> Result<ApplicationTransition> {
        match press {
            SidebarPress::Invoke(invocation) => self.invoke_semantic(invocation),
            SidebarPress::Answered => {
                let cancelled = self
                    .state
                    .cancel_workspace_resolution(WorkspaceResolutionSurface::Sidebar);
                Ok(if cancelled {
                    ApplicationTransition::CancelWorkspaceResolution(
                        WorkspaceResolutionSurface::Sidebar,
                    )
                } else {
                    ApplicationTransition::Continue
                })
            }
            SidebarPress::Elsewhere => Ok(ApplicationTransition::Continue),
        }
    }

    /// Handles the Workspace Picker's commands; any other command leaves the
    /// picker alone.
    fn handle_workspace_picker_command(&mut self, command: CommandId) -> ApplicationTransition {
        match command {
            CommandId::InsertWorkspaceSearch(text) => self.state.workspace_picker.insert(&text),
            CommandId::DeleteWorkspaceSearchBackward => {
                self.state.workspace_picker.delete_backward();
            }
            CommandId::SelectPreviousWorkspace => self.state.workspace_picker.select_previous(),
            CommandId::SelectNextWorkspace => self.state.workspace_picker.select_next(),
            CommandId::PagePreviousWorkspaces => self.state.workspace_picker.page_previous(),
            CommandId::PageNextWorkspaces => self.state.workspace_picker.page_next(),
            CommandId::CloseWorkspacePicker => {
                let cancelled = self
                    .state
                    .cancel_workspace_resolution(WorkspaceResolutionSurface::WorkspacePicker);
                self.state.workspace_picker.close();
                if cancelled {
                    return ApplicationTransition::CancelWorkspaceResolution(
                        WorkspaceResolutionSurface::WorkspacePicker,
                    );
                }
            }
            // Choosing a Workspace adopts it in full and lands the reader in
            // it. Before the listing arrives no row is the reader's, so there
            // is nothing to choose and the picker stands on its loading line.
            CommandId::SelectWorkspace => {
                if let Some(workspace) = self.state.workspace_picker.offer_selected() {
                    let request_id = self
                        .state
                        .begin_workspace_resolution(WorkspaceResolutionSurface::WorkspacePicker);
                    return ApplicationTransition::ResolveWorkspace {
                        outlook: self.state.outlook.clone(),
                        surface: WorkspaceResolutionSurface::WorkspacePicker,
                        request_id,
                        request: ResolveWorkspaceRequest {
                            base: None,
                            path: workspace,
                        },
                    };
                }
            }
            _ => {}
        }
        ApplicationTransition::Continue
    }

    /// Handles the Subagent Picker's commands; any other command leaves the
    /// picker alone.
    fn handle_subagent_picker_command(
        &mut self,
        command: CommandId,
    ) -> Result<ApplicationTransition> {
        match command {
            CommandId::SelectPreviousSubagent => self.state.move_subagent_selection(-1),
            CommandId::SelectNextSubagent => self.state.move_subagent_selection(1),
            CommandId::CloseSubagentPicker => self.state.subagent_picker.close(),
            CommandId::OpenSelectedSubagent => {
                if let Some(session_id) = self.state.selected_working_subagent() {
                    self.state.subagent_picker.close();
                    let Some(session) = self.state.reference_in_current_origin(session_id) else {
                        return Ok(ApplicationTransition::Continue);
                    };
                    return self
                        .invoke_semantic(SemanticCommandId::SubagentOpen.on_session(session));
                }
            }
            CommandId::StopSelectedSubagent => {
                // The picker stays up: the row the stop lands on settles out
                // of it live, and the reader keeps their place among the
                // Subagents still working. No confirmation — interrupting
                // never asks.
                if self.state.subagent_stop_offered()
                    && let Some(session_id) = self.state.selected_working_subagent()
                    && let Some(session) = self.state.reference_in_current_origin(session_id)
                {
                    return self
                        .invoke_semantic(SemanticCommandId::SubagentStop.on_session(session));
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
    fn session_picker_listing_transition(listing: SessionPickerListing) -> ApplicationTransition {
        match listing {
            SessionPickerListing::Origin(request) => ApplicationTransition::ListSessions(request),
            SessionPickerListing::Everywhere(request) => {
                ApplicationTransition::ListEverywhereRemotes(request)
            }
        }
    }

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
                let left_everywhere = self.state.session_picker.is_everywhere();
                return match self.state.session_picker.toggle_scope() {
                    SessionPickerListing::Origin(request) if left_everywhere => {
                        ApplicationTransition::ReconcileCatalogOrigins {
                            catalog_origins: self.state.catalog_origins(),
                            requests: vec![request],
                        }
                    }
                    listing => Self::session_picker_listing_transition(listing),
                };
            }
            CommandId::SelectSession => {
                let Some(target) = self.state.session_picker.begin_attachment() else {
                    return ApplicationTransition::Continue;
                };
                if target.origin == self.state.outlook {
                    self.state.session_picker.close();
                    self.state.open_session_route(target.clone());
                    return ApplicationTransition::ViewAndAttachSession(target);
                }
                let Some(workspace) = self.state.session_picker.workspace_of(&target) else {
                    return ApplicationTransition::Continue;
                };
                self.state
                    .turn_outlook_for_session(target.clone(), workspace);
                return ApplicationTransition::TurnOutlookAndViewAndAttach {
                    catalog_origins: self.state.catalog_origins(),
                    session: target,
                };
            }
            CommandId::CloseSessionPicker => self.edit_session_picker(SessionPicker::close),
            _ => {}
        }
        ApplicationTransition::Continue
    }

    /// Handles what the reader typed into the Sidebar; any other command leaves
    /// the line they are typing into alone. A Sidebar the reader has closed takes
    /// no typing, for the same reason it takes no arrows.
    fn handle_sidebar_text_command(&mut self, command: CommandId) -> ApplicationTransition {
        if self.state.sidebar.is_revealed() {
            let edits = matches!(
                &command,
                CommandId::InsertSidebarText(_) | CommandId::DeleteSidebarTextBackward
            );
            let cancelled = edits
                && self
                    .state
                    .cancel_workspace_resolution(WorkspaceResolutionSurface::Sidebar);
            match command {
                CommandId::InsertSidebarText(text) => self.state.sidebar.insert(&text),
                CommandId::DeleteSidebarTextBackward => self.state.sidebar.delete_backward(),
                _ => {}
            }
            if cancelled {
                return ApplicationTransition::CancelWorkspaceResolution(
                    WorkspaceResolutionSurface::Sidebar,
                );
            }
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
            SemanticCommandId::SidebarPrevious => self.state.sidebar.focus_previous(),
            SemanticCommandId::SidebarNext => self.state.sidebar.focus_next(),
            SemanticCommandId::SidebarLeave => {
                let cancelled = self
                    .state
                    .cancel_workspace_resolution(WorkspaceResolutionSurface::Sidebar);
                self.state.sidebar.leave();
                if cancelled {
                    return ApplicationTransition::CancelWorkspaceResolution(
                        WorkspaceResolutionSurface::Sidebar,
                    );
                }
            }
            SemanticCommandId::SidebarAttach => {
                let open = self.state.route.clone();
                let retry_open = self.state.open_session_can_retry();
                return match self.state.sidebar.activate(open.as_ref(), retry_open) {
                    SidebarActivation::Answered => ApplicationTransition::Continue,
                    SidebarActivation::ListEverywhereRemotes => {
                        ApplicationTransition::ListEverywhereRemotes(
                            self.state
                                .sidebar
                                .take_everywhere_remote_request()
                                .expect("choosing Everywhere queues its Remote discovery"),
                        )
                    }
                    SidebarActivation::CatalogOriginsChanged => {
                        ApplicationTransition::ReconcileCatalogOrigins {
                            catalog_origins: self.state.catalog_origins(),
                            requests: Vec::new(),
                        }
                    }
                    SidebarActivation::RetryCatalogOrigin(request) => {
                        ApplicationTransition::RetryCatalogOrigin(request)
                    }
                    // Enter and a press both arrive here, so both open the
                    // Session the same way: the route moves now and the
                    // attachment follows it.
                    SidebarActivation::Attach { session, workspace } => {
                        if session.origin == self.state.outlook {
                            self.state.open_session_route(session.clone());
                            ApplicationTransition::ViewAndAttachSession(session)
                        } else {
                            self.state
                                .turn_outlook_for_session(session.clone(), workspace);
                            ApplicationTransition::TurnOutlookAndViewAndAttach {
                                catalog_origins: self.state.catalog_origins(),
                                session,
                            }
                        }
                    }
                    // The path entry is a local act inside the Sidebar as
                    // well as a switch, so it does both: the client moves, and
                    // the column the reader typed into narrows to what they
                    // just said they meant.
                    SidebarActivation::ResolveWorkspace(request) => {
                        let request_id = self
                            .state
                            .begin_workspace_resolution(WorkspaceResolutionSurface::Sidebar);
                        ApplicationTransition::ResolveWorkspace {
                            outlook: self.state.outlook.clone(),
                            surface: WorkspaceResolutionSurface::Sidebar,
                            request_id,
                            request,
                        }
                    }
                };
            }
            _ => {}
        }
        ApplicationTransition::Continue
    }

    /// Handles the Sidebar context menu's commands routed here; any other
    /// command leaves the menu alone. A menu that is not open answers none of
    /// them, so an invocation arriving from elsewhere cannot act on a row the
    /// reader cannot see.
    fn handle_sidebar_menu_command(
        &mut self,
        command: SemanticCommandId,
    ) -> Result<ApplicationTransition> {
        if !self.state.sidebar.menu_is_open() {
            return Ok(ApplicationTransition::Continue);
        }
        match command {
            SemanticCommandId::SidebarMenuPrevious => self.state.sidebar.menu_select_previous(),
            SemanticCommandId::SidebarMenuNext => self.state.sidebar.menu_select_next(),
            SemanticCommandId::SidebarMenuClose => self.state.sidebar.close_menu(),
            SemanticCommandId::SidebarMenuSelect => {
                let press = self.state.sidebar.activate_menu_item();
                return self.answer_sidebar_press(press);
            }
            _ => {}
        }
        Ok(ApplicationTransition::Continue)
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

    /// Applies Theme picker movement entirely inside this Client. Only a
    /// confirmed choice becomes a Setting mutation for the runtime to send.
    fn handle_theme_picker_command(&mut self, command: CommandId) -> ApplicationTransition {
        let mut restore = None;
        let transition = match command {
            CommandId::InsertThemeSearch(text) => {
                self.state.theme_picker.insert(&text);
                ApplicationTransition::Continue
            }
            CommandId::DeleteThemeSearchBackward => {
                self.state.theme_picker.delete_backward();
                ApplicationTransition::Continue
            }
            CommandId::SelectPreviousTheme => {
                self.state.theme_picker.select_previous();
                ApplicationTransition::Continue
            }
            CommandId::SelectNextTheme => {
                self.state.theme_picker.select_next();
                ApplicationTransition::Continue
            }
            CommandId::PagePreviousThemes => {
                self.state.theme_picker.page_previous();
                ApplicationTransition::Continue
            }
            CommandId::PageNextThemes => {
                self.state.theme_picker.page_next();
                ApplicationTransition::Continue
            }
            CommandId::SelectTheme => self.state.theme_picker.confirm().map_or(
                ApplicationTransition::Continue,
                ApplicationTransition::MutateSetting,
            ),
            CommandId::CloseThemePicker => {
                restore = self.state.theme_picker.cancel();
                ApplicationTransition::Continue
            }
            _ => ApplicationTransition::Continue,
        };
        if let Some(name) = restore {
            self.resolve_theme_name(&name);
        } else {
            self.resolve_theme();
        }
        transition
    }

    fn choose_model(&mut self) -> Result<ApplicationTransition> {
        let purpose = self.state.model_picker.purpose();
        match self.state.model_picker.choose() {
            Some(ModelPickerAction::Retry) => Ok(self.state.model_picker.begin_retry().map_or(
                ApplicationTransition::Continue,
                ApplicationTransition::ListModels,
            )),
            Some(ModelPickerAction::Select(model)) => {
                let current = self
                    .state
                    .model_picker
                    .selection_in_force(self.state.agent_selection());
                self.state.model_picker.close();
                if model.options.is_empty() {
                    return self.apply_chosen_selection(model.default_agent_selection(), purpose);
                }
                self.state
                    .model_options
                    .open_for(model, current.as_ref(), purpose);
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
                return self.apply_to_selected_prompt(|session, prompt_id| {
                    ApplicationTransition::PromotePrompt { session, prompt_id }
                });
            }
            CommandId::CancelSelectedPrompt => {
                return self.apply_to_selected_prompt(|session, prompt_id| {
                    ApplicationTransition::CancelPrompt { session, prompt_id }
                });
            }
            CommandId::RequestInterrupt => self.request_interrupt(),
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
        act: impl FnOnce(SessionReference, PromptId) -> ApplicationTransition,
    ) -> ApplicationTransition {
        let (Some(session), CommandMode::QueuedPrompts { selected }) =
            (self.session_reference(), self.state.command_mode)
        else {
            return ApplicationTransition::Continue;
        };
        self.state.command_mode = CommandMode::Composer;
        act(session, selected)
    }

    fn confirm_interrupt(&mut self) -> ApplicationTransition {
        // Validate at the action boundary as well as on presentation ticks: a
        // delayed second Esc must not interrupt merely because the run loop
        // had no opportunity to draw between the two presses.
        self.state.reconcile_command_mode();
        let (
            Some(session),
            CommandMode::InterruptConfirmation {
                turn_id,
                armed_at: _,
            },
        ) = (self.session_reference(), self.state.command_mode)
        else {
            // This press was routed as a confirmation from the mode visible
            // before expiry was reconciled. It becomes the first press of a
            // fresh two-step gesture instead of interrupting late.
            self.request_interrupt();
            return ApplicationTransition::Continue;
        };
        self.state.command_mode = CommandMode::Composer;
        if let Some(turn_id) = turn_id {
            self.state.keep_interrupted_turn_open(turn_id);
        }
        ApplicationTransition::InterruptSession { session }
    }

    fn request_interrupt(&mut self) {
        // The gesture reaches whatever is running: the active Turn, or — with
        // none — the Subagents that outlived it. With neither there is nothing
        // to stop and the key stays inert.
        let turn_id = self.state.active_turn_id();
        if turn_id.is_some() || !self.state.working_subagent_ids().is_empty() {
            self.state.command_mode = CommandMode::InterruptConfirmation {
                turn_id,
                armed_at: self.state.presentation_clock.now(),
            };
        }
    }

    fn submit_prompt(&mut self, delivery: PromptDelivery) -> ApplicationTransition {
        // A Subagent's Session refuses Prompts, and its view offers no way to
        // write one; this guard keeps that true whatever surface asks.
        if self.state.open_subagent_parent().is_some() {
            return ApplicationTransition::Continue;
        }
        if self.state.pending_submission.is_some() {
            return ApplicationTransition::Continue;
        }
        // A Prompt is delivered to a Session, and a Session still loading is
        // not one yet. The draft stands where the reader wrote it and the
        // delivery waits, rather than racing a snapshot that may not come.
        if self.state.open_session_is_loading() {
            return ApplicationTransition::Continue;
        }
        let key = self.state.composer_key();
        if self.state.composers.text(key.clone()).trim().is_empty() {
            self.state.submission_error =
                Some("Prompt must contain non-whitespace text".to_owned());
            return ApplicationTransition::Continue;
        }
        self.state.sync_composer_completion();
        if let Some(error) = self.state.composers.skill_issue(key.clone()) {
            self.state.submission_error = Some(error.to_owned());
            self.state.composer_completion.dismiss_active();
            return ApplicationTransition::Continue;
        }
        let prompt = self.state.composers.begin_submission(key.clone());
        self.state.sync_composer_completion();
        self.state.failed_submissions.remove(&prompt.id);
        self.state.submission_error = None;
        if let ComposerKey::Session(session) = &key {
            self.state.pending_submission = Some(PendingSubmission {
                source: key.clone(),
                target: SubmissionTarget::AdmitPrompt(session.clone(), delivery),
                prompt: prompt.clone(),
            });
            return ApplicationTransition::AdmitPrompt {
                session: session.clone(),
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
            // The overlay owns input, including the release that would end a drag.
            self.state.left_press = None;
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
                    Ok(self.take_session_listing_transition())
                }
            }
        }
    }

    fn handle_origin_catalog(
        &mut self,
        outlook: Outlook,
        event: ManagedEvent,
    ) -> Result<ApplicationTransition> {
        if let ManagedEvent::RemoteFailed { status, message } = &event {
            let is_current = outlook == self.state.outlook;
            self.state.sidebar.end_origin(&outlook);
            self.state.session_picker.end_origin(&outlook);
            if !is_current {
                return Ok(ApplicationTransition::ReconcileCatalogOrigins {
                    catalog_origins: self.state.catalog_origins(),
                    requests: Vec::new(),
                });
            }
            let Some(name) = outlook.remote_name().map(str::to_owned) else {
                return Ok(ApplicationTransition::Continue);
            };
            let pending_prompt = self
                .state
                .pending_submission
                .as_ref()
                .filter(|pending| match &pending.target {
                    SubmissionTarget::CreateSession => true,
                    SubmissionTarget::AdmitPrompt(session, _) => session.origin == outlook,
                })
                .map(|pending| pending.prompt.id);
            if let Some(prompt_id) = pending_prompt {
                self.state
                    .fail_pending_submission(prompt_id, message.clone());
            }
            if let Some(reference) = self.state.session_reference.clone() {
                self.state
                    .composers
                    .recover_session_to_landing(reference.clone());
                self.state.session_interactions.remove(&reference);
            }
            self.state.connect_overlay.remote_failed(&name, *status);
            self.state.turn_outlook(Outlook::Local);
            self.state.settle_remote_failure(message.clone());
            self.state.sync_composer_completion();
            return Ok(ApplicationTransition::TurnOutlook {
                outlook: Outlook::Local,
                catalog_origins: self.state.catalog_origins(),
            });
        }
        let had_session = self.state.session.is_some();
        self.state.apply_origin_catalog(&outlook, event);
        self.state.reconcile_command_mode();
        if had_session && self.state.session.is_none() {
            Ok(ApplicationTransition::SessionEnded)
        } else {
            Ok(self.take_session_listing_transition())
        }
    }

    /// Whether an Origin-stamped catalog event can change client state now.
    /// The current Outlook takes every event; a background Origin participates
    /// only while Everywhere includes it, for catalog movement and the Remote
    /// reachability transitions represented beside its cached rows.
    pub(super) fn accepts_catalog_event(&self, outlook: &Outlook, event: &ManagedEvent) -> bool {
        let listed_by_sidebar = self.state.sidebar.includes_origin(outlook);
        let listed_by_picker = self.state.session_picker.includes_origin(outlook);
        outlook == &self.state.outlook
            || (event.moves_the_session_catalog() && (listed_by_sidebar || listed_by_picker))
            || ((listed_by_sidebar || listed_by_picker)
                && matches!(event, ManagedEvent::RemoteFailed { .. }))
            || (listed_by_sidebar
                && matches!(
                    event,
                    ManagedEvent::Recovering(_) | ManagedEvent::RemoteRecovered
                ))
    }

    fn attach_session(
        &mut self,
        reference: SessionReference,
        snapshot: SessionSnapshot,
    ) -> Result<ApplicationTransition> {
        let closes_picker = self.state.session_picker.attaching_to(&reference);
        let answers_sidebar = self.state.sidebar.attaching_to(&reference);
        let views_root = !snapshot.session.is_subagent();
        self.state.apply_attached_session(snapshot)?;
        if closes_picker {
            self.state.session_picker.close();
        }
        if answers_sidebar {
            self.state.sidebar.finish_attachment();
        }
        Ok(if views_root {
            ApplicationTransition::ViewSession(reference)
        } else {
            ApplicationTransition::Continue
        })
    }

    fn fail_open_session_attachment(&mut self, error: String) -> ApplicationTransition {
        self.state.fail_opening_session(error);
        self.state.sidebar.fail_attachment();
        ApplicationTransition::Continue
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
        self.state.outlook_landing_selections.insert(
            self.state.outlook.clone(),
            self.state.landing_agent_selection.clone(),
        );
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
    ) -> Option<SessionReference> {
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
                .session
        })
    }

    fn accept_agent_selection_update(
        &mut self,
        operation_id: AgentSelectionOperationId,
        selection: AgentSelection,
    ) -> Result<ApplicationTransition> {
        let Some(session) = self.take_pending_agent_selection(operation_id) else {
            return Ok(ApplicationTransition::Continue);
        };
        self.state.submission_error = None;
        if let Some((queued_session, queued)) = self.state.queued_agent_selection.take()
            && queued_session == session
            && queued != selection
        {
            self.state.confirmed_agent_selection = Some((session.clone(), selection));
            return self.begin_agent_selection_update(queued_session, queued);
        }
        self.state.confirmed_agent_selection = Some((session, selection));
        Ok(ApplicationTransition::Continue)
    }

    fn fail_agent_selection_update(
        &mut self,
        operation_id: AgentSelectionOperationId,
        error: String,
    ) -> Result<ApplicationTransition> {
        let Some(session) = self.take_pending_agent_selection(operation_id) else {
            return Ok(ApplicationTransition::Continue);
        };
        if let Some((queued_session, queued)) = self.state.queued_agent_selection.take()
            && queued_session == session
        {
            // A newer queued selection supersedes this failure.
            return self.begin_agent_selection_update(queued_session, queued);
        }
        // Keep any prior confirmed acceptance: it is newer authoritative state
        // than the snapshot base.
        self.state.submission_error = Some(error);
        Ok(ApplicationTransition::Continue)
    }

    /// Input may arrive after a Session update and before the next draw. Resolve
    /// selection validity against that update before acting on cached cells.
    fn refresh_text_selection(&self) {
        if self
            .state
            .text_selection
            .get()
            .is_some_and(|selection| !self.state.selection_surface_visible(selection.surface))
        {
            self.state.text_selection.set(None);
        }
        if self.state.text_selection.get().is_none()
            && self
                .state
                .left_press
                .as_ref()
                .is_none_or(|press| press.selection_anchor.is_none())
        {
            return;
        }
        let Some(interaction) = self
            .state
            .session_reference
            .as_ref()
            .and_then(|reference| self.state.session_interaction(reference))
        else {
            return;
        };
        let Some(width) = interaction
            .viewport
            .borrow()
            .as_ref()
            .map(|view| view.content_width)
        else {
            return;
        };
        self.state.transcript_view(&self.theme, width);
        if self.state.text_selection.get().is_some_and(|selection| {
            selection.surface == SelectionSurface::Transcript
                && selection.epoch != self.state.transcript_cache.selection_epoch()
        }) {
            self.state.text_selection.set(None);
        }
    }

    /// Makes the word under a Transcript cell the standing Text Selection,
    /// or leaves none when no word is there.
    fn select_transcript_word(&mut self, position: Position) {
        if self.state.overlay_owns_input()
            || self.state.reconnect_overlay_visible
            || self.state.active_selection_overlay_area().is_some()
        {
            return;
        }
        let Some(interaction) = self
            .state
            .session_reference
            .as_ref()
            .and_then(|session| self.state.session_interaction(session))
        else {
            return;
        };
        let viewport = interaction.viewport.borrow();
        let Some(viewport) = viewport.as_ref() else {
            return;
        };
        let Some(row) = viewport.transcript_row(position) else {
            return;
        };
        let column = usize::from(position.x - viewport.content_left);
        let Some((anchor, focus)) = self.state.transcript_cache.word_cells(row, column) else {
            return;
        };
        self.state.text_selection.set(Some(TextSelection {
            surface: SelectionSurface::Transcript,
            anchor,
            focus,
            epoch: self.state.transcript_cache.selection_epoch(),
            granularity: SelectionGranularity::Word,
        }));
    }

    /// Distance beyond the Transcript determines rows per presentation tick.
    fn transcript_drag_scroll(&self) -> Option<(TranscriptDirection, usize)> {
        let press = self.state.left_press.as_ref()?;
        let (_, epoch, surface) = press.selection_anchor?;
        if surface != SelectionSurface::Transcript {
            return None;
        }
        if !press.dragged || epoch != self.state.transcript_cache.selection_epoch() {
            return None;
        }
        let interaction = self
            .state
            .session_interaction(self.state.session_reference.as_ref()?)?;
        let viewport = interaction.viewport.borrow();
        let viewport = viewport.as_ref()?;
        if viewport.content_rows == 0 || viewport.content_width == 0 {
            return None;
        }
        let bottom = viewport.content_top + viewport.content_rows;
        if press.pointer.y < viewport.content_top {
            Some((
                TranscriptDirection::Up,
                usize::from(viewport.content_top - press.pointer.y),
            ))
        } else if press.pointer.y >= bottom {
            Some((
                TranscriptDirection::Down,
                usize::from(press.pointer.y - bottom) + 1,
            ))
        } else {
            None
        }
    }

    fn update_text_selection_drag(&mut self, position: Position, scroll_rows: usize) {
        let Some(press) = &mut self.state.left_press else {
            return;
        };
        press.pointer = position;
        press.dragged |= position != press.position;
        if let Some((anchor, epoch, surface)) = press.selection_anchor
            && surface != SelectionSurface::Transcript
        {
            if press.dragged
                && let Some(frame) = self
                    .state
                    .selection_frames
                    .borrow()
                    .iter()
                    .find(|frame| frame.surface == surface && frame.epoch() == epoch)
                && let Some(focus) = frame.cell(position)
            {
                self.state
                    .text_selection
                    .set((anchor != focus).then_some(TextSelection {
                        anchor,
                        focus,
                        epoch,
                        surface,
                        granularity: SelectionGranularity::Cell,
                    }));
            }
            return;
        }
        if scroll_rows > 0
            && let Some((direction, _)) = self.transcript_drag_scroll()
        {
            self.state.navigate_transcript(direction, Some(scroll_rows));
        }
        if let Some(press) = &self.state.left_press
            && press.dragged
            && let Some((anchor, epoch, surface)) = press.selection_anchor
            && epoch == self.state.transcript_cache.selection_epoch()
            && let Some(interaction) = self
                .state
                .session_reference
                .as_ref()
                .and_then(|session| self.state.session_interactions.get(session))
            && let Some(viewport) = interaction.viewport.borrow().as_ref()
            && viewport.content_width > 0
            && viewport.content_rows > 0
        {
            let row = viewport.scroll_position
                + usize::from(
                    position.y.clamp(
                        viewport.content_top,
                        viewport.content_top + viewport.content_rows - 1,
                    ) - viewport.content_top,
                );
            let column = usize::from(
                position.x.clamp(
                    viewport.content_left,
                    viewport.content_left + viewport.content_width - 1,
                ) - viewport.content_left,
            );
            let focus = SelectionCell {
                row: row.min(self.state.transcript_cache.row_count().saturating_sub(1)),
                column,
            };
            self.state
                .text_selection
                .set((anchor != focus).then_some(TextSelection {
                    anchor,
                    focus,
                    epoch,
                    surface,
                    granularity: SelectionGranularity::Cell,
                }));
        }
    }

    /// Runs one semantic command against what it names. Every surface that can
    /// drive the view — a keybinding, a slash command, a click, and one day a
    /// plugin — arrives here, so a behavior is defined once and invoked by
    /// its ID rather than reimplemented per input.
    fn invoke_semantic(
        &mut self,
        invocation: impl Into<SemanticInvocation>,
    ) -> Result<ApplicationTransition> {
        self.refresh_text_selection();
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
        // Choice pickers can sit above the settings panel. Semantic
        // invocations obey the same ownership as terminal input: an editor
        // hidden beneath a newer overlay accepts nothing.
        if (self.state.theme_picker.is_open() || self.state.model_picker.is_open())
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
            SemanticCommandId::PointerClick => {
                let SemanticSubject::ScreenPosition(position) = invocation.subject else {
                    return Ok(ApplicationTransition::Continue);
                };
                if self
                    .state
                    .active_selection_overlay_area()
                    .is_some_and(|area| !area.contains(position))
                {
                    return self
                        .command_for_input_mode(InputEvent::Key(KeyEvent::new(
                            KeyCode::Esc,
                            KeyModifiers::NONE,
                        )))
                        .map_or(Ok(ApplicationTransition::Continue), |command| {
                            self.handle_command(command)
                        });
                }
                // Resolve a recognized click through the same mode table as other
                // input, so overlays retain their ownership of pointer actions.
                let event = InputEvent::Mouse(MouseEvent {
                    kind: MouseEventKind::Up(MouseButton::Left),
                    column: position.x,
                    row: position.y,
                    modifiers: KeyModifiers::NONE,
                });
                self.command_for_input_mode(event)
                    .map_or(Ok(ApplicationTransition::Continue), |command| {
                        self.handle_command(command)
                    })
            }
            SemanticCommandId::PointerDrag => {
                if let SemanticSubject::ScreenPosition(position) = invocation.subject {
                    self.update_text_selection_drag(position, 1);
                }
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::TextSelectionWord => {
                if let SemanticSubject::ScreenPosition(position) = invocation.subject {
                    self.select_transcript_word(position);
                }
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::TextSelectionClear => {
                self.state.text_selection.set(None);
                if let Some(press) = &mut self.state.left_press {
                    press.selection_anchor = None;
                }
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::TextSelectionCopy => Ok(self
                .state
                .text_selection
                .get()
                .and_then(|selection| match selection.surface {
                    SelectionSurface::Transcript => {
                        self.state.transcript_cache.copy_selection(selection)
                    }
                    SelectionSurface::Composer => self
                        .state
                        .composers
                        .selection_frame()
                        .and_then(|frame| frame.copy(selection))
                        .map(Into::into),
                    _ => self
                        .state
                        .selection_frames
                        .borrow()
                        .iter()
                        .find(|frame| {
                            frame.surface == selection.surface && frame.epoch() == selection.epoch
                        })
                        .and_then(|frame| frame.copy(selection))
                        .map(Into::into),
                })
                .map_or(
                    ApplicationTransition::Continue,
                    ApplicationTransition::CopyToClipboard,
                )),
            SemanticCommandId::ComposerPlaceCursor => {
                if !self.state.overlay_owns_input()
                    && !self.state.reconnect_overlay_visible
                    && self.state.open_subagent_parent().is_none()
                    && let SemanticSubject::ComposerCursor(target) = invocation.subject
                {
                    self.state.sidebar.hand_back_keys();
                    self.state.command_mode = CommandMode::Composer;
                    self.state
                        .navigate_composer(|composers, key| composers.place_cursor(key, target));
                }
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::ApplicationExit => Ok(ApplicationTransition::Exit),
            SemanticCommandId::ConnectOpen => {
                self.state.connect_overlay.open();
                self.state.command_mode = CommandMode::Composer;
                Ok(ApplicationTransition::BeginConnecting)
            }
            SemanticCommandId::ConnectConfirm => {
                if self.state.connect_overlay.confirm() {
                    return Ok(ApplicationTransition::Continue);
                }
                if let Some(request) = self.state.connect_overlay.begin_redemption() {
                    return Ok(ApplicationTransition::RedeemInvite(request));
                }
                Ok(self.state.connect_overlay.begin_preview().map_or(
                    ApplicationTransition::Continue,
                    ApplicationTransition::PreviewInvite,
                ))
            }
            SemanticCommandId::ConnectFocusNext => {
                self.state.connect_overlay.focus_next();
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::ConnectPrevious => {
                self.state.connect_overlay.select_previous();
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::ConnectNext => {
                self.state.connect_overlay.select_next();
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::OutlookSelect => {
                let Some(outlook) = self.state.connect_overlay.selected_outlook() else {
                    return Ok(ApplicationTransition::Continue);
                };
                self.state.connect_overlay.close();
                if self.state.outlook == outlook {
                    return Ok(ApplicationTransition::Continue);
                }
                self.state.turn_outlook(outlook.clone());
                if matches!(outlook, Outlook::Remote(_)) {
                    self.state
                        .begin_workspace_resolution(WorkspaceResolutionSurface::Outlook);
                }
                let catalog_origins = self.state.catalog_origins();
                Ok(ApplicationTransition::TurnOutlook {
                    outlook,
                    catalog_origins,
                })
            }
            SemanticCommandId::ConnectMoveAddressUp => {
                self.state.connect_overlay.move_address_up();
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::ConnectMoveAddressDown => {
                self.state.connect_overlay.move_address_down();
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::ConnectPairAnother => {
                self.state.connect_overlay.pair_another();
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::ConnectClose => {
                self.state.connect_overlay.close();
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::ServeOpen => {
                let enable = !self.state.settings.serving.enabled;
                let port = self.state.settings.serving.port;
                self.state.serve_overlay.open();
                self.state.command_mode = CommandMode::Composer;
                Ok(ApplicationTransition::BeginServing { enable, port })
            }
            SemanticCommandId::ServePrevious => {
                self.state.serve_overlay.select_previous();
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::ServeNext => {
                self.state.serve_overlay.select_next();
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::ServeToggleAddress => {
                self.state.serve_overlay.toggle_selected();
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::ServeConfirm => Ok(self.state.serve_overlay.issue_request().map_or(
                ApplicationTransition::Continue,
                ApplicationTransition::IssueInvite,
            )),
            SemanticCommandId::ServeCopyInvite => {
                Ok(self.state.serve_overlay.copy_text().map(Into::into).map_or(
                    ApplicationTransition::Continue,
                    ApplicationTransition::CopyToClipboard,
                ))
            }
            SemanticCommandId::ServeRemovePeer => {
                Ok(self.state.serve_overlay.remove_selected().map_or(
                    ApplicationTransition::Continue,
                    ApplicationTransition::RemovePeer,
                ))
            }
            SemanticCommandId::ServeClose => {
                self.state.serve_overlay.close();
                Ok(ApplicationTransition::Continue)
            }
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
            SemanticCommandId::ThemeList => {
                self.refresh_user_themes();
                let current = self.state.settings.appearance.theme.clone();
                self.state.theme_picker.open(
                    &current,
                    |theme| SettingMutation::AppearanceTheme { value: Some(theme) },
                    &self.theme_catalog,
                );
                self.resolve_theme();
                self.state.command_mode = CommandMode::Composer;
                Ok(ApplicationTransition::Continue)
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
                let listing = self.state.session_picker.open();
                self.state.command_mode = CommandMode::Composer;
                Ok(Self::session_picker_listing_transition(listing))
            }
            SemanticCommandId::QuestionnaireRequestPrevious
            | SemanticCommandId::QuestionnaireRequestNext => {
                if let Some(session) = self.state.session_reference.clone() {
                    let requests: Vec<_> = self.state.pending_questionnaires().cloned().collect();
                    if !requests.is_empty() {
                        let current = requests
                            .iter()
                            .position(|q| Some(q.id) == self.state.questionnaires.id());
                        let index = match current {
                            None => 0,
                            Some(index)
                                if command == SemanticCommandId::QuestionnaireRequestNext =>
                            {
                                (index + 1) % requests.len()
                            }
                            Some(index) => (index + requests.len() - 1) % requests.len(),
                        };
                        self.state.questionnaires.open(session, &requests[index]);
                    }
                }
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::QuestionnaireOpen => {
                let questionnaire = self
                    .state
                    .pending_questionnaires()
                    .find(|q| match invocation.subject {
                        SemanticSubject::Questionnaire(id) => q.id == id,
                        _ => true,
                    })
                    .cloned();
                if let (Some(session), Some(questionnaire)) =
                    (self.state.session_reference.clone(), questionnaire)
                {
                    self.state.questionnaires.open(session, &questionnaire);
                }
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::QuestionnaireHide => {
                self.state.questionnaires.hide();
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::QuestionnaireDecline => {
                if let (Some(session), Some(id)) = (
                    self.state.session_reference.clone(),
                    self.state.open_questionnaire().map(|q| q.id),
                ) {
                    if !self.state.questionnaires.begin_decline() {
                        return Ok(ApplicationTransition::Continue);
                    }
                    Ok(ApplicationTransition::SubmitQuestionnaire {
                        session,
                        id,
                        submission: crate::protocol::QuestionnaireSubmission::Decline,
                    })
                } else {
                    Ok(ApplicationTransition::Continue)
                }
            }
            SemanticCommandId::QuestionnaireBack
            | SemanticCommandId::QuestionnaireNext
            | SemanticCommandId::QuestionnaireOmit
            | SemanticCommandId::QuestionnaireScrollUp
            | SemanticCommandId::QuestionnaireScrollDown
            | SemanticCommandId::QuestionnaireChoicePrevious
            | SemanticCommandId::QuestionnaireChoiceNext
            | SemanticCommandId::QuestionnaireSelect
            | SemanticCommandId::QuestionnaireReview
            | SemanticCommandId::QuestionnaireSubmit => {
                if let Some(questionnaire) = self.state.open_questionnaire().cloned()
                    && let Some(answer) = self.state.questionnaires.command(&questionnaire, command)
                    && let Some(session) = self.state.session_reference.clone()
                {
                    return Ok(ApplicationTransition::SubmitQuestionnaire {
                        session,
                        id: questionnaire.id,
                        submission: crate::protocol::QuestionnaireSubmission::Answer { answer },
                    });
                }
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::WorkspaceList => {
                let request = self.state.workspace_picker.open();
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
            SemanticCommandId::SubagentBrowse => {
                self.state.open_subagent_picker();
                Ok(ApplicationTransition::Continue)
            }
            // The child Session is the command's subject, so an invocation
            // that names none has nothing to open and leaves the view put.
            SemanticCommandId::SubagentOpen => Ok(match invocation.subject {
                SemanticSubject::Session(session) => ApplicationTransition::AttachSession(session),
                SemanticSubject::View
                | SemanticSubject::ScreenPosition(_)
                | SemanticSubject::ComposerCursor(_)
                | SemanticSubject::Turn(_)
                | SemanticSubject::Questionnaire(_)
                | SemanticSubject::Origin(_) => ApplicationTransition::Continue,
            }),
            // Stopping a Subagent is interrupting its child Session, on the
            // same subject terms as opening one.
            SemanticCommandId::SubagentStop => Ok(match invocation.subject {
                SemanticSubject::Session(session) => {
                    ApplicationTransition::InterruptSession { session }
                }
                SemanticSubject::View
                | SemanticSubject::ScreenPosition(_)
                | SemanticSubject::ComposerCursor(_)
                | SemanticSubject::Turn(_)
                | SemanticSubject::Questionnaire(_)
                | SemanticSubject::Origin(_) => ApplicationTransition::Continue,
            }),
            // Leaving acts on the Session the reader is in: only a Subagent's
            // Session has a parent to return to, so anywhere else the command
            // has nowhere to go and leaves the view put.
            SemanticCommandId::SubagentLeave => Ok(self
                .state
                .open_subagent_parent()
                .zip(self.session_reference())
                .map_or(ApplicationTransition::Continue, |(parent, session)| {
                    ApplicationTransition::ViewAndAttachSession(SessionReference::new(
                        session.origin,
                        parent,
                    ))
                })),
            // The Turn is the command's subject, so an invocation that names
            // none has no Turn to flip and leaves the view where it is.
            SemanticCommandId::TranscriptTurnToggle => {
                if let SemanticSubject::Turn(turn_id) = invocation.subject {
                    self.state.toggle_turn_fold(turn_id);
                }
                Ok(ApplicationTransition::Continue)
            }
            // The command acts on the Session it names — a Sidebar row names
            // one — and on the Session the reader is in where it names none,
            // so on the Landing there is nothing to set aside and the view
            // stays put.
            command @ (SemanticCommandId::SessionSettle | SemanticCommandId::SessionUnsettle) => {
                self.state.command_mode = CommandMode::Composer;
                let settled = command == SemanticCommandId::SessionSettle;
                let named = match invocation.subject {
                    SemanticSubject::Session(session) => Some(session),
                    SemanticSubject::View
                    | SemanticSubject::ScreenPosition(_)
                    | SemanticSubject::ComposerCursor(_)
                    | SemanticSubject::Turn(_)
                    | SemanticSubject::Questionnaire(_)
                    | SemanticSubject::Origin(_) => self.session_reference(),
                };
                Ok(named.map_or(ApplicationTransition::Continue, |session| {
                    ApplicationTransition::SettleSession { session, settled }
                }))
            }
            command @ (SemanticCommandId::SidebarPrevious
            | SemanticCommandId::SidebarNext
            | SemanticCommandId::SidebarAttach
            | SemanticCommandId::SidebarLeave) => Ok(self.handle_sidebar_command(command)),
            command @ (SemanticCommandId::SidebarMenuPrevious
            | SemanticCommandId::SidebarMenuNext
            | SemanticCommandId::SidebarMenuSelect
            | SemanticCommandId::SidebarMenuClose) => self.handle_sidebar_menu_command(command),
            SemanticCommandId::SidebarToggle => {
                let cancelled = self
                    .state
                    .cancel_workspace_resolution(WorkspaceResolutionSurface::Sidebar);
                let open = self.state.route.clone();
                self.state.sidebar.toggle(open.as_ref());
                self.state.command_mode = CommandMode::Composer;
                if cancelled {
                    return Ok(ApplicationTransition::CancelWorkspaceResolution(
                        WorkspaceResolutionSurface::Sidebar,
                    ));
                }
                Ok(self.take_session_listing_transition())
            }
            SemanticCommandId::RemoteRetry => Ok(match invocation.subject {
                SemanticSubject::Origin(outlook) => {
                    self.state.sidebar.retry_origin(outlook).map_or(
                        ApplicationTransition::Continue,
                        ApplicationTransition::RetryCatalogOrigin,
                    )
                }
                SemanticSubject::View
                | SemanticSubject::ScreenPosition(_)
                | SemanticSubject::ComposerCursor(_)
                | SemanticSubject::Turn(_)
                | SemanticSubject::Session(_)
                | SemanticSubject::Questionnaire(_) => ApplicationTransition::Continue,
            }),
            // A command naming a Session takes that one away: the surface
            // that named it has already had the reader say it twice, which is
            // what asking again is for. Naming none means the row the session
            // picker is on, which asks there.
            SemanticCommandId::SessionDelete => {
                if let SemanticSubject::Session(session) = invocation.subject {
                    self.state.sidebar.begin_deletion(session.clone());
                    return Ok(ApplicationTransition::DeleteSession(session));
                }
                Ok(self.state.session_picker.begin_deletion().map_or(
                    ApplicationTransition::Continue,
                    ApplicationTransition::DeleteSession,
                ))
            }
            SemanticCommandId::SessionNew => Ok(self.open_landing()),
        }
    }

    /// Opens the Landing: a cleared composer standing ready for the first
    /// Prompt of a Session in the Workspace this client works in, carrying
    /// over whatever Agent was chosen for the Session being left.
    ///
    /// A Session open when it opens is left where it stands — still listed,
    /// and still working if it is mid-Turn. Only this client stops watching
    /// it, which is why nothing is asked before it happens.
    fn open_landing(&mut self) -> ApplicationTransition {
        let inherited_selection = self.state.agent_selection().cloned();
        let source = self.state.composer_key();
        self.state.composers.clear(source);
        self.state.composers.clear(ComposerKey::Landing);
        self.state.submission_error = None;
        self.state.command_mode = CommandMode::Composer;
        let detached = self.state.leave_session_route();
        if detached {
            self.state.landing_agent_selection = inherited_selection.clone();
            self.state.confirmed_landing_agent_selection = inherited_selection;
            self.state.pending_landing_agent_selection = None;
            self.state.queued_landing_agent_selection = None;
            self.state.confirmed_agent_selection = None;
        }
        self.state.session_events_blocked = detached;
        self.state.abandon_pending_attachment();
        self.state.sync_composer_completion();
        // Detaching whether or not a Session was on screen: a Session being
        // opened is one the reader is on their way to, and the Landing is
        // them saying they are not going. The client has to let go of it here
        // or its answer arrives as an answer to a question they stopped
        // asking.
        ApplicationTransition::DetachSession
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
                        SettingChoiceSurface::Theme { current, pin } => {
                            self.refresh_user_themes();
                            let current = current(&self.state.settings);
                            self.state
                                .theme_picker
                                .open(&current, pin, &self.theme_catalog);
                            self.resolve_theme();
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
            SemanticCommandId::ModelOptionsSelect => {
                if self.state.model_options.is_confirm_selected() {
                    return self.apply_model_options();
                }
                self.state.model_options.choose();
            }
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
        let purpose = self.state.model_options.purpose();
        self.state.model_options.close();
        self.apply_chosen_selection(selection, purpose)
    }

    fn apply_chosen_selection(
        &mut self,
        selection: AgentSelection,
        purpose: ModelPickerPurpose,
    ) -> Result<ApplicationTransition> {
        match purpose {
            ModelPickerPurpose::AgentSelection => self.apply_agent_selection(selection),
            ModelPickerPurpose::Setting(pin) => {
                Ok(ApplicationTransition::MutateSetting(pin(selection)))
            }
        }
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
        // A Session still loading has no Selection to change, and the Landing
        // it has already left is not the one to change instead.
        if self.state.open_session_is_loading() {
            return Ok(ApplicationTransition::Continue);
        }
        let Some(session) = self.session_reference() else {
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
            self.state.queued_agent_selection = Some((session, selection));
            return Ok(ApplicationTransition::Continue);
        }
        self.begin_agent_selection_update(session, selection)
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
        session: SessionReference,
        selection: AgentSelection,
    ) -> Result<ApplicationTransition> {
        let operation_id = AgentSelectionOperationId::new();
        self.state.pending_agent_selection = Some(PendingAgentSelection {
            session: session.clone(),
            operation_id,
            selection: selection.clone(),
        });
        Ok(ApplicationTransition::UpdateAgentSelection {
            session,
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
        render_with_slots(
            frame,
            &self.state,
            &self.slots,
            &self.theme,
            self.terminal_facts.truecolor,
        );
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
    /// are still interactions, and the Application's Notice is dismissed by any of
    /// them. A resize, a focus change, and the mouse merely passing over the
    /// window are the terminal's doing rather than the reader's, so they leave
    /// the Notice standing. Reports whether anything on screen changed, so a
    /// caller that draws on demand knows to redraw.
    pub fn note_interaction(&mut self, event: &InputEvent) -> bool {
        is_reader_interaction(event) && self.state.application_notice.dismiss()
    }

    /// Drains listing work queued by the independent Session surfaces. The
    /// Sidebar can first need Remote discovery; otherwise every queued
    /// per-Origin request is dispatched together without collapsing either
    /// surface's scope or catalog ownership.
    pub fn take_session_listing_transition(&mut self) -> ApplicationTransition {
        if let Some(request_id) = self.state.sidebar.take_everywhere_remote_request() {
            return ApplicationTransition::ListEverywhereRemotes(request_id);
        }
        let mut requests = self.state.sidebar.take_listing_requests();
        requests.extend(self.state.session_picker.take_listing_requests());
        match requests {
            requests if requests.is_empty() => ApplicationTransition::Continue,
            mut requests if requests.len() == 1 => {
                ApplicationTransition::ListSessions(requests.pop().expect("one request"))
            }
            requests => ApplicationTransition::ReconcileCatalogOrigins {
                catalog_origins: self.state.catalog_origins(),
                requests,
            },
        }
    }

    pub(super) fn accepts_everywhere_remotes(&self, request: EverywhereListRequest) -> bool {
        match request.surface() {
            SessionListSurface::SessionPicker => self
                .state
                .session_picker
                .accepts_everywhere_remotes(request),
            SessionListSurface::Sidebar => self.state.sidebar.accepts_everywhere_remotes(request),
            SessionListSurface::WorkspacePicker => false,
        }
    }

    /// Whether a listing the server answered with would move anything on
    /// screen. A caller that draws on demand asks this before handing the
    /// listing over: the Sidebar catches up with every session-catalog change,
    /// and most of them it has already taken in place, so most answers carry
    /// the listing already drawn — which an idle TUI must not pay a frame for
    /// (ADR 0007). Such an answer names the same Sessions the surface already
    /// holds, so nothing adopting it does besides replacing them — moving a
    /// selection, forgetting what a departed Session stood under — has
    /// anything to move either.
    pub fn listing_moves_the_frame(
        &self,
        request: &SessionListRequest,
        sessions: &[SessionListItem],
    ) -> bool {
        match request.surface() {
            SessionListSurface::SessionPicker => {
                self.state.session_picker.would_move(request, sessions)
            }
            SessionListSurface::WorkspacePicker => {
                self.state.workspace_picker.would_move(request, sessions)
            }
            SessionListSurface::Sidebar => self.state.sidebar.would_move(request, sessions),
        }
    }

    /// Whether a reply the server sent answers the listing a surface is still
    /// waiting for. A straggler from a listing the reader has moved past lands
    /// nowhere, so it moves nothing on screen — a refusal included.
    pub fn awaits_listing(&self, request: &SessionListRequest) -> bool {
        match request.surface() {
            SessionListSurface::SessionPicker => self.state.session_picker.awaits_listing(request),
            SessionListSurface::WorkspacePicker => {
                self.state.workspace_picker.awaits_listing(request)
            }
            SessionListSurface::Sidebar => self.state.sidebar.awaits_listing(request),
        }
    }

    /// Translates a terminal event through the active input mode. `None` means
    /// the event changes nothing, so callers can skip redrawing.
    pub fn command_for_terminal_input(&self, event: InputEvent) -> Option<CommandId> {
        self.refresh_text_selection();
        if self.state.text_selection.get().is_some() {
            match &event {
                InputEvent::Key(key)
                    if key.kind != KeyEventKind::Release
                        && key.code == KeyCode::Char('c')
                        && key.modifiers == KeyModifiers::CONTROL =>
                {
                    return Some(CommandId::ClearOrExit);
                }
                InputEvent::Mouse(mouse)
                    if mouse.kind == MouseEventKind::Down(MouseButton::Right) =>
                {
                    return Some(CommandId::OpenContextMenuAt {
                        position: Position::new(mouse.column, mouse.row),
                    });
                }
                _ => {}
            }
        }
        if matches!(&event, InputEvent::Key(key) if key.code == KeyCode::Esc)
            && self.state.text_selection.get().is_some()
        {
            return Some(CommandId::InvokeSemantic(
                SemanticCommandId::TextSelectionClear,
            ));
        }
        if matches!(&event, InputEvent::Resize(..)) {
            return Some(CommandId::InvokeSemantic(
                SemanticCommandId::TextSelectionClear,
            ));
        }
        if let InputEvent::Mouse(mouse) = &event {
            let position = Position::new(mouse.column, mouse.row);
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    return Some(CommandId::PressAt { position });
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    return Some(CommandId::DragAt { position });
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    return Some(CommandId::ReleaseAt { position });
                }
                _ => {}
            }
        }
        self.command_for_input_mode(event)
    }

    fn command_for_input_mode(&self, event: InputEvent) -> Option<CommandId> {
        match self.state.top_selection_overlay() {
            Some(SelectionSurface::Connect) => {
                return command_for_connect_overlay_event(
                    event,
                    self.state.connect_overlay.input_mode(),
                );
            }
            Some(SelectionSurface::Serve) => return command_for_serve_overlay_event(event),
            Some(SelectionSurface::Themes) => return command_for_theme_picker_event(event),
            Some(SelectionSurface::Models) => return command_for_model_picker_event(event),
            Some(SelectionSurface::ModelOptions) => return command_for_model_options_event(event),
            Some(SelectionSurface::NumericEditor) => {
                return command_for_numeric_editor_event(event);
            }
            Some(SelectionSurface::Settings) => return command_for_settings_panel_event(event),
            Some(SelectionSurface::Workspaces) => return command_for_workspace_picker_event(event),
            Some(SelectionSurface::Sessions) => return command_for_session_picker_event(event),
            Some(SelectionSurface::SidebarMenu) => return command_for_sidebar_menu_event(event),
            Some(SelectionSurface::Subagents) => return command_for_subagent_picker_event(event),
            _ => {}
        }
        // The Sidebar comes after every overlay and before the composer's own
        // surfaces: it stands beside the main view rather than over it, so an
        // overlay a reader opened is still the newer surface and owns the keys,
        // while a completion list left standing over the composer does not.
        if self.state.sidebar_owns_input() {
            return command_for_sidebar_event(event);
        }
        if self
            .state
            .questionnaires
            .is_open(self.state.session_reference.as_ref())
        {
            if matches!(self.state.command_mode, CommandMode::Leader) {
                return command_for_leader_event(event);
            }
            if let InputEvent::Paste(text) = &event {
                return Some(CommandId::QuestionnaireInsert(text.clone()));
            }
            if let Some(command) = super::questionnaire::key(&event) {
                return Some(command);
            }
            return match command_for_terminal_event(event) {
                Some(
                    command @ (CommandId::ScrollTranscriptPageUp
                    | CommandId::ScrollTranscriptPageDown
                    | CommandId::ScrollTranscriptLinesUp
                    | CommandId::ScrollTranscriptLinesDown
                    | CommandId::FollowLatest
                    | CommandId::BeginLeader
                    | CommandId::InvokeSemantic(_)
                    | CommandId::ClickAt { .. }
                    | CommandId::OpenContextMenuAt { .. }),
                ) => Some(command),
                _ => None,
            };
        }
        if self.state.composer_completion.is_visible()
            && let Some(command) = command_for_completion_event(event.clone())
        {
            return Some(command);
        }
        // A Subagent's Session is read, never prompted, so its view keeps its
        // own key table: Escape leaves for the parent instead of arming an
        // interrupt, the reading keys stay, and the composer's keys — text,
        // history, submission — reach nothing.
        if self.state.open_subagent_parent().is_some() {
            return command_for_subagent_view_event(event);
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
        self.state.open_session()
    }

    pub(super) fn session_reference(&self) -> Option<SessionReference> {
        self.state.session_reference.clone()
    }

    pub(super) fn outlook(&self) -> &Outlook {
        &self.state.outlook
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

    /// Whether anything on screen has live presentation, so the run loop
    /// ticks only while animation can be drawn and an idle TUI schedules zero
    /// wakeups.
    pub fn wants_spinner(&self) -> bool {
        // A Provider's Availability being read is live work like any other, and
        // the row showing it animates only while the tick is armed.
        self.state.settings_panel.is_reading(&self.state.settings)
            // So is another Session's Turn, drawn in a Sidebar row whose
            // Working duration has to be seen rising.
            || self.state.sidebar.shows_live_work()
            || self.state.session_animation_on_screen.get()
            || self.transcript_drag_scroll().is_some()
    }

    /// Advances presentation animation one frame. Called from the run loop's
    /// tick, which only exists while [`Self::wants_spinner`] holds.
    pub(super) fn advance_spinner(&mut self) {
        self.refresh_text_selection();
        if let Some((_, rows)) = self.transcript_drag_scroll()
            && let Some(press) = &self.state.left_press
        {
            self.update_text_selection_drag(press.pointer, rows);
        }
        self.state.promote_aged_commands();
        self.state.reconcile_command_mode();
        self.state.spinner_frame = self.state.spinner_frame.wrapping_add(1);
    }
}

/// The one reading of a Workspace directory: the canonical path, which is how
/// the server reads the Workspace it roots a Session at and the one it
/// narrows a listing by, so the client and the server never hold two
/// spellings of the same directory. A Workspace Suru cannot read that way is
/// not a reason to refuse to work in it, so the path stands as given where
/// canonicalizing fails.
fn workspace_reading(workspace: &Path) -> PathBuf {
    std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_owned())
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

impl TuiState {
    pub(super) fn pending_questionnaires(
        &self,
    ) -> impl Iterator<Item = &crate::protocol::Questionnaire> {
        self.session.as_ref().into_iter().flat_map(|session| {
            super::questionnaire::pending(session.snapshot()).filter(|q| {
                self.session_reference
                    .as_ref()
                    .is_some_and(|owner| self.questionnaires.available(owner, q.id))
            })
        })
    }

    pub(super) fn open_questionnaire(&self) -> Option<&crate::protocol::Questionnaire> {
        if !self.questionnaires.is_open(self.session_reference.as_ref()) {
            return None;
        }
        let id = self.questionnaires.id()?;
        self.session
            .as_ref()?
            .snapshot()
            .activities
            .iter()
            .find_map(|activity| match activity {
                Activity::Questionnaire {
                    questionnaire,
                    outcome,
                    ..
                } if questionnaire.id == id && outcome.is_live() => Some(questionnaire),
                _ => None,
            })
    }
}
