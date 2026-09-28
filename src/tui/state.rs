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
    managed_client::{
        ManagedEvent, RecoveryStatus, SessionEvent, SessionProjection, SubagentTreeEvent,
    },
    protocol::{
        Activity, ActivityId, ActivityStatus, AdmitPromptRequest, AgentSelection,
        AgentSelectionOperationId, ApprovalId, AttachmentId, CreateSessionRequest,
        EffectiveSettings, FoldPosture, InitialPrompt, MessageId, ModelCatalog, Outlook,
        PromptDelivery, PromptId, PromptStatus, QuestionnaireId, ResolveWorkspaceRequest,
        ServerIdentity, SessionChange, SessionErrorCode, SessionId, SessionListItem,
        SessionReference, SessionSnapshot, SettingMutation, SettingsSnapshot, ShutdownReason,
        SkillCatalog, SkillCatalogRequest, TextSelectionCopy, TurnId, TurnStatus,
        UpdateAgentSelectionRequest, Workspace, WorkspaceId,
    },
    provider::built_in_providers,
    settings::SettingChoiceSurface,
    terminal::{CellSize, GraphicsProtocol, TerminalFacts},
    theme::{Theme, ThemeCatalog},
};

use super::{
    approval_posture_picker::ApprovalPosturePicker,
    aside::{Aside, AsidePresentation, AsidePress},
    attachment_check::{AttachmentCheckId, AttachmentChecks},
    attachment_preview::{
        AttachmentPreviews, PreviewMode, PreviewScope, Thumbnail, ThumbnailRequest,
    },
    clipboard::{ClipboardRead, PasteId},
    clipboard_paste::ClipboardPastes,
    commands::{SemanticCommandId, SemanticInvocation, SemanticSubject},
    completion::{CompletionConfirmation, CompletionMode, ComposerCompletion},
    composer::{ComposerKey, ComposerMemory, SelectionMotion},
    connect_overlay::ConnectOverlay,
    icon_picker::{IconPicker, IconPickerTarget},
    keymap::{
        command_for_approval_posture_picker_event, command_for_aside_event,
        command_for_completion_event, command_for_connect_overlay_event,
        command_for_icon_picker_event, command_for_interrupt_confirmation_event,
        command_for_leader_event, command_for_model_options_event, command_for_model_picker_event,
        command_for_monitoring_subagent_view_event, command_for_numeric_editor_event,
        command_for_queued_prompt_event, command_for_serve_overlay_event,
        command_for_session_picker_event, command_for_settings_panel_event,
        command_for_sidebar_event, command_for_sidebar_menu_event,
        command_for_subagent_picker_event, command_for_subagent_view_event,
        command_for_subagent_view_leader_event, command_for_terminal_event,
        command_for_theme_picker_event, command_for_workspace_picker_event,
        command_for_workspace_picker_menu_event,
    },
    model_options::{ModelOptions, ReasoningCycle, cycle_reasoning_effort},
    model_picker::{ModelPicker, ModelPickerAction, ModelPickerPurpose},
    notice::{ApplicationNotice, AttachmentDemotion, Notice, PasteFailure},
    render::render_with_slots,
    selection::{
        SelectionCell, SelectionFrame, SelectionGranularity, SelectionSurface, TextSelection,
    },
    serve_overlay::ServeOverlay,
    session_picker::{SessionPicker, SessionPickerListing},
    settings_panel::{AvailabilityRead, SettingsPanel},
    side_column::ToggleStep,
    sidebar::{Sidebar, SidebarActivation, SidebarPress},
    slots::RenderSlots,
    subagent_picker::{SubagentPicker, working_subagents},
    text_binding::{BoundAttachment, attachment_name},
    theme_picker::ThemePicker,
    transcript::{
        FoldDisclosure, FoldStep, MessageStart, TranscriptCache, TranscriptDisclosure,
        TranscriptFolds, TranscriptGroups, TranscriptTurnFolds, TranscriptView, UnitKey, UnitStart,
    },
    workspace_picker::WorkspacePicker,
};

/// Rows scrolled per mouse wheel tick, matching common terminal conventions.
pub(super) const WHEEL_SCROLL_ROWS: usize = 3;
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
/// Click counts stop growing here, so a fourth rapid click reads as a third.
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

/// The wall clock a Session's own timestamps are read against, for a surface
/// ticking how long work has been running from when the Server said it began.
#[derive(Clone)]
struct SessionClock(Arc<dyn Fn() -> crate::protocol::SessionTimestamp + Send + Sync>);

impl SessionClock {
    fn now(&self) -> crate::protocol::SessionTimestamp {
        (self.0)()
    }
}

impl Default for SessionClock {
    fn default() -> Self {
        Self(Arc::new(crate::protocol::SessionTimestamp::now))
    }
}

impl std::fmt::Debug for SessionClock {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SessionClock(..)")
    }
}

/// How long a self-presented Intervention panel ignores every key, so a
/// reader already mid-keystroke cannot answer something they have not read.
const INTERVENTION_ARMING_DELAY: Duration = Duration::from_millis(250);

/// One Intervention of the open Session: a pending Approval awaiting a
/// Decision or a pending Questionnaire awaiting an Answer. The two are one
/// thing to the reader — something the Session owes them — so presentation
/// and dismissal name them together and in one Transcript order.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum InterventionId {
    Approval(ApprovalId),
    Questionnaire(QuestionnaireId),
}

/// An Intervention ready to present, carrying what its panel needs to open
/// on it, so the presentation step never looks the same thing up twice.
#[derive(Clone, Debug)]
enum PresentableIntervention {
    Approval(ApprovalId),
    Questionnaire(crate::protocol::Questionnaire),
}

/// Which surface above the Intervention panels owns the keys. One reading
/// serves both the key routing, which asks it what to dispatch to, and
/// self-presentation, which asks it whether anything stands in the way.
#[derive(Clone, Copy, Debug)]
enum SurfaceAboveInterventions {
    ApprovalPosture,
    Selection(SelectionSurface),
    Sidebar,
    Aside,
    /// Not a surface at all, but the same answer: the Session's own Origin has
    /// stopped answering, so its Interventions wait out of sight — a Decision
    /// taken now would go nowhere — and present themselves as soon as it
    /// answers again.
    UnreachableOrigin,
}

/// A run of cells on one row that a press can land on, as the frame that drew
/// it recorded them. A frame that did not draw the affordance records nothing,
/// so a stale reading can never answer a press.
#[derive(Clone, Debug)]
pub(super) struct PointableSpan {
    row: u16,
    columns: std::ops::Range<u16>,
}

impl PointableSpan {
    pub(super) fn new(row: u16, columns: std::ops::Range<u16>) -> Self {
        Self { row, columns }
    }

    fn contains(&self, position: Position) -> bool {
        self.row == position.y && self.columns.contains(&position.x)
    }
}

/// One Origin's Server recovering: the schedule its stream last reported, and
/// whether that loss has outlived its grace period and so is the reader's to
/// see. A loss inside the grace is held but never drawn, which is what keeps a
/// momentary drop from flickering across the frame.
#[derive(Clone, Copy, Debug)]
struct OriginRecovery {
    status: RecoveryStatus,
    presented: bool,
    /// Whether this loss has already had its grace period. One loss is served
    /// once, however many attempts its recovery goes on to make: without this
    /// every retry would ask for another wakeup, and a loss taken back off the
    /// frame — as a Fatal error takes the modal down — would be put straight
    /// back up by the next grace to come due. A fresh loss is a fresh entry,
    /// and is served afresh.
    served: bool,
}

#[derive(Clone, Copy, Debug, Default)]
enum OpeningLoadingState {
    #[default]
    Inactive,
    WaitingUntil(Instant),
    Visible,
}

#[derive(Clone, Debug, Eq)]
pub enum SessionListScope {
    CurrentWorkspace(Workspace),
    AllWorkspaces,
}

impl PartialEq for SessionListScope {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::CurrentWorkspace(left), Self::CurrentWorkspace(right)) => left.id == right.id,
            (Self::AllWorkspaces, Self::AllWorkspaces) => true,
            _ => false,
        }
    }
}

impl SessionListScope {
    pub(super) fn workspace_filter(&self) -> Option<&crate::protocol::WorkspaceId> {
        match self {
            Self::CurrentWorkspace(workspace) => Some(&workspace.id),
            Self::AllWorkspaces => None,
        }
    }

    /// Widens this scope to every Workspace, or narrows it back to the one
    /// the client runs in — the flip a reader makes when a listing scoped to
    /// where they stand is too narrow, or too wide, for what they are after.
    #[cfg(test)]
    pub(super) fn toggled(&self, current_workspace: &Workspace) -> Self {
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
    WorktreeList,
    WorktreeSelection,
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
struct RememberedExecutionContext {
    directory: Option<PathBuf>,
    status: crate::protocol::ExecutionDirectoryStatus,
}

/// What one Outlook's own Server last said about the Worktree the next Session
/// would work in there: the association it named, and the reading its
/// resolution carried for it.
#[derive(Clone, Debug)]
struct ResolvedCheckout {
    association: Option<crate::protocol::CheckoutAssociation>,
    reading: Option<crate::protocol::CheckoutSummary>,
}

#[derive(Clone, Debug)]
pub struct TuiState {
    hyperlinks: bool,
    left_press: Option<LeftPress>,
    last_click: Option<LastClick>,
    click_interval: Duration,
    pub(super) text_selection: Cell<Option<TextSelection>>,
    pub(super) selection_frames: RefCell<Vec<SelectionFrame>>,
    pub(super) selection_overlay_area: Cell<Option<ratatui::layout::Rect>>,
    pub(super) outlook: Outlook,
    outlook_workspaces: HashMap<Outlook, Workspace>,
    outlook_execution_directories: HashMap<Outlook, Option<PathBuf>>,
    remembered_execution_directories:
        HashMap<(Outlook, crate::protocol::WorkspaceId), RememberedExecutionContext>,
    pub(super) execution_status: crate::protocol::ExecutionDirectoryStatus,
    pub(super) new_worktree: Option<crate::protocol::PrepareCheckoutRequest>,
    pub(super) worktree_picker: super::worktree_picker::WorktreePicker,
    initial_context_resolved: bool,
    workspace_resolution_sequence: u64,
    pending_workspace_resolutions: HashMap<WorkspaceResolutionSurface, u64>,
    pub(super) identity: Option<ServerIdentity>,
    /// What each Origin's connection is presently recovering from, keyed by
    /// the Origin whose Server stopped answering. `Outlook::Local` is this
    /// machine's own Server, whose loss still stands the whole frame down
    /// (ADR 0002). A Remote key is the narrower thing the glossary calls
    /// Unreachable — see [`Self::is_unreachable`] — which blocks only what
    /// goes to that Remote and leaves the rest of the Client as live as it
    /// ever was.
    recovering: HashMap<Outlook, OriginRecovery>,
    /// Manual stop preserves the last confirmed identity as useful final context.
    pub(super) manually_stopped: bool,
    pub(super) fatal_error: Option<String>,
    pub(super) workspace: Workspace,
    pub(super) execution_directory: Option<PathBuf>,
    /// The Worktree the next Session would work in on each Outlook, as that
    /// Server named it when it resolved a context there, with the reading that
    /// resolution carried for it. The catalog's live reading stands in front of
    /// the resolution's; keeping the resolution's is what leaves the Landing's
    /// Checkout State never blank while the catalog is still on its way.
    ///
    /// It is held per Outlook because it answers for one Server: an Outlook
    /// with nothing remembered here has no Checkout State to present, and must
    /// never be given another Server's.
    outlook_execution_checkouts: HashMap<Outlook, ResolvedCheckout>,
    checkout_states:
        HashMap<(Outlook, crate::protocol::CheckoutId), crate::protocol::CheckoutSummary>,
    pub(super) composers: ComposerMemory,
    /// The pastes from the clipboard still waiting on a read or an upload,
    /// each bound for the draft it was asked for.
    clipboard_pastes: ClipboardPastes,
    /// The checks still waiting on whether the Attachments a history recall
    /// brought into a draft are stored, each bound for that draft.
    attachment_checks: AttachmentChecks,
    pub(super) approvals: super::approval::ApprovalPanel,
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
    /// Thumbnails of the open Session's Attachments, and what each frame
    /// wanted, reserved, and drew of them (ADR 0038).
    pub(super) attachment_previews: AttachmentPreviews,
    /// Bumped whenever the Session projection is replaced wholesale, so the
    /// transcript cache never trusts a revision across snapshot swaps.
    pub(super) transcript_generation: u64,
    /// Which Spinner frame is showing, advanced by the run loop's tick and
    /// read only at draw time — never by the transcript projection (ADR 0009).
    pub(super) spinner_frame: usize,
    pub(super) shimmer_clock: super::shimmer::Clock,
    pub(super) rail_origins:
        RefCell<HashMap<SessionReference, (crate::protocol::SessionTimestamp, usize)>>,
    /// Whether the last frame actually drew current-Session animation. A
    /// Working Indicator that scrolled away cannot justify 32ms redraws.
    pub(super) session_animation_on_screen: Cell<bool>,
    /// The Monitoring Session whose Watches the reader has confirmed stopping,
    /// with when that Monitoring began. The stop settles nothing itself — the
    /// Session stops Monitoring once the Watches settle — so the Working
    /// Indicator says the stop is under way until then. Keyed by the reading
    /// it was confirmed against, so Monitoring that ends and begins again
    /// offers the gesture afresh.
    watch_stop: Option<(SessionReference, crate::protocol::SessionTimestamp)>,
    /// When each visible Active Command first appeared to this client. Time
    /// stays out of transcript projection; the spinner tick reads these ages
    /// and writes Fold overrides only when a threshold is crossed.
    active_commands_started_at: HashMap<ActivityId, Instant>,
    presentation_clock: PresentationClock,
    session_clock: SessionClock,
    /// How long a self-presented Intervention panel ignores keys.
    intervention_arming_delay: Duration,
    /// When a self-presented Intervention panel starts taking keys. `None`
    /// once the guard has been spent or the reader asked for the panel.
    intervention_armed_until: Option<Instant>,
    /// The Interventions the reader put away with Esc, per Session. This is
    /// the Client's own reading, kept only while it runs: a later arrival is
    /// an id this never saw, so it presents itself.
    dismissed_interventions: HashMap<SessionReference, HashSet<InterventionId>>,
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
    /// This client's own claim on a Session it has asked for and not been
    /// answered about, present exactly while the Provisional Session is the
    /// main view. It stands in place of a route rather than beside one: the
    /// route is what a Server has confirmed, and nothing here has been.
    pub(super) provisional: Option<ProvisionalSession>,
    /// A refusal owed to the Landing rather than to whatever the reader has
    /// since opened. A creation the reader walked away from still answers, and
    /// its answer belongs where the draft it hands back is: both wait here
    /// until the Landing is next drawn.
    deferred_refusal: Option<DeferredRefusal>,
    /// Why the open route could not hydrate, if its newest attach failed.
    ///
    /// This is client presentation, not Session history: it never enters a
    /// snapshot, Activity, Turn, or protocol event. Keeping it beside the
    /// optimistic route lets that route retain its composer while the main
    /// content speaks the refusal in the Transcript's visual language.
    pub(super) opening_error: Option<String>,
    /// The origin-qualified reference of the projection above, and so `Some`
    /// exactly when it is.
    pub(super) session_reference: Option<SessionReference>,
    workspace_paths: HashMap<Outlook, crate::protocol::WorkspacePaths>,
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
    pending_submission: Option<PendingSubmission>,
    failed_submissions: HashMap<PromptId, FailedSubmission>,
    pending_steers: Vec<HeldPrompt>,
    /// The undelivered Prompts this client asked a Session to withdraw. Their
    /// text is this reader's to get back, which is why it is remembered here
    /// rather than derived from a Session every viewer reads the same.
    withdrawing: Vec<HeldPrompt>,
    pub(super) command_mode: CommandMode,
    pub(super) composer_completion: ComposerCompletion,
    skill_catalog: Option<(SkillCatalogRequest, SkillCatalog)>,
    pending_model_options: bool,
    pub(super) model_options: ModelOptions,
    pub(super) model_picker: ModelPicker,
    pub(super) approval_posture_picker: ApprovalPosturePicker,
    pub(super) theme_picker: ThemePicker,
    pub(super) session_picker: SessionPicker,
    pub(super) workspace_picker: WorkspacePicker,
    pub(super) subagent_picker: SubagentPicker,
    pub(super) icon_picker: IconPicker,
    /// Where the last frame drew the Session header's Icon span, so a press
    /// there can be resolved without the header re-deriving it: `None`
    /// whenever nothing was drawn there, including with Icons turned off, so
    /// a press over that blank space reaches nothing (issue #360).
    pub(super) header_icon_area: RefCell<Option<PointableSpan>>,
    /// Where the Unreachable banner drew its "Try again", so a press lands on
    /// the very command the Sidebar's `[unreachable]` row invokes.
    pub(super) unreachable_banner_area: RefCell<Option<PointableSpan>>,
    pub(super) connect_overlay: ConnectOverlay,
    pub(super) serve_overlay: ServeOverlay,
    pub(super) sidebar: Sidebar,
    /// The column on the far side of the main view from the Sidebar,
    /// answering for the open Session through its Sections.
    pub(super) aside: Aside,
    pub(super) settings_panel: SettingsPanel,
}

enum OutlookTurn {
    Deliberate,
    SessionRow {
        fallback_workspace: Workspace,
        fallback_execution_directory: PathBuf,
    },
}

#[derive(Clone, Debug)]
struct PendingSubmission {
    source: ComposerKey,
    target: SubmissionTarget,
    prompt: InitialPrompt,
    /// One spawned preparation request, distinct from stable Prompt and
    /// Preparation identities so an interrupted task cannot answer a retry.
    preparation_attempt: Option<uuid::Uuid>,
}

/// The Session view a client draws from the moment the Landing's first Prompt
/// is submitted until the Server answers with the Session it made.
///
/// Everything in it is this client's own knowledge: the Prompt as the user
/// Message it will become, the Title that Prompt gives, and a Working Indicator
/// with no elapsed time, because only the Server knows when Working began. It
/// is never listed in the Sidebar and leaves no highlight there, and the real
/// Session replaces it in place when it arrives.
#[derive(Clone, Debug)]
pub(super) struct ProvisionalSession {
    /// A local identity, held only so the Transcript projection and the render
    /// slots have the Session id they are shaped around. It never leaves this
    /// client and is never the created Session's own.
    pub(super) session_id: SessionId,
    pub(super) prompt: InitialPrompt,
    /// The Worktree prepared for this Session, once one has been. A retry uses
    /// it rather than preparing again: a claim never makes two.
    prepared: Option<PreparedFor>,
    pub(super) standing: ClaimStanding,
    pub(super) phase: ProvisionalSessionPhase,
}

/// Where a Provisional Session stands, which is one reading rather than two:
/// a claim cannot be both waiting on an answer and refused, and an interrupt
/// can only be owed by a claim that is still waiting.
#[derive(Clone, Debug)]
pub(super) enum ClaimStanding {
    /// Asked for and not answered — Working, as far as this client can say.
    /// `interrupt_intent` is a confirmed interrupt with nowhere to go yet: it
    /// waits here and travels with the Session's arrival.
    Claimed { interrupt_intent: bool },
    /// Refused by the Server, the reason standing where the Working Indicator
    /// was until the reader retries.
    Refused { error: String },
}

impl ProvisionalSession {
    fn refused(&self) -> bool {
        matches!(self.standing, ClaimStanding::Refused { .. })
    }

    /// Records a confirmed interrupt, and answers whether this claim had not
    /// already taken one — a second confirmation asks for nothing new.
    fn hold_interrupt(&mut self) -> bool {
        match &mut self.standing {
            ClaimStanding::Claimed { interrupt_intent } if !*interrupt_intent => {
                *interrupt_intent = true;
                true
            }
            _ => false,
        }
    }

    const fn interrupt_intent(&self) -> bool {
        matches!(
            self.standing,
            ClaimStanding::Claimed {
                interrupt_intent: true
            }
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ProvisionalSessionPhase {
    PreparingWorktree,
    CreatingSession,
}

/// A Worktree already made for a Session that has not been created yet.
#[derive(Clone, Debug)]
struct PreparedFor {
    id: crate::protocol::PreparationId,
    destination: crate::protocol::ExecutionDirectory,
    ready: bool,
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

/// A refused creation the reader had already walked away from, kept until the
/// Landing they wrote it in is drawn again.
///
/// Distinct from [`FailedSubmission`], which is a refusal waiting to be
/// reconciled against a Session that may yet report the Prompt: this one names
/// no Session, is answered by the Landing alone, and carries the words the
/// reader is owed.
#[derive(Clone, Debug)]
struct DeferredRefusal {
    error: String,
    prompt: InitialPrompt,
}

/// A Prompt one client is holding on a Session's behalf: a steer it has
/// admitted and drawn before the Session reports it, or one it has asked the
/// Session to withdraw and owes the reader back. Both are the same shape
/// because both are the same fact — this client, that Session, that Prompt —
/// read at different moments.
#[derive(Clone, Debug)]
struct HeldPrompt {
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
            hyperlinks: false,
            left_press: None,
            last_click: None,
            click_interval: CLICK_INTERVAL,
            text_selection: Cell::new(None),
            selection_frames: RefCell::new(Vec::new()),
            selection_overlay_area: Cell::new(None),
            outlook: Outlook::Local,
            outlook_workspaces: HashMap::from([(
                Outlook::Local,
                Workspace::directory(workspace.clone()),
            )]),
            outlook_execution_directories: HashMap::from([(
                Outlook::Local,
                Some(workspace.clone()),
            )]),
            remembered_execution_directories: HashMap::new(),
            execution_status: crate::protocol::ExecutionDirectoryStatus::Available,
            new_worktree: None,
            worktree_picker: Default::default(),
            initial_context_resolved: false,
            workspace_resolution_sequence: 0,
            pending_workspace_resolutions: HashMap::new(),
            identity: None,
            recovering: HashMap::new(),
            manually_stopped: false,
            fatal_error: None,
            workspace: Workspace::directory(workspace.clone()),
            execution_directory: Some(workspace.clone()),
            outlook_execution_checkouts: HashMap::new(),
            checkout_states: HashMap::new(),
            composers: ComposerMemory::default(),
            clipboard_pastes: ClipboardPastes::default(),
            attachment_checks: AttachmentChecks::default(),
            approvals: super::approval::ApprovalPanel::default(),
            questionnaires: super::questionnaire::QuestionnairePanels::default(),
            session_interactions: HashMap::new(),
            settings: EffectiveSettings::default(),
            settings_received: false,
            pinned_settings: Vec::new(),
            application_notice: ApplicationNotice::default(),
            transcript_cache: TranscriptCache::default(),
            attachment_previews: AttachmentPreviews::default(),
            transcript_generation: 0,
            spinner_frame: 0,
            shimmer_clock: super::shimmer::Clock::default(),
            rail_origins: RefCell::new(HashMap::new()),
            session_animation_on_screen: Cell::new(false),
            watch_stop: None,
            active_commands_started_at: HashMap::new(),
            presentation_clock: PresentationClock::default(),
            session_clock: SessionClock::default(),
            intervention_arming_delay: INTERVENTION_ARMING_DELAY,
            intervention_armed_until: None,
            dismissed_interventions: HashMap::new(),
            submission_error: None,
            route: None,
            opening_loading: OpeningLoadingState::Inactive,
            session: None,
            provisional: None,
            deferred_refusal: None,
            opening_error: None,
            session_reference: None,
            workspace_paths: HashMap::new(),
            outlook_landing_selections: HashMap::from([(Outlook::Local, None)]),
            landing_agent_selection: None,
            confirmed_landing_agent_selection: None,
            pending_landing_agent_selection: None,
            queued_landing_agent_selection: None,
            pending_agent_selection: None,
            queued_agent_selection: None,
            confirmed_agent_selection: None,
            session_events_blocked: false,
            pending_submission: None,
            failed_submissions: HashMap::new(),
            pending_steers: Vec::new(),
            withdrawing: Vec::new(),
            command_mode: CommandMode::Composer,
            composer_completion: ComposerCompletion::default(),
            skill_catalog: None,
            pending_model_options: false,
            model_options: ModelOptions::default(),
            model_picker: ModelPicker::default(),
            approval_posture_picker: ApprovalPosturePicker::default(),
            theme_picker: ThemePicker::default(),
            session_picker: SessionPicker::new(workspace.clone()),
            workspace_picker: WorkspacePicker::new(workspace.clone()),
            subagent_picker: SubagentPicker::default(),
            icon_picker: IconPicker::default(),
            header_icon_area: RefCell::new(None),
            unreachable_banner_area: RefCell::new(None),
            connect_overlay: ConnectOverlay::default(),
            serve_overlay: ServeOverlay::default(),
            sidebar: Sidebar::new(workspace),
            aside: Aside::new(),
            settings_panel: SettingsPanel::default(),
        }
    }

    fn cancel_worktree_intent(&mut self) {
        if self.new_worktree.take().is_some()
            && let Some(id) = self.pending_submission.as_ref().map(|p| p.prompt.id)
        {
            self.fail_pending_submission(
                id,
                "Worktree preparation left in its previous context; draft retained".to_owned(),
            );
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
    fn adopt_context(&mut self, context: crate::protocol::ResolvedWorkspace) {
        if self.workspace.id != context.workspace.id
            || self.execution_directory.as_ref()
                != context.execution_directory.as_ref().map(|d| &d.path)
        {
            self.cancel_worktree_intent();
        }

        self.execution_status = context.execution_status.clone();
        self.outlook_execution_checkouts.insert(
            self.outlook.clone(),
            ResolvedCheckout {
                reading: context.checkout.as_ref().and_then(|checkout| {
                    context
                        .checkouts
                        .iter()
                        .find(|reading| reading.association.id == checkout.id)
                        .cloned()
                }),
                association: context.checkout.clone(),
            },
        );
        self.remembered_execution_directories.insert(
            (self.outlook.clone(), context.workspace.id.clone()),
            RememberedExecutionContext {
                directory: context
                    .execution_directory
                    .as_ref()
                    .map(|directory| directory.path.clone()),
                status: context.execution_status.clone(),
            },
        );
        self.execution_directory = context.execution_directory.map(|directory| directory.path);
        self.workspace = context.workspace.clone();
        self.outlook_workspaces
            .insert(self.outlook.clone(), context.workspace.clone());
        self.outlook_execution_directories
            .insert(self.outlook.clone(), self.execution_directory.clone());
        self.session_picker
            .adopt_workspace(context.workspace.clone());
        self.workspace_picker
            .adopt_workspace(context.workspace.clone());
        self.sidebar.adopt_workspace(context.workspace);
        self.sidebar
            .adopt_execution_directory(self.execution_directory.clone());
        self.sync_composer_completion();
    }

    fn abandon_pending_attach(&mut self) {
        self.sidebar.abandon_attach();
        self.session_picker.abandon_attach();
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
        self.abandon_provisional_session();
        self.forget_awaited_withdrawals();
        self.text_selection.set(None);
        self.composers.clear_selections();
        self.left_press = None;
        self.session = None;
        self.session_reference = None;
        self.aside.note_opened(&target);
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
        self.abandon_provisional_session();
        self.forget_awaited_withdrawals();
        // A refusal owed to the Landing is said the first time the Landing is
        // drawn after it, rather than in whatever the reader was looking at,
        // and the draft it hands back is put there with it.
        if let Some(refusal) = self.deferred_refusal.take() {
            self.composers
                .admission_failed(ComposerKey::Landing, &refusal.prompt);
            self.submission_error = Some(refusal.error);
        }
        self.text_selection.set(None);
        self.composers.clear_selections();
        self.left_press = None;
        self.session = None;
        self.session_reference = None;
        self.opening_error = None;
        self.stop_opening_loading();
        self.route.take().is_some()
    }

    /// Gives up the withdrawals this client was owed. A Prompt comes back to
    /// the composer of the Session it was written in while the reader is
    /// standing in it; once they have left, the answer is stale and the draft
    /// they write next is theirs alone.
    fn forget_awaited_withdrawals(&mut self) {
        self.withdrawing.clear();
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
        self.begin_workspace_resolution(WorkspaceResolutionSurface::Outlook);
    }

    /// Turns toward the Origin of a Session already present in a merged
    /// listing, then carries the reader into its optimistic route. The row's
    /// Workspace is authoritative when this Client has not visited that
    /// Outlook before; unlike a deliberate Connect turn, no resolution is
    /// needed because the listing already came from that Origin.
    fn turn_outlook_for_session(
        &mut self,
        target: SessionReference,
        workspace: Workspace,
        execution_directory: PathBuf,
    ) {
        let remembers_workspace = self.outlook_workspaces.contains_key(&target.origin);
        self.turn_outlook_with(
            target.origin.clone(),
            OutlookTurn::SessionRow {
                fallback_workspace: workspace,
                fallback_execution_directory: execution_directory,
            },
        );
        if !remembers_workspace {
            self.outlook_workspaces
                .insert(target.origin.clone(), self.workspace.clone());
            self.outlook_execution_directories
                .insert(target.origin.clone(), self.execution_directory.clone());
        }
        self.open_session_route(target);
    }

    fn turn_outlook_with(&mut self, outlook: Outlook, turn: OutlookTurn) {
        if self.outlook != outlook {
            self.cancel_worktree_intent();
        }
        if self.outlook == outlook {
            return;
        }
        let (fallback_workspace, fallback_execution_directory, adopt_sidebar): (
            Workspace,
            PathBuf,
            fn(&mut Sidebar, Outlook),
        ) = match turn {
            OutlookTurn::Deliberate => (
                Workspace::directory(PathBuf::from(".")),
                PathBuf::from("."),
                Sidebar::adopt_outlook,
            ),
            OutlookTurn::SessionRow {
                fallback_workspace,
                fallback_execution_directory,
            } => (
                fallback_workspace,
                fallback_execution_directory,
                Sidebar::adopt_outlook_from_row,
            ),
        };
        self.outlook_landing_selections
            .insert(self.outlook.clone(), self.landing_agent_selection.clone());
        self.outlook = outlook.clone();
        self.pending_workspace_resolutions.clear();
        self.worktree_picker.close();
        self.workspace = self
            .outlook_workspaces
            .get(&outlook)
            .cloned()
            .unwrap_or(fallback_workspace);
        self.execution_status = self
            .remembered_execution_directories
            .get(&(outlook.clone(), self.workspace.id.clone()))
            .map_or(
                crate::protocol::ExecutionDirectoryStatus::Available,
                |remembered| remembered.status.clone(),
            );
        self.execution_directory = self
            .outlook_execution_directories
            .get(&outlook)
            .cloned()
            .unwrap_or(Some(fallback_execution_directory));
        self.leave_session_route();
        // After the route is left, which may hand the Landing back a Prompt
        // written for the Server just turned away from.
        self.leave_landing_attachments_behind();
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
        self.sidebar
            .adopt_execution_directory(self.execution_directory.clone());
        self.session_picker.adopt_outlook(outlook.clone());
        self.workspace_picker.adopt_outlook(outlook.clone());
        if let Some(paths) = self.workspace_paths.get(&outlook) {
            self.workspace_picker.adopt_workspace_paths(paths.clone());
        }
        adopt_sidebar(&mut self.sidebar, outlook);
        self.command_mode = CommandMode::Composer;
        self.submission_error = None;
        self.sync_composer_completion();
    }

    /// Remote catalog streams required by the current Outlook and independently
    /// scoped Session surfaces. Narrowing a listing must not release the Model
    /// Catalog of the Session on screen or another surface's interest.
    fn catalog_origins(&self) -> HashSet<Outlook> {
        let mut origins = self.sidebar.catalog_origins();
        origins.extend(self.session_picker.catalog_origins());
        if matches!(self.outlook, Outlook::Remote(_)) {
            origins.insert(self.outlook.clone());
        }
        origins
    }

    fn begin_workspace_resolution(&mut self, surface: WorkspaceResolutionSurface) -> u64 {
        self.workspace_resolution_sequence = self.workspace_resolution_sequence.wrapping_add(1);
        if !matches!(
            surface,
            WorkspaceResolutionSurface::Outlook | WorkspaceResolutionSurface::WorktreeList
        ) {
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
        // for nor the Execution Directory it is asked about, and the Landing it has
        // already left answers for neither. Skill resolution waits with every
        // other act that needs the Session.
        if self.open_session_is_loading() {
            return None;
        }
        let selection = self.agent_selection()?;
        let execution_directory = self
            .session
            .as_ref()
            .map(|session| session.snapshot().session.execution_directory.path.clone())
            .or_else(|| self.execution_directory.clone())?;
        let execution_directory = match self.outlook {
            Outlook::Local => workspace_reading(&execution_directory),
            Outlook::Remote(_) => execution_directory,
        };
        Some(SkillCatalogRequest {
            provider: selection.provider.clone(),
            execution_directory: crate::protocol::ExecutionDirectory {
                path: execution_directory,
            },
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

    /// Pastes `text` into `draft` as a bracketed paste into it would.
    fn paste_into_draft(&mut self, draft: ComposerKey, text: &str) {
        self.edit_draft_without_completion(draft, |composers, key| composers.insert(key, text));
    }

    /// Edits `draft` as the composer with the keys is edited, or, where the
    /// reader has since moved to another, quietly in its place.
    fn edit_draft_without_completion(
        &mut self,
        draft: ComposerKey,
        edit: impl FnOnce(&mut ComposerMemory, ComposerKey),
    ) {
        if draft == self.composer_key() {
            self.edit_composer_without_completion(edit);
        } else {
            edit(&mut self.composers, draft);
        }
    }

    /// Walks composer history, and asks whether any Attachment a Prompt
    /// recalled from it binds is still stored on the Server the draft goes
    /// to. The recalled draft stands bound meanwhile.
    fn restore_composer_history(
        &mut self,
        restore: impl FnOnce(&mut ComposerMemory, ComposerKey) -> Vec<BoundAttachment>,
    ) -> ApplicationTransition {
        let draft = self.composer_key();
        let mut recalled = Vec::new();
        self.edit_composer_without_completion(|composers, key| {
            recalled = restore(composers, key);
        });
        if recalled.is_empty() {
            return ApplicationTransition::Continue;
        }
        let origin = self.draft_origin(&draft);
        let (check, attachments) = self.attachment_checks.begin(draft, recalled);
        ApplicationTransition::CheckAttachments {
            check,
            origin,
            attachments,
        }
    }

    /// The Server a draft's Prompt goes to, and so the one its Attachments
    /// are uploaded to: its Session's Origin, or the Outlook's for the
    /// Landing.
    fn draft_origin(&self, draft: &ComposerKey) -> Outlook {
        match draft {
            ComposerKey::Session(session) => session.origin.clone(),
            ComposerKey::Landing => self.outlook.clone(),
        }
    }

    /// Demotes the labels of the Landing draft's Attachments to plain text,
    /// with a Notice naming them, once the Outlook has turned away from the
    /// Server they were pasted to: a Prompt from the Landing begins a Session
    /// on the new Outlook, which never stored them. Uploads still on their way
    /// there, and checks asked of the old Server, are abandoned with them.
    fn leave_landing_attachments_behind(&mut self) {
        let draft = ComposerKey::Landing;
        self.clipboard_pastes.abandon_uploads_into(&draft);
        self.attachment_checks.forget(&draft);
        let demoted = self.composers.demote_attachments(&draft, |_| true);
        self.report_demoted_attachments(&demoted, AttachmentDemotion::LeftBehind);
    }

    /// Says which Attachments a draft's labels no longer bind, and why: their
    /// ids to the Log, and their names in a Notice. A label is the reader's
    /// own words, so the Log never carries it.
    fn report_demoted_attachments(
        &mut self,
        demoted: &[BoundAttachment],
        demotion: AttachmentDemotion,
    ) {
        if demoted.is_empty() {
            return;
        }
        let ids = demoted
            .iter()
            .map(|bound| bound.attachment_id().to_string())
            .collect::<Vec<_>>();
        tracing::warn!(
            ?demotion,
            attachment_ids = ?ids,
            "a draft's Attachment labels were demoted to plain text"
        );
        let names = demoted
            .iter()
            .map(|bound| attachment_name(bound.label()))
            .collect::<Vec<_>>();
        self.application_notice
            .receive_demoted_attachments(&names, demotion);
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

    fn adopt_workspace_paths(&mut self, origin: &Outlook, event: &ManagedEvent) {
        if let ManagedEvent::SessionCatalogReconciled(snapshot) = event {
            if origin == &self.outlook {
                self.workspace_picker
                    .adopt_workspace_paths(snapshot.workspace_paths.clone());
            }
            self.workspace_paths
                .insert(origin.clone(), snapshot.workspace_paths.clone());
        }
    }

    fn adopt_checkout_state(&mut self, origin: &Outlook, event: &ManagedEvent) {
        match event {
            ManagedEvent::CheckoutStateChanged(changed) => {
                let reference = (origin.clone(), changed.checkout_id.clone());
                match &changed.checkout_state {
                    Some(checkout) => {
                        self.checkout_states.insert(reference, checkout.clone());
                    }
                    None => {
                        self.checkout_states.remove(&reference);
                    }
                }
            }
            ManagedEvent::SessionCatalogReconciled(snapshot) => {
                self.checkout_states
                    .retain(|(checkout_origin, _), _| checkout_origin != origin);
                self.checkout_states
                    .extend(snapshot.checkout_states.iter().map(|checkout| {
                        (
                            (origin.clone(), checkout.association.id.clone()),
                            checkout.clone(),
                        )
                    }));
            }
            _ => {}
        }
    }

    pub(super) fn checkout_state(
        &self,
        origin: &Outlook,
        checkout_id: &crate::protocol::CheckoutId,
    ) -> Option<&crate::protocol::CheckoutSummary> {
        self.checkout_states
            .get(&(origin.clone(), checkout_id.clone()))
    }

    /// The Checkout State of the Worktree the next Session would work in. The
    /// catalog's live reading answers wherever it has one, and the reading the
    /// owning Server's resolution carried answers until then.
    pub(super) fn execution_checkout_state(&self) -> Option<&crate::protocol::CheckoutSummary> {
        let resolved = self.outlook_execution_checkouts.get(&self.outlook)?;
        let association = resolved.association.as_ref()?;
        self.checkout_state(&self.outlook, &association.id)
            .or(resolved.reading.as_ref())
    }

    pub(super) fn execution_subdirectory(&self) -> Option<String> {
        let path = self.execution_directory.as_deref()?;
        let checkout = self.outlook_execution_checkouts.get(&self.outlook)?;
        let root = &checkout.association.as_ref()?.root;
        self.paths_for(&self.outlook)?.subdirectory(path, root)
    }

    /// The path facts a path of this Origin's is spelled by: the ones its own
    /// Server answered with, or — for the Client's own Server alone, which
    /// runs on this very machine — the ones this machine has. A Remote that
    /// has not yet answered has none, and its paths are left exactly as it
    /// spelled them rather than read with the Client's own syntax.
    fn paths_for(&self, origin: &Outlook) -> Option<crate::protocol::WorkspacePaths> {
        self.workspace_paths
            .get(origin)
            .cloned()
            .or_else(|| (origin == &Outlook::Local).then(crate::protocol::WorkspacePaths::default))
    }

    /// Where a Worktree stands, in the owning Server's own path syntax and as
    /// short as that Server's facts allow.
    pub(super) fn worktree_location(
        &self,
        origin: &Outlook,
        root: &Path,
        presented_root: Option<&Path>,
    ) -> String {
        self.paths_for(origin).map_or_else(
            || root.to_string_lossy().into_owned(),
            |paths| paths.worktree_location(root, presented_root),
        )
    }

    pub(super) fn workspace_name(&self, origin: &Outlook, path: &Path) -> String {
        self.paths_for(origin).map_or_else(
            || super::sidebar::workspace_name(path),
            |paths| paths.name(path),
        )
    }

    pub(super) fn workspace_label(&self, origin: &Outlook, path: &Path) -> String {
        self.paths_for(origin).map_or_else(
            || path.to_string_lossy().into_owned(),
            |paths| paths.label(path),
        )
    }

    pub fn apply(&mut self, event: ManagedEvent) {
        self.adopt_workspace_paths(&Outlook::Local, &event);
        self.adopt_checkout_state(&Outlook::Local, &event);
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
        if let ManagedEvent::ModelCatalog(catalog) = event {
            self.model_picker.adopt_catalog(outlook.clone(), catalog);
            return;
        }
        self.adopt_workspace_paths(outlook, &event);
        self.adopt_checkout_state(outlook, &event);
        self.reconcile_questionnaire_catalog(outlook, &event);
        // Reachability is the Origin's own, whether or not the Outlook is
        // turned toward it: a Remote that stops answering blocks what goes to
        // that Remote and nothing else. The two events that say nothing but
        // reachability are answered here in full, so the local Server's own
        // recovery arm can never be reached by a Remote's loss.
        if let ManagedEvent::Recovering(status) = &event {
            self.begin_recovery(outlook.clone(), *status);
            self.sidebar.mark_origin_recovering(outlook.clone());
            return;
        }
        if matches!(
            &event,
            ManagedEvent::RemoteRecovered | ManagedEvent::SessionCatalogReconciled(_)
        ) {
            self.end_recovery(outlook);
            self.sidebar.mark_origin_catalog_current(outlook);
        }
        // A bare recovery says nothing but reachability and is done with here;
        // a reconciled catalog says the rows too, and goes on below.
        if matches!(&event, ManagedEvent::RemoteRecovered) {
            return;
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
                    &changed.inputs.subagent_interventions,
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

    /// Drops one Origin's rows from the listings that range over Everywhere,
    /// which both a Remote that stopped answering and one whose Pairing ended
    /// leave behind.
    fn drop_origin_rows(&mut self, outlook: &Outlook) {
        // A Remote this Client can no longer reach at all is past recovering:
        // leaving it in hand would go on refusing everything bound for it long
        // after a new Pairing had made it reachable again.
        self.end_recovery(outlook);
        self.sidebar.end_origin(outlook);
        self.session_picker.end_origin(outlook);
    }

    /// Everything this Client held for a Remote it can no longer reach at all.
    /// Reaching it again takes a new Invite, so nothing remembered per Outlook
    /// survives its Pairing.
    fn end_pairing_memories(&mut self, outlook: &Outlook) {
        self.drop_origin_rows(outlook);
        self.outlook_workspaces.remove(outlook);
        self.outlook_execution_directories.remove(outlook);
        self.outlook_execution_checkouts.remove(outlook);
        self.outlook_landing_selections.remove(outlook);
        self.workspace_paths.remove(outlook);
        self.remembered_execution_directories
            .retain(|(origin, _), _| origin != outlook);
    }

    fn settle_remote_failure(&mut self, message: String) {
        let outlook = self.outlook.clone();
        self.end_recovery(&outlook);
        self.submission_error = Some(message);
    }

    /// Notes that one Origin's Server has stopped answering. A loss already
    /// being drawn goes on being drawn across the attempts that follow, so the
    /// grace period is served once per loss rather than once per retry.
    pub(super) fn begin_recovery(&mut self, outlook: Outlook, status: RecoveryStatus) {
        let (presented, served) = self
            .recovering
            .get(&outlook)
            .map_or((false, false), |held| (held.presented, held.served));
        self.recovering.insert(
            outlook,
            OriginRecovery {
                status,
                presented,
                served,
            },
        );
    }

    /// An Origin answers again. Reconnection is silent: what was drawn about
    /// the loss simply leaves.
    pub(super) fn end_recovery(&mut self, outlook: &Outlook) {
        self.recovering.remove(outlook);
    }

    /// One Origin's loss has outlived its grace and becomes the reader's to
    /// see. A loss already served is left exactly as it stands: its grace was
    /// spent once and cannot put back what something else has since taken off
    /// the frame.
    pub(super) fn present_recovery(&mut self, outlook: &Outlook) {
        if let Some(held) = self.recovering.get_mut(outlook)
            && !held.served
        {
            held.served = true;
            held.presented = true;
        }
    }

    /// Holds a loss back from the frame again while the recovery itself
    /// stands, which is what a Fatal error does: the modal comes down, the
    /// reading behind it does not.
    pub(super) fn withhold_recovery(&mut self, outlook: &Outlook) {
        if let Some(held) = self.recovering.get_mut(outlook) {
            held.presented = false;
        }
    }

    /// Every Origin whose loss is still waiting out its grace period — the
    /// only Origins a wakeup has anything left to do for. An Origin already
    /// served goes on recovering without asking for another.
    pub(super) fn origins_awaiting_grace(&self) -> std::collections::BTreeSet<Outlook> {
        self.recovering
            .iter()
            .filter(|(_, held)| !held.served)
            .map(|(outlook, _)| outlook.clone())
            .collect()
    }

    /// Whether this Origin is the thing the glossary calls Unreachable: a
    /// paired Remote that has stopped answering while its Pairing stands, so
    /// nothing sent to it goes anywhere. This machine's own Server is never
    /// Unreachable — its loss is the whole-frame modal's to say — so it is
    /// deliberately not part of this answer.
    pub(super) fn is_unreachable(&self, outlook: &Outlook) -> bool {
        outlook.remote_name().is_some() && self.recovering.contains_key(outlook)
    }

    /// The loss an Origin is presenting: one that has outlived its grace and
    /// so is the reader's to see.
    pub(super) fn presented_recovery(&self, outlook: &Outlook) -> Option<RecoveryStatus> {
        self.recovering
            .get(outlook)
            .filter(|held| held.presented)
            .map(|held| held.status)
    }

    /// The Remote the Outlook is turned toward while it is Unreachable, and
    /// the schedule its recovery is on — what the banner above the composer is
    /// drawn from. Nothing is answered for a Remote the Outlook has left.
    pub(super) fn unreachable_remote(&self) -> Option<(&str, RecoveryStatus)> {
        let status = self.presented_recovery(&self.outlook)?;
        Some((self.outlook.remote_name()?, status))
    }

    /// Whether the whole-frame modal stands. Only this machine's own Server
    /// going away raises it; a Remote that stops answering is scoped to itself.
    pub(super) fn reconnect_overlay_visible(&self) -> bool {
        self.presented_recovery(&Outlook::Local).is_some()
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
                self.end_recovery(&Outlook::Local);
                self.manually_stopped = false;
                self.fatal_error = None;
            }
            ManagedEvent::Connected(health) => {
                if self.outlook == Outlook::Local {
                    self.workspace_picker
                        .adopt_workspace_paths(health.workspace_paths.clone());
                }
                self.workspace_paths
                    .insert(Outlook::Local, health.workspace_paths.clone());
                self.outlook_landing_selections
                    .insert(Outlook::Local, health.landing_agent_selection.clone());
                let replaced_server = self
                    .identity
                    .as_ref()
                    .is_some_and(|identity| identity.instance_id != health.instance_id);
                if replaced_server {
                    if self.new_worktree.is_some()
                        && let Some(prompt_id) = self
                            .pending_submission
                            .as_ref()
                            .map(|pending| pending.prompt.id)
                    {
                        self.fail_pending_submission(prompt_id, "Worktree preparation interrupted by Server replacement; submit again to resume the same preparation".into());
                    }
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
                    self.submission_error = Some(if self.new_worktree.is_some() {
                        "Worktree preparation interrupted by Server replacement; submit again to resume the same preparation".to_owned()
                    } else {
                        "Session ended because the shared server was replaced".to_owned()
                    });
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
                self.end_recovery(&Outlook::Local);
                self.manually_stopped = false;
                self.fatal_error = None;
            }
            ManagedEvent::SettingsSnapshot(snapshot) => self.adopt_settings(snapshot),
            // The lifecycle stream is the local server's, so the catalog it
            // pushes is the Local Outlook's whichever Outlook is on screen.
            ManagedEvent::ModelCatalog(catalog) => {
                self.model_picker.adopt_catalog(Outlook::Local, catalog);
            }
            ManagedEvent::SkillCatalogUpdated(catalog) => {
                let request = SkillCatalogRequest {
                    provider: catalog.provider.clone(),
                    execution_directory: catalog.execution_directory.clone(),
                };
                self.load_skill_catalog(request, catalog);
            }
            // The lifecycle stream belongs to this machine's own Server, so a
            // recovery it reports is the local one whichever Outlook is on
            // screen. A Remote's own loss arrives Origin-stamped instead, and
            // is taken in `apply_origin_catalog`.
            ManagedEvent::Recovering(status) => {
                self.begin_recovery(Outlook::Local, status);
                self.manually_stopped = false;
                self.fatal_error = None;
            }
            ManagedEvent::RemoteRecovered => self.end_recovery(&Outlook::Local),
            ManagedEvent::RemoteFailed { message, .. } => self.settle_remote_failure(message),
            ManagedEvent::ServerShutdown(shutdown) => {
                self.stop_opening_loading();
                if shutdown.reason == ShutdownReason::Manual {
                    self.end_recovery(&Outlook::Local);
                    self.manually_stopped = true;
                    self.fatal_error = None;
                }
            }
            // A creation says an id and nothing a row is drawn from, so there
            // is nothing to take in place: the catch-up above is the whole of
            // the Sidebar's answer to it.
            ManagedEvent::SessionCatalogInvalidated { .. }
            | ManagedEvent::CheckoutStateChanged(_)
            | ManagedEvent::SessionCreated(_) => {}
            ManagedEvent::SessionDeleted(deleted) => {
                self.session_picker.remove(deleted.session_id);
                self.remove_deleted_session(deleted.session_id);
            }
            ManagedEvent::SessionTitleChanged(retitled) => {
                self.session_picker.retitle(
                    retitled.session_id,
                    retitled.title.clone(),
                    retitled.icon.clone(),
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
            ManagedEvent::SessionWorkingChanged(_) | ManagedEvent::SessionMonitoringChanged(_) => {}
            // Neither listing surface states a Session's total yet — the
            // footer of the Session in view reads its own — so the roll-up
            // the catalog announces moves nothing this client draws.
            ManagedEvent::SessionUsageChanged(_) => {}
            ManagedEvent::WorkspaceIconChanged(changed) => {
                // The Sidebar already took this in `apply_sidebar_catalog_event`,
                // called ahead of this match. What is left is the Landing's own
                // current Workspace and the two pickers.
                if self.workspace.id == changed.workspace_id {
                    self.workspace.icon = changed.icon.clone();
                }
                self.session_picker.set_workspace_icon_origin(
                    self.outlook.clone(),
                    &changed.workspace_id,
                    changed.icon.clone(),
                );
                self.workspace_picker.set_workspace_icon_origin(
                    self.outlook.clone(),
                    &changed.workspace_id,
                    changed.icon,
                );
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
                self.withhold_recovery(&Outlook::Local);
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
            | ManagedEvent::CheckoutStateChanged(_)
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
                retitled.icon.clone(),
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
            ManagedEvent::SessionMonitoringChanged(monitoring) => {
                self.sidebar.set_monitoring_origin(
                    outlook.clone(),
                    monitoring.session_id,
                    monitoring.monitoring_since,
                )
            }
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
            ManagedEvent::WorkspaceIconChanged(changed) => self.sidebar.set_workspace_icon_origin(
                outlook.clone(),
                &changed.workspace_id,
                changed.icon.clone(),
            ),
            ManagedEvent::Connecting
            | ManagedEvent::Connected(_)
            | ManagedEvent::SettingsSnapshot(_)
            | ManagedEvent::ModelCatalog(_)
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
            | ManagedEvent::CheckoutStateChanged(_)
            | ManagedEvent::SessionCreated(_)
            | ManagedEvent::SessionWorkingChanged(_)
            | ManagedEvent::SessionMonitoringChanged(_)
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
                retitled.icon.clone(),
            ),
            ManagedEvent::SessionSettlementChanged(settled) => self.session_picker.settle_origin(
                outlook.clone(),
                settled.session_id,
                settled.settled_at,
            ),
            ManagedEvent::SessionCatalogReconciled(snapshot) => self
                .session_picker
                .retain_origin_catalog(outlook.clone(), &snapshot.session_ids),
            ManagedEvent::WorkspaceIconChanged(changed) => {
                self.session_picker.set_workspace_icon_origin(
                    outlook.clone(),
                    &changed.workspace_id,
                    changed.icon.clone(),
                );
                self.workspace_picker.set_workspace_icon_origin(
                    outlook.clone(),
                    &changed.workspace_id,
                    changed.icon.clone(),
                );
            }
            ManagedEvent::Connecting
            | ManagedEvent::Connected(_)
            | ManagedEvent::SettingsSnapshot(_)
            | ManagedEvent::ModelCatalog(_)
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
        // The Settings the Sidebar draws under are the ones that act on
        // arrival rather than on the next view opened, because the frames it
        // governs may be on screen already.
        self.sidebar.adopt_settings(&self.settings);
        self.aside.adopt_settings(&self.settings);
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
        self.reconcile_withdrawn_prompts();
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

    /// Lets go of a creation the reader walked away from before it was
    /// answered: the Session is theirs and reaches the Sidebar through the
    /// catalog, but the submission that made it settles where its draft stands
    /// rather than under a Session they are not in.
    fn detach_creation(&mut self, snapshot: &SessionSnapshot) -> bool {
        let settles = self.pending_submission.as_ref().is_some_and(|pending| {
            pending.target == SubmissionTarget::CreateSession
                && snapshot
                    .prompts
                    .iter()
                    .any(|prompt| prompt.id == pending.prompt.id)
        });
        if !settles {
            return false;
        }
        self.new_worktree = None;
        let pending = self
            .pending_submission
            .take()
            .expect("the settling submission was just observed");
        self.composers.admission_reconciled(
            pending.source.clone(),
            pending.source,
            &pending.prompt,
        );
        true
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
        // Any claim this client was drawing is answered by a Session arriving,
        // whether or not it is the one the claim was for.
        self.release_claim();
        self.text_selection.set(None);
        self.composers.clear_selections();
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
        self.aside.note_opened(&reference);
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

    fn has_text_selection(&self) -> bool {
        self.text_selection.get().is_some()
            || self
                .composers
                .selection_range(self.composer_key())
                .is_some()
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

    /// Whether the working Subagent `subagent` may be stopped on its own from
    /// its Subagent Picker row. A brokered one always may: Suru runs it on a
    /// Provider actor of its own (ADR 0035). A native one may where the open
    /// Session's Provider offers stopping one of its Subagents on its own —
    /// read off the Session's own settled Selection, because the native
    /// Subagents on offer run under it whatever Selection edit may be pending.
    pub(super) fn subagent_stop_offered(&self, subagent: SessionId) -> bool {
        let Some(snapshot) = self.session.as_ref().map(|session| session.snapshot()) else {
            return false;
        };
        let brokered = working_subagents(snapshot)
            .iter()
            .any(|working| working.session_id == subagent && working.brokered);
        brokered
            || snapshot
                .session
                .agent_selection
                .as_ref()
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
        if projection.snapshot().session.is_subagent() {
            return projection
                .snapshot()
                .turns
                .last()
                .and_then(|turn| turn.agent.as_ref())
                .map(|agent| &agent.selection);
        }
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
        let provisional = self.provisional_prompts(snapshot.session.id);
        Some(self.transcript_cache.view_with_hyperlinks(
            self.transcript_generation,
            snapshot,
            &provisional.iter().collect::<Vec<_>>(),
            TranscriptDisclosure {
                folds: &interaction.folds.borrow(),
                groups: &interaction.groups.borrow(),
                turns: &interaction.turns.borrow(),
                reasoning_visibility: self.settings().transcript.reasoning_visibility,
            },
            &self.attachment_previews,
            theme,
            width,
            self.hyperlinks,
        ))
    }

    /// Answers one step of the wheel where the pointer stood: the Sidebar
    /// takes every step over its own column, and the Transcript every other.
    fn wheel_at(&mut self, position: Position, direction: ScrollDirection) {
        // Over the Aside the wheel moves its Sections, and never the
        // Transcript beside them.
        if self.aside_is_present()
            && self.aside.wheel_at(
                position,
                direction == ScrollDirection::Down,
                WHEEL_SCROLL_ROWS,
            )
        {
            return;
        }
        if !self
            .sidebar
            .wheel_at(position, direction, WHEEL_SCROLL_ROWS)
        {
            self.navigate_transcript_lines(direction);
        }
    }

    fn navigate_transcript_page(&mut self, direction: ScrollDirection) {
        self.navigate_transcript(direction, None);
    }

    fn navigate_transcript_lines(&mut self, direction: ScrollDirection) {
        self.navigate_transcript(direction, Some(WHEEL_SCROLL_ROWS));
    }

    fn navigate_transcript(&mut self, direction: ScrollDirection, rows: Option<usize>) {
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
            ScrollDirection::Up => viewport.scroll_position.saturating_sub(step),
            ScrollDirection::Down => viewport
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
                        crate::protocol::Activity::Approval { id, approval, .. }
                            if *id == activity_id
                                && self.pending_approvals().any(|pending| {
                                    matches!(pending, Activity::Approval { approval: candidate, .. } if candidate.id == approval.id)
                                }) =>
                        {
                            Some(approval.id)
                        }
                        _ => None,
                    })
            })
        {
            return Some(SemanticCommandId::ApprovalOpen.on_approval(id));
        }
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
            UnitKey::Subagent { session_id, .. } => {
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

    /// Whether the composer has the keys. The Sidebar and the Aside are the
    /// surfaces that take them without opening over the composer, so a
    /// composer that has them is simply one neither column is driving.
    pub(super) fn composer_focused(&self) -> bool {
        !self.sidebar.has_focus() && !self.aside.has_focus()
    }

    /// The Session the main view has open, and `None` on the Landing. It is
    /// the route the reader is looking at rather than a question about what
    /// has loaded, which is why the Sidebar's open highlight is derived from
    /// it and from nothing else.
    pub(super) fn open_session(&self) -> Option<SessionId> {
        self.route.as_ref().map(|route| route.session_id)
    }

    /// The moment now on the clock a Session's timestamps are read against.
    pub(super) fn session_now(&self) -> crate::protocol::SessionTimestamp {
        self.session_clock.now()
    }

    pub(super) fn open_session_reference(&self) -> Option<&SessionReference> {
        self.route.as_ref()
    }

    /// The Session the Sidebar's open-Session highlight and its starting row
    /// focus answer for: the open Session, or — while a Subagent's Session is
    /// open, at any depth — the top-level Session heading its tree, since that
    /// tree is still what the main view is for. The top-level Session is read
    /// off the per-tree subscription's snapshot, so until that arrives a
    /// Subagent's Session answers for itself and no row is highlighted.
    /// `None` on the Landing.
    ///
    /// Opening a row is not answered from here: from a Subagent's Session the
    /// top-level row is another Session, and choosing it opens it.
    pub(super) fn sidebar_highlight(&self) -> Option<SessionReference> {
        let open = self.route.as_ref()?;
        Some(
            self.aside
                .top_level_of(open)
                .unwrap_or_else(|| open.clone()),
        )
    }

    /// Whether activating the open Sidebar row means retry rather than merely
    /// handing the keys back. Only a failed optimistic shell has that meaning:
    /// an attach still in flight must not be duplicated, and a hydrated
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
    /// The presentation clock's reading of now.
    pub(super) fn presentation_now(&self) -> Instant {
        self.presentation_clock.now()
    }

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

    /// Whether the Aside is the surface the keys actually reach: its own
    /// claim, drawn, and outranked by nothing opened over the main view. It
    /// ranks beside the Sidebar, and the two never claim the keys at once.
    pub(super) fn aside_owns_input(&self) -> bool {
        self.aside.has_focus() && !self.overlay_owns_input()
    }

    /// Whether the Aside stands in this frame at all: only beside an open
    /// Session, so it is absent on the Landing.
    pub(super) fn aside_is_present(&self) -> bool {
        self.aside_subject().is_some()
    }

    /// The Session the Aside answers for: the open Session, or the
    /// Provisional Session from the moment the Landing's first Prompt is
    /// submitted, so the layout changes once, at submission. `None` on the
    /// Landing.
    pub(super) fn aside_subject(&self) -> Option<SessionReference> {
        self.route.clone().or_else(|| self.provisional_reference())
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
        if self.icon_picker.is_open() {
            Some(SelectionSurface::Icons)
        } else if self.connect_overlay.is_open() {
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
        } else if self.worktree_picker.open {
            Some(SelectionSurface::Worktrees)
        } else if self.workspace_picker.menu_is_open() {
            Some(SelectionSurface::WorkspacePickerMenu)
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
        if self.reconnect_overlay_visible() {
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
        self.icon_picker.is_open()
            || self.theme_picker.is_open()
            || self.model_picker.is_open()
            || self.connect_overlay.is_open()
            || self.serve_overlay.is_open()
            || self.settings_panel.is_open()
            || self.model_options.is_open()
            || self.session_picker.is_open()
            || self.workspace_picker.is_open()
            || self.worktree_picker.open
            || self.subagent_picker.is_open()
            || self.approvals.is_open(self.session_reference.as_ref())
            || self.questionnaires.is_open(self.session_reference.as_ref())
    }

    pub(super) fn composer_border_style(&self, theme: &Theme) -> Style {
        if self.composers.skill_issue(self.composer_key()).is_some() {
            theme.form_field.invalid
        } else {
            theme.accent.primary
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
        let created_session = pending.target == SubmissionTarget::CreateSession;
        // A Provisional Session stands for a Server's answer about a Session.
        // Anything else that stops a submission on its way — a Worktree that
        // could not be prepared, Skills to reselect in a destination — is about
        // the draft, so the reader goes back to the Landing holding it rather
        // than standing in a view whose composer means retry.
        if self
            .provisional
            .as_ref()
            .is_some_and(|claim| claim.prompt.id == prompt_id)
        {
            self.abandon_provisional_session();
        }
        self.composers
            .admission_failed(pending.source.clone(), &pending.prompt);
        self.failed_submissions.insert(
            pending.prompt.id,
            FailedSubmission {
                source: pending.source,
                target: pending.target,
                prompt: pending.prompt.clone(),
            },
        );
        // A creation the reader walked away from answers to the Landing, where
        // the draft it just restored stands, rather than to whatever they
        // opened in the meantime.
        if created_session && self.route.is_some() {
            self.deferred_refusal = Some(DeferredRefusal {
                error,
                prompt: pending.prompt,
            });
            return;
        }
        self.submission_error = Some(error);
    }

    /// Takes the Server's refusal of a Session this client asked for.
    ///
    /// A claim still standing for it keeps the view: the user Message stays, the
    /// Working Indicator gives way to the refusal. Ordinary failures leave an
    /// empty composer whose Enter retries the Prompt; a destination Skill
    /// mismatch restores that Prompt for editing unless the reader has already
    /// typed a newer replacement. A reader who has left is answered at the
    /// Landing instead, with the draft the refusal hands back.
    fn refuse_creation(&mut self, prompt_id: PromptId, error: String, restore_for_editing: bool) {
        let refuses_claim = self
            .provisional
            .as_ref()
            .is_some_and(|claim| claim.prompt.id == prompt_id && !claim.refused())
            && self
                .pending_submission
                .as_ref()
                .is_some_and(|pending| pending.prompt.id == prompt_id);
        if !refuses_claim {
            self.fail_pending_submission(prompt_id, error);
            return;
        }
        let prompt = self
            .pending_submission
            .as_ref()
            .expect("the refused claim has its pending submission")
            .prompt
            .clone();
        self.pending_submission = None;
        if restore_for_editing && self.composers.is_empty(ComposerKey::Landing) {
            self.composers
                .admission_failed(ComposerKey::Landing, &prompt);
        }
        self.provisional
            .as_mut()
            .expect("the refused claim was just observed")
            .standing = ClaimStanding::Refused { error };
        // Refused, the claim is no longer Working as far as this client can
        // say, so its Aside entry gives up the Working Marker.
        if let Some(reference) = self.provisional_reference() {
            self.aside
                .stand_in_for(reference, prompt.text.trim().to_owned(), false);
        }
        self.transcript_generation = self.transcript_generation.wrapping_add(1);
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

    /// The Prompts this Session draws in the Transcript's position as the user
    /// Messages they will become.
    ///
    /// A Prompt admitted to begin a Turn and not yet delivered is one of them
    /// for every client, read off the Session itself, so no reader waits on the
    /// Agent to see what was asked. The client that submitted one draws it from
    /// its own hand first, before the Server has confirmed anything; the two
    /// readings are the same Prompt, so the identity decides and never the
    /// source.
    pub(super) fn provisional_prompts(&self, session_id: SessionId) -> Vec<InitialPrompt> {
        let session = SessionReference::new(self.outlook.clone(), session_id);
        let mut prompts = self
            .pending_steers
            .iter()
            .filter(|steer| steer.session == session)
            .map(|steer| steer.prompt.clone())
            .collect::<Vec<_>>();
        if let Some(pending) = self.pending_submission.as_ref().filter(|pending| {
            pending.target == SubmissionTarget::AdmitPrompt(session.clone(), PromptDelivery::Steer)
        }) {
            prompts.push(pending.prompt.clone());
        }
        let admitted = self
            .session
            .as_ref()
            .filter(|projection| projection.session_id() == session_id)
            .into_iter()
            .flat_map(|projection| &projection.snapshot().prompts)
            .filter(|prompt| {
                prompt.status == PromptStatus::Pending && prompt.delivery == PromptDelivery::Steer
            })
            .filter(|prompt| !prompts.iter().any(|drawn| drawn.id == prompt.id))
            .filter(|prompt| {
                self.session.as_ref().is_none_or(|projection| {
                    !projection
                        .snapshot()
                        .turns
                        .iter()
                        .any(|turn| turn.prompt_id == Some(prompt.id))
                })
            })
            .map(|prompt| InitialPrompt {
                id: prompt.id,
                text: prompt.text.clone(),
                skill_invocations: prompt.skill_invocations.clone(),
                attachments: prompt.attachments.clone(),
            })
            .collect::<Vec<_>>();
        prompts.extend(admitted);
        prompts
    }

    /// Carries the reader into their own claim on the Session they have just
    /// asked for, before the Server has answered anything about it.
    ///
    /// The claim stands in the route's place rather than beside it, so nothing
    /// that reads the route — the Sidebar's highlight, the composer the keys
    /// write into, what a Prompt would be delivered to — takes the Provisional
    /// Session for a Session that exists. The draft stays under the Landing's
    /// own composer, which is where the migration onto the created Session's
    /// key reads it from.
    fn begin_provisional_session(&mut self, prompt: InitialPrompt) -> ApplicationTransition {
        self.text_selection.set(None);
        self.command_mode = CommandMode::Composer;
        self.submission_error = None;
        let session_id = SessionId::new();
        let phase = if self.new_worktree.is_some() {
            ProvisionalSessionPhase::PreparingWorktree
        } else {
            ProvisionalSessionPhase::CreatingSession
        };
        self.provisional = Some(ProvisionalSession {
            session_id,
            prompt: prompt.clone(),
            prepared: None,
            phase,
            standing: ClaimStanding::Claimed {
                interrupt_intent: false,
            },
        });
        // The claim is drawn by the Session view's own body, which reads view
        // state — where the Transcript is scrolled, what is folded — from an
        // interaction. It gets one of its own under its local identity, given
        // up with the claim so nothing outlives what it was for.
        let reference = SessionReference::new(self.outlook.clone(), session_id);
        self.ensure_interaction(reference.clone());
        // The Aside answers for the claim at once, from what it says.
        self.aside
            .stand_in_for(reference.clone(), prompt.text.trim().to_owned(), true);
        // The draft's thumbnails are the claim's own Prompt's.
        self.attachment_previews
            .carry_to(PreviewScope::Session(reference));
        self.transcript_generation = self.transcript_generation.wrapping_add(1);
        self.creation_transition(prompt)
    }

    /// The origin-qualified reference a standing claim is keyed by. It names
    /// nothing on any Server: the identity is this client's own.
    fn provisional_reference(&self) -> Option<SessionReference> {
        Some(SessionReference::new(
            self.outlook.clone(),
            self.provisional.as_ref()?.session_id,
        ))
    }

    /// Every Attachment bound where a strip of thumbnails may show it: by the
    /// open Session's Prompts and Messages, by the Prompts this client sent it
    /// that it has not echoed yet, by the Provisional Session's Prompt, and by
    /// the draft in view.
    fn bound_attachments(&self) -> HashSet<crate::protocol::AttachmentId> {
        let mut bound = HashSet::new();
        if let Some(session) = &self.session {
            let snapshot = session.snapshot();
            let prompts = snapshot
                .prompts
                .iter()
                .flat_map(|prompt| &prompt.attachments);
            let messages = snapshot
                .messages
                .iter()
                .flat_map(|message| &message.attachments);
            bound.extend(
                prompts
                    .chain(messages)
                    .map(|binding| binding.attachment_id.clone()),
            );
            bound.extend(
                self.provisional_prompts(snapshot.session.id)
                    .into_iter()
                    .flat_map(|prompt| prompt.attachments)
                    .map(|binding| binding.attachment_id),
            );
        }
        if let Some(provisional) = &self.provisional {
            bound.extend(
                provisional
                    .prompt
                    .attachments
                    .iter()
                    .map(|binding| binding.attachment_id.clone()),
            );
        }
        bound.extend(self.composers.bound_attachments(&self.composer_key()));
        bound
    }

    /// Whether the Worktree the Landing intends has already been made for the
    /// Session being created. The reader is standing in it, so the intent has
    /// nothing left to say about what a submit would do.
    pub(super) fn intent_already_prepared(&self) -> bool {
        self.provisional
            .as_ref()
            .and_then(|claim| claim.prepared.as_ref())
            .is_some_and(|prepared| {
                self.execution_directory.as_ref() == Some(&prepared.destination.path)
            })
    }

    pub(super) fn provisional_interaction(&self) -> Option<&SessionInteraction> {
        self.session_interaction(&self.provisional_reference()?)
    }

    /// Asks for the Session this Prompt is written for: the Worktree already
    /// prepared for it, a new one where that intent stands, or the Execution
    /// Directory the reader is working in.
    ///
    /// Both the first submission and every retry come through here, so a retry
    /// is asked under the context the reader is in rather than one they have
    /// left, and a Worktree already made for this Prompt is never made twice.
    fn creation_transition(&mut self, prompt: InitialPrompt) -> ApplicationTransition {
        // The Worktree this claim's Prompt was prepared for is where it is
        // asked for again, wherever the reader has moved since: a retry is the
        // same request, and a claim that prepared a Worktree never prepares a
        // second one to strand the first.
        let prepared = self
            .provisional
            .as_ref()
            .and_then(|claim| claim.prepared.clone())
            .filter(|prepared| prepared.ready);
        if let Some(prepared) = prepared {
            return ApplicationTransition::CreateSession(CreateSessionRequest {
                preparation_id: Some(prepared.id),
                agent_selection: self.landing_agent_selection.clone(),
                execution_directory: prepared.destination,
                prompt,
            });
        }
        if let Some(request) = &mut self.new_worktree {
            // The owning Server names the Worktree from the Prompt as bound,
            // leaving out its Skill markers and Attachment labels itself.
            request.prompt = crate::protocol::PreparationPrompt {
                text: prompt.text.clone(),
                skill_invocations: prompt.skill_invocations.clone(),
                attachments: prompt.attachments.clone(),
            };
            if let Some(selection) = &self.landing_agent_selection {
                request.provider = selection.provider.clone();
            }
            let attempt_id = uuid::Uuid::new_v4();
            if let Some(pending) = self.pending_submission.as_mut() {
                pending.preparation_attempt = Some(attempt_id);
            }
            return ApplicationTransition::PrepareCheckout {
                attempt_id,
                prompt_id: prompt.id,
                request: request.clone(),
            };
        }
        let Some(path) = self.execution_directory.clone() else {
            self.submission_error =
                Some("Choose a working copy before starting a Session".to_owned());
            return ApplicationTransition::Continue;
        };
        ApplicationTransition::CreateSession(CreateSessionRequest {
            preparation_id: None,
            agent_selection: self.landing_agent_selection.clone(),
            execution_directory: crate::protocol::ExecutionDirectory { path },
            prompt,
        })
    }

    /// Gives up a Provisional Session, which a newer route does without pulling
    /// the client back: the creation goes on, and its answer no longer decides
    /// where the reader is.
    fn abandon_provisional_session(&mut self) -> Option<ProvisionalSession> {
        let abandoned = self.release_claim()?;
        // A refused Prompt is the reader's again: its text goes back to the
        // Landing as a draft, to be resubmitted as the very Prompt it was.
        if abandoned.refused() {
            self.composers
                .admission_failed(ComposerKey::Landing, &abandoned.prompt);
        }
        Some(abandoned)
    }

    /// Lets go of a claim and the view state it kept, however it ended: given
    /// up by the reader, or answered by the Session it was a claim on. Nothing
    /// keyed by its local identity outlives it.
    fn release_claim(&mut self) -> Option<ProvisionalSession> {
        let reference = self.provisional_reference();
        let released = self.provisional.take()?;
        if let Some(reference) = reference {
            self.session_interactions.remove(&reference);
        }
        self.aside.drop_stand_in();
        self.transcript_generation = self.transcript_generation.wrapping_add(1);
        Some(released)
    }

    /// The Session the Provisional Session draws as, built from what this
    /// client knows and held nowhere: a claim it renders from rather than a
    /// Session anything may act on. It describes the Attachments its Prompt
    /// binds by what this client uploaded, as the Session will once it
    /// arrives.
    pub(super) fn provisional_snapshot(&self) -> Option<SessionSnapshot> {
        let provisional = self.provisional.as_ref()?;
        Some(SessionSnapshot {
            title: provisional.prompt.text.trim().to_owned(),
            icon: None,
            session: crate::protocol::Session {
                context_fill: None,
                id: provisional.session_id,
                workspace: self.workspace.clone(),
                execution_directory: crate::protocol::ExecutionDirectory {
                    path: self
                        .execution_directory
                        .clone()
                        .unwrap_or_else(|| self.workspace.path.clone()),
                },
                checkout: None,
                agent_selection: self.landing_agent_selection.clone(),
                agent_selection_availability: crate::protocol::ModelAvailability::Available,
                approval_posture: None,
                status: crate::protocol::SessionStatus::Active,
                working_since: None,
                monitoring_since: None,
                parent: None,
            },
            revision: crate::protocol::SessionRevision::INITIAL,
            prompts: Vec::new(),
            turns: Vec::new(),
            messages: Vec::new(),
            activities: Vec::new(),
            transcript: Vec::new(),
            subagent_usage: None,
            total_cost: None,
            subagent_interventions: Vec::new(),
            pending_approvals: Vec::new(),
            submitting_approvals: Vec::new(),
            pending_approvals_revision: crate::protocol::SessionRevision(0),
            watches: Vec::new(),
            attachments: self.composers.uploaded(&provisional.prompt.attachments),
        })
    }

    /// The Provisional Session's one row, projected exactly as the Transcript
    /// projects a Prompt admitted and not yet delivered, so the row the reader
    /// is looking at survives the Session's arrival unchanged.
    pub(super) fn provisional_transcript_view(
        &self,
        snapshot: &SessionSnapshot,
        theme: &Theme,
        width: u16,
    ) -> Option<std::cell::Ref<'_, TranscriptView>> {
        let provisional = self.provisional.as_ref()?;
        // A claim has no disclosure of its own: nothing in it folds, groups,
        // or hides, so the axes open at their default and die with the frame.
        let folds = TranscriptFolds::default();
        let groups = TranscriptGroups::default();
        let turns = TranscriptTurnFolds::default();
        Some(self.transcript_cache.view_with_hyperlinks(
            self.transcript_generation,
            snapshot,
            &[&provisional.prompt],
            TranscriptDisclosure {
                folds: &folds,
                groups: &groups,
                turns: &turns,
                reasoning_visibility: self.settings().transcript.reasoning_visibility,
            },
            &self.attachment_previews,
            theme,
            width,
            self.hyperlinks,
        ))
    }

    /// Remembers the one undelivered Prompt an interrupt just asked to withdraw,
    /// so this client — the one that interrupted — is the one its text comes
    /// back to when the Session says it was cancelled.
    ///
    /// An interrupt withdraws the Prompt the Session is Working for, which is
    /// the one admitted to begin a Turn and named by no Turn yet: a queued
    /// Prompt is left where it is, and one a Turn has already taken is being
    /// delivered rather than waiting. Where more than one could answer that
    /// description, this client's own is the one it is owed, and the earliest
    /// admitted otherwise — never more than one, and never in place of a
    /// withdrawal already awaited.
    fn await_withdrawal(&mut self, session: &SessionReference, own: Option<PromptId>) {
        let Some(snapshot) = self.session.as_ref().map(SessionProjection::snapshot) else {
            return;
        };
        let mut candidates = snapshot
            .prompts
            .iter()
            .filter(|prompt| {
                prompt.status == PromptStatus::Pending && prompt.delivery == PromptDelivery::Steer
            })
            .filter(|prompt| {
                !snapshot
                    .turns
                    .iter()
                    .any(|turn| turn.prompt_id == Some(prompt.id))
            })
            .collect::<Vec<_>>();
        candidates.sort_unstable_by_key(|prompt| prompt.admission_order);
        let owned = |id: PromptId| {
            own == Some(id)
                || self
                    .pending_steers
                    .iter()
                    .any(|held| &held.session == session && held.prompt.id == id)
        };
        let Some(awaited) = candidates
            .iter()
            .find(|prompt| owned(prompt.id))
            .or(candidates.first())
            .map(|prompt| InitialPrompt {
                id: prompt.id,
                text: prompt.text.clone(),
                skill_invocations: prompt.skill_invocations.clone(),
                attachments: prompt.attachments.clone(),
            })
        else {
            return;
        };
        if self
            .withdrawing
            .iter()
            .any(|held| held.prompt.id == awaited.id)
        {
            return;
        }
        self.withdrawing.push(HeldPrompt {
            session: session.clone(),
            prompt: awaited,
        });
    }

    /// Returns a withdrawn Prompt's text to the composer of the Session it was
    /// written in, cursor at its end, as a refused admission does.
    fn reconcile_withdrawn_prompts(&mut self) {
        let Some(snapshot) = self.session.as_ref().map(SessionProjection::snapshot) else {
            return;
        };
        let session = self
            .session_reference
            .as_ref()
            .expect("a Session snapshot carries its reference")
            .clone();
        let mut returned = Vec::new();
        self.withdrawing.retain(|awaited| {
            if awaited.session != session {
                return true;
            }
            match snapshot
                .prompts
                .iter()
                .find(|prompt| prompt.id == awaited.prompt.id)
            {
                Some(prompt) if prompt.status == PromptStatus::Pending => true,
                Some(prompt) if prompt.status == PromptStatus::Cancelled => {
                    returned.push(awaited.prompt.clone());
                    false
                }
                // Delivered after all, or gone from the Session: nothing to
                // hand back.
                _ => false,
            }
        });
        for prompt in returned {
            self.composers
                .return_prompt(ComposerKey::Session(session.clone()), &prompt);
        }
    }

    fn track_pending_steer(&mut self, session: SessionReference, prompt: InitialPrompt) {
        if !self
            .pending_steers
            .iter()
            .any(|pending| pending.prompt.id == prompt.id)
        {
            self.pending_steers.push(HeldPrompt { session, prompt });
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

    /// What an interrupt would reach here, and `None` where the gesture has
    /// nothing to stop and the key stays inert.
    ///
    /// A Session Working for a Prompt it has not delivered is one of them: the
    /// interrupt withdraws that Prompt rather than stopping a Turn that does not
    /// exist. So is a Provisional Session, whose claim the reader may give up
    /// before the Server has answered at all.
    fn interrupt_target(&self) -> Option<InterruptTarget> {
        if let Some(turn_id) = self.active_turn_id() {
            return Some(InterruptTarget::Turn(turn_id));
        }
        if !self.working_subagent_ids().is_empty() {
            return Some(InterruptTarget::Subagents);
        }
        if self
            .provisional
            .as_ref()
            .is_some_and(|claim| !claim.refused())
        {
            return Some(InterruptTarget::Prompt);
        }
        let snapshot = self.session.as_ref()?.snapshot();
        if snapshot.working_since().is_some() {
            return Some(InterruptTarget::Prompt);
        }
        snapshot
            .monitoring_since()
            .is_some()
            .then_some(InterruptTarget::Watches)
    }

    /// Whether the reader has confirmed stopping the Watches of the Monitoring
    /// `session` has been doing since `since`, so its Working Indicator says
    /// the stop is under way rather than offering it again.
    pub(super) fn watch_stop_requested(
        &self,
        session: &SessionReference,
        since: crate::protocol::SessionTimestamp,
    ) -> bool {
        self.watch_stop
            .as_ref()
            .is_some_and(|(requested, at)| requested == session && *at == since)
    }

    /// Remembers that the reader confirmed stopping the open Session's
    /// Watches, against the Monitoring reading they stop.
    fn request_watch_stop(&mut self, session: &SessionReference) {
        self.watch_stop = self
            .session
            .as_ref()
            .and_then(|projection| projection.snapshot().monitoring_since())
            .map(|since| (session.clone(), since));
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
                    None => matches!(
                        self.interrupt_target(),
                        Some(
                            InterruptTarget::Subagents
                                | InterruptTarget::Prompt
                                | InterruptTarget::Watches
                        )
                    ),
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

/// What the interrupt gesture would reach.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InterruptTarget {
    Turn(TurnId),
    Subagents,
    /// A Prompt admitted and not yet delivered — including one this client has
    /// only claimed, with no Session to send anything to yet. Interrupting
    /// withdraws it instead of stopping a Turn.
    Prompt,
    /// Nothing is Working, but the Session is Monitoring: interrupting stops
    /// the Watches it waits on, and settles nothing (ADR 0030).
    Watches,
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

/// Which way a list is being read: toward its head, or toward its tail.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScrollDirection {
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
/// distance, and target do.
#[derive(Clone, Copy, Debug)]
struct LastClick {
    count: u8,
    target: ClickTarget,
    position: Position,
    at: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClickTarget {
    Blank,
    Selection(SelectionSurface),
    SidebarEdge,
    AsideEdge,
}

impl LastClick {
    /// Whether `next` continues this click's count: same target, within the
    /// interval, and within the slop on either axis.
    fn continues(&self, next: &LastClick, interval: Duration) -> bool {
        self.target == next.target
            && next.at.saturating_duration_since(self.at) <= interval
            && self.position.x.abs_diff(next.position.x) <= CLICK_SLOP
            && self.position.y.abs_diff(next.position.y) <= CLICK_SLOP
    }
}

#[derive(Clone, Debug)]
struct LeftPress {
    position: Position,
    pointer: Position,
    dragged: bool,
    outside_overlay: bool,
    selection_anchor: Option<(SelectionCell, u64, SelectionSurface)>,
    composer_anchor: Option<std::ops::Range<usize>>,
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
    /// One Origin's loss has outlived its grace period and is now the
    /// reader's to see.
    ReconnectGraceElapsed(Outlook),
    Managed(ManagedEvent),
    OriginCatalog {
        outlook: Outlook,
        event: ManagedEvent,
    },
    Session(SessionEvent),
    SessionSubscriptionEnded,
    /// One event from the per-tree subscription asked through `through`,
    /// which is how the Aside's Subagents Section learns the tree the open
    /// Session belongs to.
    SubagentTree {
        through: SessionReference,
        event: SubagentTreeEvent,
    },
    PromptAdmissionSucceeded {
        session: SessionReference,
        prompt_id: PromptId,
    },
    PromptAdmissionFailed {
        session: SessionReference,
        prompt_id: PromptId,
        error: String,
    },
    CheckoutRemoval {
        request_id: uuid::Uuid,
        result: Result<crate::protocol::RemoveCheckoutResult, String>,
    },
    CheckoutPrepared {
        attempt_id: uuid::Uuid,
        prompt_id: PromptId,
        result: crate::protocol::PrepareCheckoutResult,
    },
    CheckoutPreparationFailed {
        attempt_id: uuid::Uuid,
        prompt_id: PromptId,
        error: String,
    },
    SessionCreationFailed {
        prompt_id: PromptId,
        code: Option<SessionErrorCode>,
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
    SessionAttachFailed(String),
    OriginSessionAttachFailed {
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
    ApprovalSubmissionReconciled {
        id: crate::protocol::ApprovalId,
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
    /// How a Remote's removal ended: the local Server forgot it either way,
    /// and the removal says whether the Remote itself answered.
    RemoteRemoved {
        name: String,
        result: std::result::Result<crate::protocol::RemoteRemoval, String>,
    },
    RemoteProbed {
        name: String,
        result: Result<crate::protocol::RemoteHealth, String>,
    },
    WorkspaceResolved {
        outlook: Outlook,
        surface: WorkspaceResolutionSurface,
        request_id: u64,
        result: std::result::Result<crate::protocol::ResolvedWorkspace, String>,
    },
    /// What the host clipboard held when a paste read it.
    ClipboardRead {
        paste: PasteId,
        read: ClipboardRead,
    },
    /// A pasted image's upload stored it as this Attachment.
    AttachmentUploaded {
        paste: PasteId,
        descriptor: crate::protocol::AttachmentDescriptor,
    },
    /// A pasted image's upload was refused, or never reached its Server, in
    /// words the reader can be shown.
    AttachmentUploadFailed {
        paste: PasteId,
        reason: String,
    },
    /// The thumbnail the [`ApplicationTransition::FetchAttachment`] naming
    /// `request` asked for, made at `cell_size`.
    AttachmentThumbnail {
        request: ThumbnailRequest,
        attachment_id: AttachmentId,
        cell_size: CellSize,
        thumbnail: Thumbnail,
    },
    /// The fetch or the decode behind a thumbnail failed, so the Attachment's
    /// dimmed line stands in its place for as long as the Session is open.
    AttachmentThumbnailFailed {
        request: ThumbnailRequest,
        attachment_id: AttachmentId,
        cell_size: CellSize,
    },
    /// Which of a check's Attachments their Server no longer stores, or why
    /// it could not be asked.
    AttachmentsChecked {
        check: AttachmentCheckId,
        result: std::result::Result<Vec<AttachmentId>, String>,
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
    MoveCursorLineStart,
    MoveCursorLineEnd,
    ExtendSelectionLeft,
    ExtendSelectionRight,
    ExtendSelectionUp,
    ExtendSelectionDown,
    ExtendSelectionLineStart,
    ExtendSelectionLineEnd,
    SelectAll,
    CutSelection,
    HistoryPrevious,
    HistoryNext,
    ScrollTranscriptPageUp,
    ScrollTranscriptPageDown,
    FollowLatest,
    /// One step of the wheel, at the cell the pointer stood on. Where it
    /// stood decides what moves — the Sidebar's list over the Sidebar, the
    /// Transcript anywhere else — so it is resolved against the frame's own
    /// geometry, as a click is, rather than by who has the keys.
    WheelAt {
        position: Position,
        direction: ScrollDirection,
    },
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
    /// A semantic command that carries typed text as its own payload rather
    /// than acting on a target already in view state — the Icon Picker's
    /// search insert is the first of these, so it still reaches
    /// [`Application::invoke_semantic`] like every other semantic command
    /// rather than mutating picker state directly from the keymap.
    InvokeSemanticText(SemanticCommandId, String),
    InsertText(String),
    PasteText(String),
    InsertConnectText(String),
    DeleteConnectTextBackward,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApplicationTransition {
    SubmitDecision {
        session: SessionReference,
        id: crate::protocol::ApprovalId,
        decision: crate::protocol::Decision,
    },
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
    /// A user's own choice of a Session's Icon by Icon Catalog name, made
    /// through the Icon Picker. Routed to the Session's own Origin like every
    /// other Session act.
    SetSessionIcon {
        session: SessionReference,
        icon: String,
    },
    /// A user's own choice of a Workspace's Icon by Icon Catalog name, made
    /// through the Icon Picker. Routed to the Workspace's own Origin like
    /// every other Workspace act.
    SetWorkspaceIcon {
        origin: Outlook,
        workspace_id: WorkspaceId,
        icon: String,
    },
    /// Both removal requests are boxed: each carries a whole Repository and
    /// Checkout, which would otherwise size every transition to them.
    PreviewCheckoutRemoval {
        request_id: uuid::Uuid,
        target: Box<crate::protocol::CheckoutRemovalTarget>,
    },
    RemoveCheckout {
        request_id: uuid::Uuid,
        request: Box<crate::protocol::RemoveCheckoutRequest>,
    },
    PrepareCheckout {
        attempt_id: uuid::Uuid,
        prompt_id: PromptId,
        request: crate::protocol::PrepareCheckoutRequest,
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
    /// Report a listed root Session as Viewed while beginning its attach.
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
    UpdateApprovalPosture {
        session: SessionReference,
        request: crate::protocol::UpdateApprovalPostureRequest,
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
    /// Read the host clipboard for a paste, off the UI thread, and answer
    /// with [`ApplicationEvent::ClipboardRead`].
    ReadClipboard(PasteId),
    /// Upload a pasted image's PNG bytes to `origin` — through this Client's
    /// own Server, which carries them on to a Remote — and answer with
    /// [`ApplicationEvent::AttachmentUploaded`] or
    /// [`ApplicationEvent::AttachmentUploadFailed`].
    UploadAttachment {
        paste: PasteId,
        origin: Outlook,
        png: Vec<u8>,
    },
    /// Fetch an Attachment's bytes from `origin` and make its thumbnail at
    /// `cell_size` for `protocol`, off the UI thread, answering with
    /// [`ApplicationEvent::AttachmentThumbnail`] or
    /// [`ApplicationEvent::AttachmentThumbnailFailed`], either naming
    /// `request`.
    FetchAttachment {
        request: ThumbnailRequest,
        origin: Outlook,
        attachment_id: AttachmentId,
        cell_size: CellSize,
        protocol: GraphicsProtocol,
    },
    /// Ask `origin` whether it still stores each of `attachments`, which a
    /// Prompt recalled from composer history binds, without fetching their
    /// bytes, and answer with [`ApplicationEvent::AttachmentsChecked`].
    CheckAttachments {
        check: AttachmentCheckId,
        origin: Outlook,
        attachments: Vec<AttachmentId>,
    },
    OpenHyperlink(String),
    RemovePeer(String),
    /// End the Pairing with the named Remote, which the reader confirmed by
    /// pressing the removal key a second time.
    RemoveRemote(String),
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

    fn from_state(mut state: TuiState, terminal_facts: TerminalFacts) -> Self {
        state.hyperlinks = terminal_facts.hyperlinks;
        let mut application = Self {
            state,
            slots: RenderSlots::builtins(),
            terminal_facts,
            theme: Theme::system(),
            config_root: None,
            theme_catalog: ThemeCatalog::default(),
        };
        application.resolve_theme();
        application.hold_attachment_previews();
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
        self.state.hyperlinks = terminal_facts.hyperlinks;
        self.resolve_theme();
        self.hold_attachment_previews();
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

    /// Injects the wall clock the Aside's elapsed times are read against.
    /// Production reads the system clock, as the Server stamps its Sessions;
    /// tests move a deterministic one to see a time advance.
    pub fn with_session_clock(
        mut self,
        clock: impl Fn() -> crate::protocol::SessionTimestamp + Send + Sync + 'static,
    ) -> Self {
        self.state.session_clock = SessionClock(Arc::new(clock));
        self
    }

    /// Injects how long after one left press a second one continues the click
    /// count. Production uses the fixed interval; tests shorten it or advance
    /// the presentation clock instead of waiting.
    pub fn with_click_interval(mut self, interval: Duration) -> Self {
        self.state.click_interval = interval;
        self
    }

    /// Injects how long a self-presented Intervention panel ignores keys.
    /// Production uses the fixed delay; tests shorten it or advance the
    /// presentation clock instead of waiting.
    pub fn with_intervention_arming_delay(mut self, delay: Duration) -> Self {
        self.state.intervention_arming_delay = delay;
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
        self.present_intervention();
        self.hold_attachment_previews();
        Ok(transition)
    }

    /// Keeps the thumbnails held to the open Session's, presented as the
    /// Setting and the terminal allow, and to the Attachments it and the
    /// draft in view still bind. Read from state after every event rather
    /// than at each place a Session opens or a draft changes, so no route
    /// into another Session can keep the last one's thumbnails and no image
    /// deleted from a draft keeps its own.
    fn hold_attachment_previews(&mut self) {
        let scope = self
            .state
            .route
            .clone()
            .or_else(|| self.state.provisional_reference())
            .map_or_else(
                || PreviewScope::Landing(self.state.outlook.clone()),
                PreviewScope::Session,
            );
        let mode = PreviewMode::of(
            self.state.settings().transcript.image_previews,
            self.terminal_facts.graphics,
            self.terminal_facts.cell_size,
        );
        self.state.attachment_previews.hold_for(scope, mode);
        if self.state.attachment_previews.holds_any() {
            let bound = self.state.bound_attachments();
            self.state.attachment_previews.retain_bound(&bound);
        }
    }

    /// The next thumbnail the last frame drew a strip for and none is held or
    /// coming for, as the fetch that makes it; `Continue` once there is none.
    /// A caller that draws frames drains this after each one.
    pub fn take_attachment_fetch(&mut self) -> ApplicationTransition {
        let Some((request, attachment_id, cell_size, protocol)) =
            self.state.attachment_previews.take_fetch()
        else {
            return ApplicationTransition::Continue;
        };
        let origin = self
            .state
            .route
            .as_ref()
            .map_or_else(|| self.state.outlook.clone(), |route| route.origin.clone());
        ApplicationTransition::FetchAttachment {
            request,
            origin,
            attachment_id,
            cell_size,
            protocol,
        }
    }

    /// Presents the open Session's oldest Intervention when nothing stands in
    /// its way. This is read from state rather than driven by an arrival, so
    /// opening a Session that already owes the reader something, coming back
    /// from a picker, and reconnecting all reach the same panel; and it runs
    /// before the frame, so the reader never sees a Session that owes them a
    /// Decision without the panel that takes it.
    fn present_intervention(&mut self) {
        let Some(owner) = self.state.session_reference.clone() else {
            return;
        };
        self.state.prune_dismissed_interventions();
        // An Origin that has stopped answering takes its panels off the frame
        // rather than leaving one standing that no key reaches and no Decision
        // leaves. Hidden, not dismissed: it presents itself again, unanswered,
        // as soon as the Remote answers.
        if self.state.interventions_are_held() {
            self.state.approvals.hide();
            self.state.questionnaires.hide();
            return;
        }
        if self.state.surface_above_interventions().is_some() {
            return;
        }
        // An arrival waits rather than replacing work already in progress.
        if self.state.approvals.is_open(Some(&owner))
            || self.state.questionnaires.is_open(Some(&owner))
        {
            return;
        }
        let Some(intervention) = self.state.next_intervention() else {
            return;
        };
        match intervention {
            PresentableIntervention::Approval(id) => self.state.approvals.present(owner, id),
            PresentableIntervention::Questionnaire(questionnaire) => {
                self.state.questionnaires.present(owner, &questionnaire);
            }
        }
        self.state.intervention_armed_until = self
            .state
            .presentation_clock
            .now()
            .checked_add(self.state.intervention_arming_delay);
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
            ApplicationEvent::ReconnectGraceElapsed(outlook) => {
                Ok(self.elapse_reconnect_grace(&outlook))
            }
            ApplicationEvent::ClipboardRead { paste, read } => {
                Ok(self.receive_clipboard_read(paste, read))
            }
            ApplicationEvent::AttachmentUploaded { paste, descriptor } => {
                self.receive_uploaded_attachment(paste, descriptor);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::AttachmentsChecked { check, result } => {
                self.receive_attachment_check(check, result);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::AttachmentUploadFailed { paste, reason } => {
                if self.state.clipboard_pastes.finish(paste).is_some() {
                    self.paste_failed(paste, super::clipboard_paste::refused(reason));
                }
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::AttachmentThumbnail {
                request,
                attachment_id,
                cell_size,
                thumbnail,
            } => {
                self.state.attachment_previews.receive(
                    request,
                    attachment_id,
                    cell_size,
                    Some(thumbnail),
                );
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::AttachmentThumbnailFailed {
                request,
                attachment_id,
                cell_size,
            } => {
                self.state
                    .attachment_previews
                    .receive(request, attachment_id, cell_size, None);
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::Managed(event) => self.handle_managed_event(event),
            ApplicationEvent::OriginCatalog { outlook, event } => {
                self.handle_origin_catalog(outlook, event)
            }
            ApplicationEvent::Session(event) => {
                if let SessionEvent::Snapshot(snapshot) = &event {
                    let reference =
                        SessionReference::new(self.state.outlook.clone(), snapshot.session.id);
                    self.state.approvals.reconcile(&reference, snapshot);
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
            ApplicationEvent::SubagentTree { through, event } => {
                let open = self.state.route.clone();
                self.state.aside.receive_tree(through, event, open.as_ref());
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::SessionSubscriptionEnded => Ok(self
                .session_reference()
                .map_or(ApplicationTransition::Continue, |session_id| {
                    ApplicationTransition::SubscribeSession(session_id)
                })),
            ApplicationEvent::SessionCreated(snapshot) => self.take_created_session(snapshot),
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
            ApplicationEvent::SessionAttachFailed(error) => {
                if self.state.sidebar.is_attaching()
                    || (self.state.route.is_some() && self.state.session.is_none())
                {
                    // The target remains the route and the refusal belongs to
                    // its main content, not to the listing that led there.
                    Ok(self.fail_open_session_attach(error))
                } else {
                    Ok(Self::session_picker_listing_transition(
                        self.state.session_picker.fail_attach(error),
                    ))
                }
            }
            ApplicationEvent::OriginSessionAttachFailed { reference, error } => {
                if reference.origin != self.state.outlook {
                    return Ok(ApplicationTransition::Continue);
                }
                if self.state.route.as_ref() == Some(&reference) && self.state.session.is_none() {
                    Ok(self.fail_open_session_attach(error))
                } else {
                    Ok(Self::session_picker_listing_transition(
                        self.state.session_picker.fail_attach(error),
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
            ApplicationEvent::ApprovalSubmissionReconciled {
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
                        .approvals
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
                    self.state.submission_error = error;
                }
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::SessionOperationFailed(error) => {
                // A Watch stop that failed stopped nothing, so the gesture is
                // offered again.
                self.state.watch_stop = None;
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
            ApplicationEvent::RemoteRemoved { name, result } => match result {
                Ok(removal) => {
                    self.state
                        .connect_overlay
                        .remote_removed(&removal.name, removal.acknowledged);
                    let message = format!("{name} was removed");
                    Ok(self.end_pairing(&Outlook::Remote(name), &message))
                }
                Err(error) => {
                    self.state.connect_overlay.removal_failed(error);
                    Ok(ApplicationTransition::Continue)
                }
            },
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
                        if surface == WorkspaceResolutionSurface::WorktreeList {
                            self.state.adopt_context(workspace.clone());
                            self.state.worktree_picker.load(workspace);
                            return Ok(ApplicationTransition::Continue);
                        }
                        self.state.adopt_context(workspace.clone());
                        match surface {
                            WorkspaceResolutionSurface::WorktreeList => {
                                unreachable!("reading handled above")
                            }
                            WorkspaceResolutionSurface::WorktreeSelection => {
                                self.state.worktree_picker.close();
                                Ok(ApplicationTransition::Continue)
                            }
                            WorkspaceResolutionSurface::Outlook => {
                                self.state.sidebar.refresh_after_outlook_workspace();
                                Ok(self.take_session_listing_transition())
                            }
                            WorkspaceResolutionSurface::WorkspacePicker => {
                                self.state.workspace_picker.close();
                                Ok(self.open_landing())
                            }
                            WorkspaceResolutionSurface::Sidebar => {
                                let activation =
                                    self.state.sidebar.accept_workspace(workspace.workspace);
                                self.state.sidebar.adopt_execution_directory(
                                    self.state.execution_directory.clone(),
                                );
                                match activation {
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
                            WorkspaceResolutionSurface::WorktreeList
                            | WorkspaceResolutionSurface::WorktreeSelection => {
                                self.state.worktree_picker.fail(error);
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
            ApplicationEvent::CheckoutRemoval { request_id, result } => {
                let picker = &mut self.state.worktree_picker;
                if picker.removal_request == Some(request_id) {
                    picker.removal_request = None;
                    picker.loading = false;
                    match result {
                        Ok(result) if result.removed => {
                            let checkout = &result.preview.target.checkout;
                            let status = crate::protocol::ExecutionDirectoryStatus::Unavailable {
                                reason: "Worktree removed; prompt a retained Session to recover it"
                                    .into(),
                            };
                            if self.state.workspace.id == checkout.repository.workspace_id()
                                && self
                                    .state
                                    .execution_directory
                                    .as_ref()
                                    .is_some_and(|path| path.starts_with(&checkout.root))
                            {
                                self.state.execution_status = status.clone();
                            }
                            if let Some(remembered) =
                                self.state.remembered_execution_directories.get_mut(&(
                                    self.state.outlook.clone(),
                                    checkout.repository.workspace_id(),
                                ))
                                && remembered
                                    .directory
                                    .as_ref()
                                    .is_some_and(|path| path.starts_with(&checkout.root))
                            {
                                remembered.status = status.clone();
                            }
                            if let Some(context) = &mut picker.context {
                                if context
                                    .checkout
                                    .as_ref()
                                    .is_some_and(|current| current.id == checkout.id)
                                {
                                    context.execution_status = status;
                                }
                                for reading in &mut context.checkouts {
                                    if reading.association.id == result.preview.target.checkout.id {
                                        reading.revision = None;
                                        reading.availability = crate::protocol::SourceControlAvailability::Unavailable { reason: "Worktree removed; prompt a retained Session to recover it".into() };
                                    }
                                }
                            }
                            picker.removal = None;
                            picker.error = Some(
                                match result.preview.branch_outcome {
                                    crate::protocol::CheckoutBranchOutcome::Deleted => {
                                        "Worktree removed. Branch deleted; Session histories retained"
                                    }
                                    crate::protocol::CheckoutBranchOutcome::Retained => {
                                        "Worktree removed. Branch retained; Session histories retained"
                                    }
                                }
                                .into(),
                            );
                        }
                        Ok(result) => {
                            picker.removal = Some(result.preview);
                            picker.error = result.error;
                        }
                        Err(error) => picker.error = Some(error),
                    }
                }
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::CheckoutPrepared {
                attempt_id,
                prompt_id,
                result,
            } => {
                if self
                    .state
                    .pending_submission
                    .as_ref()
                    .is_none_or(|p| p.prompt.id != prompt_id)
                    || self
                        .state
                        .pending_submission
                        .as_ref()
                        .is_none_or(|pending| pending.preparation_attempt != Some(attempt_id))
                    || self
                        .state
                        .new_worktree
                        .as_ref()
                        .is_none_or(|p| p.id != result.preparation.id)
                {
                    return Ok(ApplicationTransition::Continue);
                }
                let intent = self.state.new_worktree.take();
                if let Some(location) = result.location {
                    self.state.adopt_context(location);
                }
                self.state.new_worktree = intent.clone();
                if result.preparation.checkout_created
                    && let Some(intent) = &mut self.state.new_worktree
                {
                    intent.source = result.preparation.destination.clone();
                }
                if let Some(claim) = self.state.provisional.as_mut() {
                    claim.prepared = Some(PreparedFor {
                        id: result.preparation.id,
                        destination: result.preparation.destination.clone(),
                        ready: result.preparation.ready,
                    });
                }
                if let Some(error) = result.error {
                    // Preparation refusals have the same retry affordance as
                    // creation refusals, but keep the retained Worktree proof
                    // above so the next attempt resumes this exact checkout.
                    self.state.refuse_creation(prompt_id, error, false);
                    self.state.sync_composer_completion();
                    return Ok(ApplicationTransition::Continue);
                }
                let prompt = self
                    .state
                    .pending_submission
                    .as_ref()
                    .unwrap()
                    .prompt
                    .clone();
                if let Some(claim) = self.state.provisional.as_mut() {
                    claim.phase = ProvisionalSessionPhase::CreatingSession;
                    if let Some(prepared) = &mut claim.prepared {
                        prepared.ready = true;
                    }
                }
                Ok(ApplicationTransition::CreateSession(CreateSessionRequest {
                    preparation_id: Some(result.preparation.id),
                    agent_selection: self.state.landing_agent_selection.clone(),
                    execution_directory: result.preparation.destination,
                    prompt,
                }))
            }
            ApplicationEvent::CheckoutPreparationFailed {
                attempt_id,
                prompt_id,
                error,
            } => {
                let current_attempt =
                    self.state
                        .pending_submission
                        .as_ref()
                        .is_some_and(|pending| {
                            pending.prompt.id == prompt_id
                                && pending.target == SubmissionTarget::CreateSession
                                && pending.preparation_attempt == Some(attempt_id)
                        });
                if current_attempt {
                    self.state.refuse_creation(prompt_id, error, false);
                    self.state.sync_composer_completion();
                }
                Ok(ApplicationTransition::Continue)
            }
            ApplicationEvent::SessionCreationFailed {
                prompt_id,
                code,
                error,
            } => {
                if self
                    .state
                    .pending_submission
                    .as_ref()
                    .is_some_and(|pending| {
                        pending.prompt.id == prompt_id
                            && pending.target == SubmissionTarget::CreateSession
                    })
                {
                    self.state.refuse_creation(
                        prompt_id,
                        error,
                        code == Some(SessionErrorCode::InvalidSkillInvocation),
                    );
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
                        let open = self.state.sidebar_highlight();
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

    /// Records one left press against its resolved target, sharing the same
    /// interval, slop, and count limit across selectable text and the Sidebar
    /// edge without sending the edge through either surface's click routing.
    fn record_click(&mut self, target: ClickTarget, position: Position) -> LastClick {
        let mut click = LastClick {
            count: 1,
            target,
            position,
            at: self.state.presentation_clock.now(),
        };
        if let Some(last) = self.state.last_click
            && last.continues(&click, self.state.click_interval)
        {
            click.count = last.count.saturating_add(1).min(CLICK_COUNT_LIMIT);
        }
        self.state.last_click = Some(click);
        click
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
        // This machine's own Server going away stands the whole frame down,
        // and only leaving is still answered: a reader must always be able to
        // quit whatever the connection is doing.
        if self.state.reconnect_overlay_visible() && !leaves_the_application(&command) {
            return Ok(ApplicationTransition::Continue);
        }
        if self.defers_for_agent_selection(&command) {
            return Ok(ApplicationTransition::Continue);
        }
        // One choke point decides every refusal a Remote that stopped
        // answering makes: the question is only ever whether what this command
        // would do goes to an Origin nothing reaches.
        if let Some(origin) = self.command_origin(&command)
            && self.refuse_if_unreachable(&origin)
        {
            return Ok(ApplicationTransition::Continue);
        }
        match command {
            CommandId::ClearOrExit | CommandId::OpenContextMenuAt { .. }
                if self.state.has_text_selection() =>
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
            CommandId::InvokeSemanticText(command, text) => {
                self.invoke_semantic(command.on_text(text))
            }
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
            | CommandId::MoveCursorLineStart
            | CommandId::MoveCursorLineEnd
            | CommandId::ExtendSelectionLeft
            | CommandId::ExtendSelectionRight
            | CommandId::ExtendSelectionUp
            | CommandId::ExtendSelectionDown
            | CommandId::ExtendSelectionLineStart
            | CommandId::ExtendSelectionLineEnd
            | CommandId::SelectAll
            | CommandId::CutSelection
            | CommandId::HistoryPrevious
            | CommandId::HistoryNext) => Ok(self.handle_composer_command(command)),
            command @ (CommandId::ScrollTranscriptPageUp
            | CommandId::ScrollTranscriptPageDown
            | CommandId::FollowLatest) => self.handle_transcript_command(command),
            CommandId::WheelAt {
                position,
                direction,
            } => {
                self.state.wheel_at(position, direction);
                Ok(ApplicationTransition::Continue)
            }
            CommandId::PressAt { position } => {
                // The edge is a pointer surface only in the plain
                // Sidebar-beside-main state. Check it before any row,
                // composer, or Text Selection can claim the press; overlays
                // remain newer surfaces and keep their existing routing.
                if self.state.top_selection_overlay().is_none()
                    && !self.state.overlay_owns_input()
                    && self.state.sidebar.column_mut().hold_edge_at(position)
                {
                    self.invoke_semantic(SemanticCommandId::TextSelectionClear)?;
                    self.state.left_press = None;
                    let click = self.record_click(ClickTarget::SidebarEdge, position);
                    if click.count == 2 {
                        self.invoke_semantic(SemanticCommandId::SidebarWidthReset)?;
                    }
                    return Ok(ApplicationTransition::Continue);
                }
                // The Aside's edge answers on the same terms, beside an open
                // Session where the Aside stands.
                if self.state.top_selection_overlay().is_none()
                    && !self.state.overlay_owns_input()
                    && self.state.aside_is_present()
                    && self.state.aside.column_mut().hold_edge_at(position)
                {
                    self.invoke_semantic(SemanticCommandId::TextSelectionClear)?;
                    self.state.left_press = None;
                    let click = self.record_click(ClickTarget::AsideEdge, position);
                    if click.count == 2 {
                        self.invoke_semantic(SemanticCommandId::AsideWidthReset)?;
                    }
                    return Ok(ApplicationTransition::Continue);
                }
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
                        self.transcript_cell(position).map(|cell| {
                            (
                                cell,
                                self.state.transcript_cache.selection_epoch(),
                                SelectionSurface::Transcript,
                            )
                        })
                    });

                let composer_anchor = selection_anchor
                    .filter(|(_, _, surface)| *surface == SelectionSurface::Composer)
                    .and_then(|(cell, _, _)| {
                        self.state.composers.selection_frame()?.cell_range(cell)
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
                    composer_anchor,
                });
                let click = self.record_click(
                    selection_anchor.map_or(ClickTarget::Blank, |(_, _, surface)| {
                        ClickTarget::Selection(surface)
                    }),
                    position,
                );
                if click.count >= 2
                    && click.target == ClickTarget::Selection(SelectionSurface::Transcript)
                {
                    self.invoke_semantic(SemanticInvocation {
                        id: if click.count == 2 {
                            SemanticCommandId::TextSelectionWord
                        } else {
                            SemanticCommandId::TextSelectionLine
                        },
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
                if self.state.sidebar.column_mut().release_edge()
                    | self.state.aside.column_mut().release_edge()
                {
                    // Reset is decided by the press count. Release merely
                    // ends the held paint and cannot become a row click or a
                    // Text Selection copy.
                    self.state.left_press = None;
                    return Ok(ApplicationTransition::Continue);
                }
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

                let Some(press) = self.state.left_press.take() else {
                    return Ok(ApplicationTransition::Continue);
                };
                let mut transition = ApplicationTransition::Continue;
                if !press.dragged && press.position == position {
                    transition = self.invoke_semantic(SemanticInvocation {
                        id: SemanticCommandId::PointerClick,
                        subject: SemanticSubject::ScreenPosition(position),
                    })?;
                }
                // A press that marked a word or Line made a selection the way a drag
                // does, so its release copies the way a drag's does; the copy
                // is what the release reports when both happen.
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
                Ok(transition)
            }
            CommandId::ClickAt { position } => self.handle_click(position),
            CommandId::OpenContextMenuAt { position } => {
                if self.state.workspace_picker.is_open() {
                    let show_icons = self.state.settings().appearance.show_icons;
                    self.state
                        .workspace_picker
                        .open_menu_at(position, show_icons);
                } else {
                    self.state.sidebar.open_menu_at(position);
                }
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

    /// Tries an Origin that has stopped answering again, now, rather than
    /// waiting its backoff out. It is this Client's own reading of what is
    /// recovering that says whether there is anything to try — not the
    /// Sidebar's, which knows only the Origins its chosen scope lists — so the
    /// banner above the composer and the `[unreachable]` row reach the retry
    /// on the very same terms.
    fn retry_origin(&mut self, outlook: Outlook) -> ApplicationTransition {
        if !self.state.is_unreachable(&outlook) {
            return ApplicationTransition::Continue;
        }
        ApplicationTransition::RetryCatalogOrigin(self.state.sidebar.retry_origin(outlook))
    }

    /// Refuses, where the reader can see it, work that was bound for a Remote
    /// that has stopped answering, and answers whether it did. Every refusal a
    /// lost Remote makes is decided here and nowhere else, so a key, a slash,
    /// a pointer and a Sidebar row all get the same answer.
    fn refuse_if_unreachable(&mut self, origin: &Outlook) -> bool {
        if !self.state.is_unreachable(origin) {
            return false;
        }
        let Some(remote) = origin.remote_name() else {
            return false;
        };
        // Naming the Remote, never "Suru": the rest of the Client is working.
        self.state.submission_error = Some(format!(
            "{remote} is unreachable; this waits until it answers"
        ));
        true
    }

    /// The Origin a command's work is bound for, if it is bound for one.
    ///
    /// A command that only moves what the Client already holds — reading a
    /// Transcript, walking the Sidebar, Settings, Themes, turning the Outlook,
    /// leaving — answers `None`, because nothing it does has to reach a
    /// Server. The rest answer the Origin they would act on: the Session's own
    /// where the command names one, and otherwise the Outlook's, since that is
    /// the Server everything which acts or begins answers for.
    fn command_origin(&self, command: &CommandId) -> Option<Outlook> {
        match command {
            // A Prompt is delivered to the Session it was written for, or
            // begins one on the Server the Outlook is turned toward.
            CommandId::SubmitSteer | CommandId::SubmitQueue => Some(
                self.state
                    .session_reference
                    .as_ref()
                    .map_or_else(|| self.state.outlook.clone(), |open| open.origin.clone()),
            ),
            // A picker's choice is applied on the Server it was fetched from.
            CommandId::SelectModel | CommandId::SelectWorkspace => Some(self.state.outlook.clone()),
            // Interrupting a Turn, and promoting or withdrawing a Prompt it
            // has not started, are all asked of the Session's own Server.
            CommandId::RequestInterrupt
            | CommandId::ConfirmInterrupt
            | CommandId::PromoteSelectedPrompt
            | CommandId::CancelSelectedPrompt => self
                .state
                .session_reference
                .as_ref()
                .map(|open| open.origin.clone()),
            // A semantic command is refused in `invoke_semantic`, where the
            // subject it names is known — and where a Sidebar row or a menu
            // item reaches it without passing through here at all.
            _ => None,
        }
    }

    /// The Origin a semantic command acts on, if its work goes to a Server at
    /// all. The subject decides which Origin: a command naming a Session, an
    /// Origin, or a Workspace acts on that one, and everything else acts on
    /// the Outlook's.
    fn semantic_origin(&self, id: SemanticCommandId, subject: &SemanticSubject) -> Option<Outlook> {
        if super::commands::descriptor(id).reach != super::commands::SemanticReach::Origin {
            return None;
        }
        Some(match subject {
            SemanticSubject::Session(reference) => reference.origin.clone(),
            SemanticSubject::Origin(outlook) => outlook.clone(),
            SemanticSubject::Workspace { origin, .. } => origin.clone(),
            _ => self.state.outlook.clone(),
        })
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
            command @ (CommandId::ExtendSelectionLeft
            | CommandId::ExtendSelectionRight
            | CommandId::ExtendSelectionUp
            | CommandId::ExtendSelectionDown
            | CommandId::ExtendSelectionLineStart
            | CommandId::ExtendSelectionLineEnd) => {
                let motion = match command {
                    CommandId::ExtendSelectionLeft => SelectionMotion::Left,
                    CommandId::ExtendSelectionRight => SelectionMotion::Right,
                    CommandId::ExtendSelectionUp => SelectionMotion::Up,
                    CommandId::ExtendSelectionDown => SelectionMotion::Down,
                    CommandId::ExtendSelectionLineStart => SelectionMotion::LineStart,
                    CommandId::ExtendSelectionLineEnd => SelectionMotion::LineEnd,
                    _ => unreachable!(),
                };
                self.state.text_selection.set(None);
                self.state
                    .navigate_composer(|composers, key| composers.extend_selection(key, motion));
            }
            CommandId::SelectAll => {
                if self.state.composers.select_all(self.state.composer_key()) {
                    self.state.text_selection.set(None);
                    self.state.sync_composer_completion();
                }
            }
            CommandId::CutSelection => {
                let mut copied = None;
                self.state.edit_composer(|composers, key| {
                    copied = composers.cut_selection(key);
                });
                if let Some(text) = copied {
                    return ApplicationTransition::CopyToClipboard(text.into());
                }
            }
            CommandId::MoveCursorLeft => self
                .state
                .navigate_composer(|composers, key| composers.move_left(key)),
            CommandId::MoveCursorRight => self
                .state
                .navigate_composer(|composers, key| composers.move_right(key)),
            CommandId::MoveCursorLineStart => self
                .state
                .navigate_composer(|composers, key| composers.move_line_start(key)),
            CommandId::MoveCursorLineEnd => self
                .state
                .navigate_composer(|composers, key| composers.move_line_end(key)),
            CommandId::HistoryPrevious => {
                return self
                    .state
                    .restore_composer_history(|composers, key| composers.history_previous(key));
            }
            // Down serves the composer first — caret movement within the
            // draft, then the history walk — and the Subagent Picker takes
            // exactly the key's one free meaning: Down at rest, which today
            // does nothing. With nothing to browse the open ask leaves the
            // view put, so the key stays as inert as it was.
            CommandId::HistoryNext => {
                self.state.composers.clear_selections();
                if self.state.composer_down_is_inert() {
                    self.state.open_subagent_picker();
                } else {
                    return self
                        .state
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
                self.state.navigate_transcript_page(ScrollDirection::Up);
            }
            CommandId::ScrollTranscriptPageDown => {
                self.state.navigate_transcript_page(ScrollDirection::Down);
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
        // The Icon Picker stands over everything below while it is up, so it
        // answers first, ahead of even the Subagent Picker: a press on one of
        // its cells chooses the glyph it names — the same command Enter
        // invokes — and a press anywhere else puts the picker away without
        // choosing, since it has no clear action to fall back on.
        if self.state.icon_picker.is_open() {
            return Ok(match self.state.icon_picker.hit(position) {
                Some(name) => {
                    self.state.icon_picker.focus(name);
                    self.choose_icon()
                }
                None => {
                    self.state.icon_picker.close();
                    ApplicationTransition::Continue
                }
            });
        }
        // The Workspace Picker's own row menu stands over the picker while it
        // is up, so it answers next: a press anywhere inside its one-item box
        // acts on that item, the same command Enter invokes — a press outside
        // the box never reaches here at all, since `PointerClick` already
        // turned it into the Escape that closes the menu instead (see
        // `active_selection_overlay_area`).
        if self.state.workspace_picker.menu_is_open() {
            return self.activate_workspace_picker_menu();
        }
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
        // An Aside entry opens the Session it stands for without taking the
        // keys, as a Sidebar row does; the rest of the Aside answers nothing.
        match self.state.aside.press_at(position) {
            AsidePress::Elsewhere => {}
            AsidePress::Inert => return Ok(ApplicationTransition::Continue),
            AsidePress::Invoke(invocation) => return self.invoke_semantic(invocation),
        }
        // The header draws the open Session's Icon as its own leading span
        // only while Icons are shown and the Session has one to draw, so a
        // press over that blank space when either is untrue hits nothing
        // recorded here and falls through like any other press.
        let header_icon = self.state.header_icon_area.borrow().clone();
        if let Some(icon) = header_icon
            && icon.contains(position)
            && let Some(session) = self.state.session_reference.clone()
        {
            return self.invoke_semantic(SemanticCommandId::SessionIconChoose.on_session(session));
        }
        // The banner's retry, which is the same command the Sidebar's
        // `[unreachable]` row invokes and names the same Origin.
        let banner = self.state.unreachable_banner_area.borrow().clone();
        if let Some(affordance) = banner
            && affordance.contains(position)
        {
            let outlook = self.state.outlook.clone();
            return self.invoke_semantic(SemanticCommandId::RemoteRetry.on_origin(outlook));
        }
        // A thumbnail is drawn over the rows of the composer or Transcript it
        // stands in, so it answers before either of them.
        if let Some(invocation) = self.state.attachment_previews.press_at(position) {
            return self.invoke_semantic(invocation);
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
        if let Some(target) = self.transcript_hyperlink(position) {
            return self.invoke_semantic(SemanticCommandId::HyperlinkOpen.on_hyperlink(target));
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

    /// Worktree choices affect the next Session's execution context only.
    fn handle_worktree_command(&mut self, command: SemanticCommandId) -> ApplicationTransition {
        use super::worktree_picker::WorktreeChoice;
        match command {
            SemanticCommandId::WorktreeRemove | SemanticCommandId::WorktreeForceRemove => {
                if self.state.worktree_picker.loading {
                    return ApplicationTransition::Continue;
                }
                let picker = &mut self.state.worktree_picker;
                let request_id = uuid::Uuid::new_v4();
                if let Some(preview) = picker.removal.clone() {
                    if preview.working_sessions != 0 {
                        picker.error =
                            Some("A Session is Working on this Server; removal is blocked".into());
                        return ApplicationTransition::Continue;
                    }
                    let force = command == SemanticCommandId::WorktreeForceRemove;
                    if preview.inspection.requires_force() && !force {
                        picker.error = Some(
                            "These conditions require the distinct Force remove action (F)".into(),
                        );
                        return ApplicationTransition::Continue;
                    }
                    picker.loading = true;
                    picker.removal_request = Some(request_id);
                    return ApplicationTransition::RemoveCheckout {
                        request_id,
                        request: Box::new(crate::protocol::RemoveCheckoutRequest {
                            preview,
                            force,
                        }),
                    };
                }
                if command == SemanticCommandId::WorktreeForceRemove {
                    return ApplicationTransition::Continue;
                }
                let checkout = match picker.choice() {
                    Some(WorktreeChoice::Checkout(c)) => Some(c.association),
                    _ => None,
                };
                if let Some(checkout) = checkout
                    && checkout.kind == crate::protocol::CheckoutKind::Linked
                    && let Some(repository) = picker
                        .context
                        .as_ref()
                        .and_then(|c| c.workspace.repository.as_deref().cloned())
                {
                    picker.loading = true;
                    picker.removal_request = Some(request_id);
                    picker.error = None;
                    return ApplicationTransition::PreviewCheckoutRemoval {
                        request_id,
                        target: Box::new(crate::protocol::CheckoutRemovalTarget {
                            repository,
                            checkout,
                        }),
                    };
                }
                picker.error =
                    Some("Main checkout cannot be removed. Select a linked Worktree".into());
            }
            SemanticCommandId::WorktreeList => {
                if self.state.session_reference.is_some() {
                    self.state.submission_error =
                        Some("Start a new Session before choosing a Worktree".to_owned());
                    return ApplicationTransition::Continue;
                }
                self.state.worktree_picker.open();
                self.state.command_mode = CommandMode::Composer;
                let request = self.current_workspace_request();
                return self.resolve_worktree(WorkspaceResolutionSurface::WorktreeList, request);
            }
            SemanticCommandId::WorktreePrevious => self.state.worktree_picker.move_by(-1),
            SemanticCommandId::WorktreeNext => self.state.worktree_picker.move_by(1),
            SemanticCommandId::WorktreeClose => {
                // Confirmation already sent is an operation in progress, not
                // a cancellable preview. Keep its eventual result visible.
                if self.state.worktree_picker.removal.is_some()
                    && self.state.worktree_picker.loading
                {
                    return ApplicationTransition::Continue;
                }
                if self.state.worktree_picker.removal.take().is_some()
                    || self.state.worktree_picker.removal_request.take().is_some()
                {
                    self.state.worktree_picker.removal_request = None;
                    self.state.worktree_picker.loading = false;
                    self.state.worktree_picker.error = None;
                    return ApplicationTransition::Continue;
                }
                self.state.worktree_picker.close();
                for surface in [
                    WorkspaceResolutionSurface::WorktreeList,
                    WorkspaceResolutionSurface::WorktreeSelection,
                ] {
                    if self.state.cancel_workspace_resolution(surface) {
                        return ApplicationTransition::CancelWorkspaceResolution(surface);
                    }
                }
            }
            SemanticCommandId::WorktreeSelect => {
                if self.state.worktree_picker.removal.is_some() {
                    return self.handle_worktree_command(SemanticCommandId::WorktreeRemove);
                }
                match self.state.worktree_picker.choice() {
                    Some(WorktreeChoice::New) => {
                        let capability = self
                            .state
                            .worktree_picker
                            .context
                            .as_ref()
                            .and_then(|c| c.workspace.repository.as_ref())
                            .map(|r| &r.capabilities.create_checkout);
                        if let Some(crate::protocol::SourceControlCapability::Unsupported {
                            reason,
                        }) = capability
                        {
                            self.state.worktree_picker.fail(reason.clone());
                            return ApplicationTransition::Continue;
                        }
                        if capability.is_none() {
                            self.state
                                .worktree_picker
                                .fail("A Repository is required".to_owned());
                            return ApplicationTransition::Continue;
                        }
                        let Some(selection) = &self.state.landing_agent_selection else {
                            self.state
                                .worktree_picker
                                .fail("Choose an Agent first".to_owned());
                            return ApplicationTransition::Continue;
                        };
                        if self.state.new_worktree.is_none() {
                            self.state.new_worktree =
                                Some(crate::protocol::PrepareCheckoutRequest {
                                    id: Default::default(),
                                    source: crate::protocol::ExecutionDirectory {
                                        path: self
                                            .state
                                            .execution_directory
                                            .clone()
                                            .unwrap_or_else(|| self.state.workspace.path.clone()),
                                    },
                                    prompt: Default::default(),
                                    provider: selection.provider.clone(),
                                });
                        }
                        self.state.worktree_picker.close();
                    }
                    Some(WorktreeChoice::Checkout(checkout)) => {
                        // Choosing the Worktree the next Session already
                        // stands in is saying to stay put: there is nothing to
                        // resolve, and a pending intention to make another one
                        // is what the reader has just taken back.
                        if self.state.worktree_picker.current_checkout()
                            == Some(&checkout.association.id)
                        {
                            self.state.cancel_worktree_intent();
                            self.state.worktree_picker.close();
                            return ApplicationTransition::Continue;
                        }
                        if let crate::protocol::SourceControlAvailability::Unavailable { reason } =
                            &checkout.availability
                        {
                            self.state.worktree_picker.fail(reason.clone());
                            return ApplicationTransition::Continue;
                        }
                        let request = ResolveWorkspaceRequest {
                            checkout_id: Some(checkout.association.id),
                            remembered_execution_directory: None,
                            workspace_id: Some(self.state.workspace.id.clone()),
                            base: None,
                            path: self.state.workspace.path.clone(),
                        };
                        self.state.worktree_picker.loading = true;
                        return self.resolve_worktree(
                            WorkspaceResolutionSurface::WorktreeSelection,
                            request,
                        );
                    }
                    None => {}
                }
            }
            _ => {}
        }
        ApplicationTransition::Continue
    }

    fn resolve_worktree(
        &mut self,
        surface: WorkspaceResolutionSurface,
        request: ResolveWorkspaceRequest,
    ) -> ApplicationTransition {
        let request_id = self.state.begin_workspace_resolution(surface);
        ApplicationTransition::ResolveWorkspace {
            outlook: self.state.outlook.clone(),
            surface,
            request_id,
            request,
        }
    }

    pub(super) fn current_workspace_request(&self) -> ResolveWorkspaceRequest {
        let known = self
            .state
            .remembered_execution_directories
            .contains_key(&(self.state.outlook.clone(), self.state.workspace.id.clone()));
        ResolveWorkspaceRequest {
            checkout_id: None,
            remembered_execution_directory: known
                .then(|| self.state.execution_directory.clone())
                .flatten()
                .map(|path| crate::protocol::ExecutionDirectory { path }),
            workspace_id: known.then(|| self.state.workspace.id.clone()),
            base: None,
            path: if known {
                self.state.workspace.path.clone()
            } else {
                self.state
                    .execution_directory
                    .clone()
                    .unwrap_or_else(|| self.state.workspace.path.clone())
            },
        }
    }

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
                            checkout_id: None,
                            remembered_execution_directory: self
                                .state
                                .remembered_execution_directories
                                .get(&(self.state.outlook.clone(), workspace.id.clone()))
                                .and_then(|remembered| remembered.directory.clone())
                                .map(|path| crate::protocol::ExecutionDirectory { path }),
                            workspace_id: Some(workspace.id),
                            base: None,
                            path: workspace.path,
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
                if let Some(session_id) = self.state.selected_working_subagent()
                    && self.state.subagent_stop_offered(session_id)
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
                let Some(target) = self.state.session_picker.begin_attach() else {
                    return ApplicationTransition::Continue;
                };
                if target.origin == self.state.outlook {
                    self.state.session_picker.close();
                    self.state.open_session_route(target.clone());
                    return ApplicationTransition::ViewAndAttachSession(target);
                }
                let Some((workspace, execution_directory)) =
                    self.state.session_picker.context_of(&target)
                else {
                    return ApplicationTransition::Continue;
                };
                self.state
                    .turn_outlook_for_session(target.clone(), workspace, execution_directory);
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
                    // attach follows it.
                    SidebarActivation::Attach {
                        session,
                        workspace,
                        execution_directory,
                    } => {
                        if session.origin == self.state.outlook {
                            self.state.open_session_route(session.clone());
                            ApplicationTransition::ViewAndAttachSession(session)
                        } else {
                            self.state.turn_outlook_for_session(
                                session.clone(),
                                workspace,
                                execution_directory,
                            );
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

    fn handle_sidebar_width_command(
        &mut self,
        command: SemanticCommandId,
    ) -> ApplicationTransition {
        match command {
            SemanticCommandId::SidebarWiden => self.state.sidebar.column_mut().widen(),
            SemanticCommandId::SidebarNarrow => self.state.sidebar.column_mut().narrow(),
            SemanticCommandId::SidebarWidthSet { columns } => {
                self.state.sidebar.column_mut().set_width(columns);
            }
            SemanticCommandId::SidebarWidthReset => {
                let initial_width = self.state.settings.sidebar.initial_width;
                self.state.sidebar.column_mut().set_width(initial_width);
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

    /// Handles the Workspace Picker row menu's own commands, routed here only
    /// while it is open: the menu has exactly one item — Choose icon, the
    /// only reason the menu ever opens at all — so Enter and a click both ask
    /// for the very same invocation the menu already knows how to build.
    fn handle_workspace_picker_menu_command(
        &mut self,
        command: SemanticCommandId,
    ) -> Result<ApplicationTransition> {
        if !self.state.workspace_picker.menu_is_open() {
            return Ok(ApplicationTransition::Continue);
        }
        match command {
            SemanticCommandId::WorkspacePickerMenuClose => {
                self.state.workspace_picker.close_menu();
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::WorkspacePickerMenuSelect => self.activate_workspace_picker_menu(),
            _ => Ok(ApplicationTransition::Continue),
        }
    }

    /// Acts on the Workspace Picker row menu's one item and closes it,
    /// whether asked for by Enter or by a press inside the menu's own box.
    fn activate_workspace_picker_menu(&mut self) -> Result<ApplicationTransition> {
        let invocation = self.state.workspace_picker.activate_menu();
        self.state.workspace_picker.close_menu();
        match invocation {
            Some(invocation) => self.invoke_semantic(invocation),
            None => Ok(ApplicationTransition::Continue),
        }
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
        // Worktree preparation precedes Prompt admission. Confirming Escape in
        // this phase cancels only this client attempt: Git may safely finish in
        // its spawned task, but its late result no longer has a matching
        // submission and therefore cannot dispatch Session creation.
        if self.state.provisional.as_ref().is_some_and(|claim| {
            claim.phase == ProvisionalSessionPhase::PreparingWorktree
                && matches!(
                    self.state.command_mode,
                    CommandMode::InterruptConfirmation { .. }
                )
        }) {
            self.state.command_mode = CommandMode::Composer;
            let claim = self
                .state
                .release_claim()
                .expect("the preparing provisional Session was just observed");
            if self
                .state
                .pending_submission
                .as_ref()
                .is_some_and(|pending| {
                    pending.prompt.id == claim.prompt.id
                        && pending.target == SubmissionTarget::CreateSession
                })
            {
                self.state.pending_submission = None;
            }
            self.state
                .composers
                .return_prompt(ComposerKey::Landing, &claim.prompt);
            self.state.submission_error = None;
            self.state.transcript_generation = self.state.transcript_generation.wrapping_add(1);
            self.state.sync_composer_completion();
            return ApplicationTransition::Continue;
        }
        // A claim has no Session to interrupt yet. The intent is recorded and
        // travels with the Session's arrival, so the reader's second Escape
        // means what it said even though nothing could be sent when they made
        // it.
        if self.state.provisional.is_some() {
            if matches!(
                self.state.command_mode,
                CommandMode::InterruptConfirmation { .. }
            ) {
                self.state.command_mode = CommandMode::Composer;
                if let Some(claim) = self.state.provisional.as_mut() {
                    claim.hold_interrupt();
                }
            } else {
                self.request_interrupt();
            }
            return ApplicationTransition::Continue;
        }
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
        let target = self.state.interrupt_target();
        self.state.command_mode = CommandMode::Composer;
        if let Some(turn_id) = turn_id {
            self.state.keep_interrupted_turn_open(turn_id);
        } else if target == Some(InterruptTarget::Watches) {
            self.state.request_watch_stop(&session);
        } else {
            self.state.await_withdrawal(&session, None);
        }
        ApplicationTransition::InterruptSession { session }
    }

    fn request_interrupt(&mut self) {
        // The gesture reaches whatever the Session is Working for: the active
        // Turn, the Subagents that outlived it, or the Prompt it has admitted
        // and not delivered. With none of them there is nothing to stop and the
        // key stays inert.
        // A claim that has already taken the reader's confirmation is waiting
        // on the Session to send it to; asking again would arm a gesture that
        // has nothing left to say.
        if self
            .state
            .provisional
            .as_ref()
            .is_some_and(ProvisionalSession::interrupt_intent)
        {
            return;
        }
        let Some(target) = self.state.interrupt_target() else {
            return;
        };
        self.state.command_mode = CommandMode::InterruptConfirmation {
            turn_id: match target {
                InterruptTarget::Turn(turn_id) => Some(turn_id),
                InterruptTarget::Subagents | InterruptTarget::Prompt | InterruptTarget::Watches => {
                    None
                }
            },
            armed_at: self.state.presentation_clock.now(),
        };
    }

    /// Asks for the refused Session again.
    ///
    /// An empty composer means the very Prompt that was refused — the one place
    /// an empty submit is not an error — and anything typed replaces it: a new
    /// Prompt, drawn in the old one's place. Either way the request context the
    /// reader submitted under is reused, so a retry cannot quietly land
    /// somewhere else, and the refusal gives way to the Working Indicator.
    fn retry_provisional_session(&mut self) -> ApplicationTransition {
        let key = ComposerKey::Landing;
        self.state.sync_composer_completion();
        let typed = !self.state.composers.text(key.clone()).trim().is_empty();
        if typed && let Some(error) = self.state.composers.skill_issue(key.clone()) {
            self.state.submission_error = Some(error.to_owned());
            self.state.composer_completion.dismiss_active();
            return ApplicationTransition::Continue;
        }
        let prompt = if typed {
            let prompt = self.state.composers.begin_submission(key.clone());
            self.state.sync_composer_completion();
            prompt
        } else {
            self.state
                .provisional
                .as_ref()
                .expect("a retry answers the claim that was refused")
                .prompt
                .clone()
        };
        let claim = self
            .state
            .provisional
            .as_mut()
            .expect("a retry answers the claim that was refused");
        claim.prompt = prompt.clone();
        claim.standing = ClaimStanding::Claimed {
            interrupt_intent: false,
        };
        let claimed = SessionReference::new(self.state.outlook.clone(), claim.session_id);
        self.state
            .aside
            .stand_in_for(claimed, prompt.text.trim().to_owned(), true);
        self.state.transcript_generation = self.state.transcript_generation.wrapping_add(1);
        self.state.failed_submissions.remove(&prompt.id);
        self.state.submission_error = None;
        self.state.pending_submission = Some(PendingSubmission {
            source: key,
            target: SubmissionTarget::CreateSession,
            prompt: prompt.clone(),
            preparation_attempt: None,
        });
        self.state.creation_transition(prompt)
    }

    /// Takes the Session the Server made in place of the claim the client drew
    /// for it, and answers with the interrupt the reader confirmed while there
    /// was nothing yet to send it to.
    ///
    /// A claim the reader has since left is not replaced: a newer route stands,
    /// and no late answer pulls them back to a Session they stopped waiting on.
    fn take_created_session(&mut self, snapshot: SessionSnapshot) -> Result<ApplicationTransition> {
        // A claim is answered by the Session carrying its Prompt and by no
        // other: a creation that names a different one is not this claim's
        // answer, so the claim stands and the interrupt it holds waits for the
        // Session it was meant for.
        let answers_claim = self.state.provisional.as_ref().is_some_and(|claim| {
            snapshot
                .prompts
                .iter()
                .any(|prompt| prompt.id == claim.prompt.id)
        });
        if self.state.provisional.is_some() && !answers_claim {
            return Ok(ApplicationTransition::Continue);
        }
        let claim = self.state.release_claim();
        // Without a claim, the only creation this client could have walked away
        // from is the submission still waiting on an answer. A Session arriving
        // for anything else — a client that never drew a claim for it — is
        // opened as it always was.
        if claim.is_none() && self.state.detach_creation(&snapshot) {
            return Ok(ApplicationTransition::Continue);
        }
        self.state.new_worktree = None;
        self.state.session_events_blocked = false;
        let title = snapshot.title.clone();
        let answered = claim.is_some();
        self.state.apply_session(SessionEvent::snapshot(snapshot))?;
        // The Session answering a claim carries on the Aside entry the claim
        // stood in, until the per-tree subscription's tree replaces it in
        // place, so the Aside neither blanks nor says Loading between them.
        if answered && let Some(session) = self.state.session_reference.clone() {
            self.state.aside.stand_in_for(session.clone(), title, true);
            // So does it carry on the thumbnails the claim held.
            self.state
                .attachment_previews
                .carry_to(PreviewScope::Session(session));
        }
        if let Some(claim) = claim.filter(ProvisionalSession::interrupt_intent)
            && let Some(session) = self.state.session_reference.clone()
        {
            self.state.await_withdrawal(&session, Some(claim.prompt.id));
            return Ok(ApplicationTransition::InterruptSession { session });
        }
        Ok(ApplicationTransition::Continue)
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
        if self
            .state
            .provisional
            .as_ref()
            .is_some_and(ProvisionalSession::refused)
        {
            return self.retry_provisional_session();
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
        if let Some(error) = self.state.composers.skill_issue(key.clone())
            && self.state.new_worktree.as_ref().is_none_or(|intent| {
                self.state.execution_directory.as_ref() == Some(&intent.source.path)
            })
        {
            self.state.submission_error = Some(error.to_owned());
            self.state.composer_completion.dismiss_active();
            return ApplicationTransition::Continue;
        }
        if !matches!(key, ComposerKey::Session(_))
            && self.state.new_worktree.is_none()
            && self.state.execution_directory.is_none()
        {
            self.state.submission_error =
                Some("Choose a working copy before starting a Session".to_owned());
            return ApplicationTransition::Continue;
        }
        if !matches!(key, ComposerKey::Session(_))
            && let crate::protocol::ExecutionDirectoryStatus::Unavailable { reason } =
                &self.state.execution_status
        {
            self.state.submission_error =
                Some(format!("Execution Directory unavailable: {reason}"));
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
                preparation_attempt: None,
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
            preparation_attempt: None,
        });
        self.state.begin_provisional_session(prompt)
    }

    /// Asks for the host clipboard to be read into the composer that has the
    /// keys. Anywhere else the command does nothing, as Ctrl+V always has.
    fn paste_from_clipboard(&mut self) -> ApplicationTransition {
        if self.state.overlay_owns_input()
            || self.state.reconnect_overlay_visible()
            || self.state.open_subagent_parent().is_some()
            || !matches!(self.state.command_mode, CommandMode::Composer)
        {
            return ApplicationTransition::Continue;
        }
        let draft = self.state.composer_key();
        self.state.composers.keep_draft(draft.clone());
        ApplicationTransition::ReadClipboard(self.state.clipboard_pastes.begin(draft))
    }

    /// Answers a paste's read of the clipboard in the draft it was asked for:
    /// text pastes there as a bracketed paste would, and an image goes on to
    /// be uploaded unless it could never be attached.
    fn receive_clipboard_read(
        &mut self,
        paste: PasteId,
        read: ClipboardRead,
    ) -> ApplicationTransition {
        let Some(draft) = self.state.clipboard_pastes.draft(paste).cloned() else {
            return ApplicationTransition::Continue;
        };
        if !self.state.composers.has_draft(&draft) {
            self.state.clipboard_pastes.finish(paste);
            return ApplicationTransition::Continue;
        }
        let png = match read {
            ClipboardRead::Image { png } => png,
            ClipboardRead::Text(text) => {
                self.state.clipboard_pastes.finish(paste);
                if !text.is_empty() {
                    self.state.paste_into_draft(draft, &text);
                }
                return ApplicationTransition::Continue;
            }
            ClipboardRead::Empty => {
                self.state.clipboard_pastes.finish(paste);
                return ApplicationTransition::Continue;
            }
            ClipboardRead::Unsupported { format } => {
                self.state.clipboard_pastes.finish(paste);
                self.paste_failed(paste, super::clipboard_paste::unsupported(&format));
                return ApplicationTransition::Continue;
            }
            ClipboardRead::Failed { reason } => {
                self.state.clipboard_pastes.finish(paste);
                self.paste_failed(paste, super::clipboard_paste::unreadable(&reason));
                return ApplicationTransition::Continue;
            }
        };
        if let Some(refusal) = super::clipboard_paste::refuse_image(
            &png,
            self.state.composers.attachment_count(draft.clone()),
            self.state.clipboard_pastes.uploads_into(&draft),
        ) {
            self.state.clipboard_pastes.finish(paste);
            self.paste_failed(paste, refusal);
            return ApplicationTransition::Continue;
        }
        self.state.clipboard_pastes.begin_upload(paste);
        let origin = match &draft {
            ComposerKey::Session(session) => session.origin.clone(),
            ComposerKey::Landing => self.state.outlook.clone(),
        };
        ApplicationTransition::UploadAttachment { paste, origin, png }
    }

    /// Writes an uploaded image's label into the draft it was pasted into.
    fn receive_uploaded_attachment(
        &mut self,
        paste: PasteId,
        descriptor: crate::protocol::AttachmentDescriptor,
    ) {
        let Some(draft) = self.state.clipboard_pastes.finish(paste) else {
            return;
        };
        if !self.state.composers.has_draft(&draft) {
            return;
        }
        if let Some(refusal) = super::clipboard_paste::refuse_binding(
            self.state.composers.attachment_count(draft.clone()),
        ) {
            self.paste_failed(paste, refusal);
            return;
        }
        self.state
            .edit_draft_without_completion(draft, |composers, key| {
                composers.insert_attachment(key, descriptor);
            });
    }

    /// Answers a check of a recall's Attachments in the draft it was asked
    /// for: each label the recall bound whose Attachment the Server no longer
    /// stores is demoted to plain text, and a Notice names them. A check that
    /// could not be made leaves the draft bound, for admission to judge; one
    /// whose labels are gone from the draft since changes nothing.
    fn receive_attachment_check(
        &mut self,
        check: AttachmentCheckId,
        result: std::result::Result<Vec<AttachmentId>, String>,
    ) {
        let Some(pending) = self.state.attachment_checks.finish(check) else {
            return;
        };
        let missing = match result {
            Ok(missing) => missing,
            Err(reason) => {
                tracing::warn!(
                    ?check,
                    %reason,
                    "could not check whether a recalled Prompt's Attachments are still stored; \
                     their labels stay bound"
                );
                return;
            }
        };
        if missing.is_empty() || !self.state.composers.has_draft(&pending.draft) {
            return;
        }
        let mut demoted = Vec::new();
        self.state
            .edit_draft_without_completion(pending.draft.clone(), |composers, key| {
                demoted =
                    composers.demote_attachments(&key, |bound| pending.demotes(bound, &missing));
            });
        self.state
            .report_demoted_attachments(&demoted, AttachmentDemotion::NoLongerStored);
    }

    /// Says why a paste inserted nothing, and keeps the whole of it in the Log.
    fn paste_failed(&mut self, paste: PasteId, (failure, summary): (PasteFailure, String)) {
        tracing::warn!(?paste, ?failure, reason = %summary, "paste from the clipboard inserted nothing");
        self.state
            .application_notice
            .receive_paste_failure(paste, failure, summary);
    }

    /// One Origin's loss has outlived its grace, so it becomes the reader's to
    /// see: a banner above the composer for a Remote, and the whole-frame
    /// modal for this machine's own Server.
    fn elapse_reconnect_grace(&mut self, outlook: &Outlook) -> ApplicationTransition {
        self.state.present_recovery(outlook);
        if outlook == &Outlook::Local && self.state.reconnect_overlay_visible() {
            // The modal owns input, including the release that would end a drag.
            self.state.left_press = None;
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
                let resolve_launch = matches!(&event, ManagedEvent::Connected(_))
                    && !self.state.initial_context_resolved
                    && self.state.outlook == Outlook::Local;
                let had_session = self.state.session.is_some();
                self.state.apply(event);
                self.state.reconcile_command_mode();
                if resolve_launch {
                    self.state.initial_context_resolved = true;
                    let request_id = self
                        .state
                        .begin_workspace_resolution(WorkspaceResolutionSurface::Outlook);
                    return Ok(ApplicationTransition::ResolveWorkspace {
                        outlook: Outlook::Local,
                        surface: WorkspaceResolutionSurface::Outlook,
                        request_id,
                        request: ResolveWorkspaceRequest {
                            checkout_id: None,
                            remembered_execution_directory: None,
                            workspace_id: None,
                            base: None,
                            path: self
                                .state
                                .execution_directory
                                .clone()
                                .unwrap_or_else(|| self.state.workspace.path.clone()),
                        },
                    });
                }
                if had_session && self.state.session.is_none() {
                    Ok(ApplicationTransition::SessionEnded)
                } else {
                    Ok(self.take_session_listing_transition())
                }
            }
        }
    }

    /// The one path a Pairing's end takes, whether the Remote revoked it or
    /// this user removed it: an Outlook turned toward it leaves first, so the
    /// turn's own bookkeeping cannot re-remember what is about to go, then
    /// its rows leave every listing and the memories held for that Outlook go
    /// with them.
    fn end_pairing(&mut self, outlook: &Outlook, message: &str) -> ApplicationTransition {
        let was_current = *outlook == self.state.outlook;
        if was_current {
            self.leave_current_remote(outlook, message);
        }
        self.state.end_pairing_memories(outlook);
        self.state.sync_composer_completion();
        if was_current {
            return ApplicationTransition::TurnOutlook {
                outlook: Outlook::Local,
                catalog_origins: self.state.catalog_origins(),
            };
        }
        ApplicationTransition::ReconcileCatalogOrigins {
            catalog_origins: self.state.catalog_origins(),
            requests: Vec::new(),
        }
    }

    /// Turns the Outlook back to this Client's own Server because the Remote
    /// it was turned toward can no longer be worked in: whatever was about to
    /// be submitted there fails with `message`, and an open Session's composer
    /// and interaction state come home rather than stranding.
    fn leave_current_remote(&mut self, outlook: &Outlook, message: &str) {
        let pending_prompt = self
            .state
            .pending_submission
            .as_ref()
            .filter(|pending| match &pending.target {
                SubmissionTarget::CreateSession => true,
                SubmissionTarget::AdmitPrompt(session, _) => session.origin == *outlook,
            })
            .map(|pending| pending.prompt.id);
        if let Some(prompt_id) = pending_prompt {
            self.state
                .fail_pending_submission(prompt_id, message.to_owned());
        }
        if let Some(reference) = self.state.session_reference.clone() {
            self.state
                .composers
                .recover_session_to_landing(reference.clone());
            self.state.session_interactions.remove(&reference);
        }
        self.state.turn_outlook(Outlook::Local);
    }

    fn handle_origin_catalog(
        &mut self,
        outlook: Outlook,
        event: ManagedEvent,
    ) -> Result<ApplicationTransition> {
        if let ManagedEvent::RemoteFailed { status, message } = &event {
            let is_current = outlook == self.state.outlook;
            let Some(name) = outlook.remote_name().map(str::to_owned) else {
                return Ok(ApplicationTransition::Continue);
            };
            if is_current {
                self.state.connect_overlay.remote_failed(&name, *status);
            }
            if *status == crate::protocol::RemoteStatus::Revoked {
                let transition = self.end_pairing(&outlook, message);
                if is_current {
                    self.state.settle_remote_failure(message.clone());
                }
                return Ok(transition);
            }
            self.state.drop_origin_rows(&outlook);
            if !is_current {
                return Ok(ApplicationTransition::ReconcileCatalogOrigins {
                    catalog_origins: self.state.catalog_origins(),
                    requests: Vec::new(),
                });
            }
            self.leave_current_remote(&outlook, message);
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
            || (matches!(event, ManagedEvent::ModelCatalog(_))
                && (listed_by_sidebar || listed_by_picker))
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
            self.state.sidebar.finish_attach();
        }
        Ok(if views_root {
            ApplicationTransition::ViewSession(reference)
        } else {
            ApplicationTransition::Continue
        })
    }

    fn fail_open_session_attach(&mut self, error: String) -> ApplicationTransition {
        self.state.fail_opening_session(error);
        self.state.sidebar.fail_attach();
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

    /// The Transcript cell under a screen position while the Transcript can
    /// take the pointer: no overlay owns input and the position lies on a
    /// projected row.
    fn transcript_cell(&self, position: Position) -> Option<SelectionCell> {
        if self.state.overlay_owns_input()
            || self.state.reconnect_overlay_visible()
            || self.state.active_selection_overlay_area().is_some()
        {
            return None;
        }
        let interaction = self
            .state
            .session_reference
            .as_ref()
            .and_then(|session| self.state.session_interaction(session))?;
        let viewport = interaction.viewport.borrow();
        let viewport = viewport.as_ref()?;
        // The cell immediately past the edge's grab zone belongs to ordinary
        // main-view Text Selection even though the Content Column's layout
        // padding leaves its first painted cell one column farther right.
        // Map only that boundary cell; the rest of a centered gutter remains
        // inert as ADR 0012 requires.
        let position = if self
            .state
            .sidebar
            .column()
            .borders_edge_on_main_side(position)
        {
            Position::new(viewport.content_left, position.y)
        } else if self
            .state
            .aside
            .column()
            .borders_edge_on_main_side(position)
        {
            Position::new(
                viewport
                    .content_left
                    .saturating_add(viewport.content_width.saturating_sub(1)),
                position.y,
            )
        } else {
            position
        };
        let row = viewport.transcript_row(position)?;
        (row < self.state.transcript_cache.row_count()).then_some(SelectionCell {
            row,
            column: usize::from(position.x - viewport.content_left),
        })
    }

    fn transcript_hyperlink(&self, position: Position) -> Option<String> {
        let cell = self.transcript_cell(position)?;
        self.state
            .transcript_cache
            .hyperlink_at(cell.row, cell.column)
    }

    /// Marks the word or whole Line under a Transcript cell.
    fn select_transcript_text(&mut self, position: Position, granularity: SelectionGranularity) {
        let Some(cell) = self.transcript_cell(position) else {
            return;
        };
        let bounds = match granularity {
            SelectionGranularity::Word => self
                .state
                .transcript_cache
                .word_cells(cell.row, cell.column),
            SelectionGranularity::Line => self.state.transcript_cache.line_cells(cell.row),
            SelectionGranularity::Cell => None,
        };
        let Some((anchor, focus)) = bounds else {
            return;
        };
        self.state.composers.clear_selections();
        self.state.text_selection.set(Some(TextSelection {
            surface: SelectionSurface::Transcript,
            anchor,
            focus,
            epoch: self.state.transcript_cache.selection_epoch(),
            granularity,
        }));
    }

    /// Distance beyond the Transcript determines rows per presentation tick.
    fn transcript_drag_scroll(&self) -> Option<(ScrollDirection, usize)> {
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
                ScrollDirection::Up,
                usize::from(viewport.content_top - press.pointer.y),
            ))
        } else if press.pointer.y >= bottom {
            Some((
                ScrollDirection::Down,
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
        if let Some(anchor) = press.composer_anchor.clone() {
            if press.dragged
                && let Some(frame) = self.state.composers.selection_frame()
                && let Some(cell) = frame.cell(position)
                && let Some(focus) = frame.cell_range(cell)
            {
                let (anchor, focus) = if focus.start < anchor.start {
                    (anchor.end, focus.start)
                } else if focus.start > anchor.start {
                    (anchor.start, focus.end)
                } else {
                    (anchor.start, anchor.start)
                };
                self.state.text_selection.set(None);
                self.state
                    .composers
                    .select(self.state.composer_key(), anchor, focus);
            }
            return;
        }
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
        // A surface that names its own subject — a Sidebar row, a picker row,
        // a menu item — reaches a semantic command without passing the
        // CommandId choke point, so the same one question is asked of the
        // invocation it built.
        if let Some(origin) = self.semantic_origin(command, &invocation.subject)
            && self.refuse_if_unreachable(&origin)
        {
            return Ok(ApplicationTransition::Continue);
        }
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
            // Reserved for opening an Attachment at full size, which is a later
            // change: a thumbnail's click target reaches it, and it does
            // nothing yet.
            SemanticCommandId::AttachmentOpen => Ok(ApplicationTransition::Continue),
            SemanticCommandId::HyperlinkOpen => {
                let SemanticSubject::Hyperlink(target) = invocation.subject else {
                    return Ok(ApplicationTransition::Continue);
                };
                Ok(super::clipboard::safe_hyperlink_target(&target)
                    .map_or(ApplicationTransition::Continue, |target| {
                        ApplicationTransition::OpenHyperlink(target.to_owned())
                    }))
            }
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
                if self.state.sidebar.column().edge_is_held() {
                    let SemanticSubject::ScreenPosition(position) = &invocation.subject else {
                        return Ok(ApplicationTransition::Continue);
                    };
                    // The Sidebar column includes its rule, so a choice one
                    // wider than the pointer's zero-based screen column puts
                    // that rule directly under the pointer. The Sidebar
                    // resolves that choice under the last frame's floors,
                    // then the semantic setter owns the view-state mutation.
                    let Some(columns) = self.state.sidebar.column().width_at_held_edge(*position)
                    else {
                        return Ok(ApplicationTransition::Continue);
                    };
                    return self.invoke_semantic(SemanticCommandId::SidebarWidthSet { columns });
                }
                if self.state.aside.column().edge_is_held() {
                    let SemanticSubject::ScreenPosition(position) = &invocation.subject else {
                        return Ok(ApplicationTransition::Continue);
                    };
                    let Some(columns) = self.state.aside.column().width_at_held_edge(*position)
                    else {
                        return Ok(ApplicationTransition::Continue);
                    };
                    return self.invoke_semantic(SemanticCommandId::AsideWidthSet { columns });
                }
                if let SemanticSubject::ScreenPosition(position) = invocation.subject {
                    self.update_text_selection_drag(position, 1);
                }
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::TextSelectionWord | SemanticCommandId::TextSelectionLine => {
                if let SemanticSubject::ScreenPosition(position) = invocation.subject {
                    let granularity = if invocation.id == SemanticCommandId::TextSelectionWord {
                        SelectionGranularity::Word
                    } else {
                        SelectionGranularity::Line
                    };
                    self.select_transcript_text(position, granularity);
                }
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::TextSelectionClear => {
                self.state.text_selection.set(None);
                self.state.composers.clear_selections();
                if let Some(press) = &mut self.state.left_press {
                    press.selection_anchor = None;
                    press.composer_anchor = None;
                }
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::TextSelectionCopy => {
                if let Some(text) = self
                    .state
                    .composers
                    .copy_selection(self.state.composer_key())
                {
                    return Ok(ApplicationTransition::CopyToClipboard(text.into()));
                }
                Ok(self
                    .state
                    .text_selection
                    .get()
                    .and_then(|selection| match selection.surface {
                        SelectionSurface::Transcript => {
                            self.state.transcript_cache.copy_selection(selection)
                        }
                        _ => self
                            .state
                            .selection_frames
                            .borrow()
                            .iter()
                            .find(|frame| {
                                frame.surface == selection.surface
                                    && frame.epoch() == selection.epoch
                            })
                            .and_then(|frame| frame.copy(selection))
                            .map(Into::into),
                    })
                    .map_or(
                        ApplicationTransition::Continue,
                        ApplicationTransition::CopyToClipboard,
                    ))
            }
            SemanticCommandId::ComposerPlaceCursor => {
                if !self.state.overlay_owns_input()
                    && !self.state.reconnect_overlay_visible()
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
            SemanticCommandId::ComposerClipboardPaste => Ok(self.paste_from_clipboard()),
            SemanticCommandId::ApplicationExit => Ok(ApplicationTransition::Exit),
            SemanticCommandId::ConnectOpen => {
                self.state.connect_overlay.open();
                self.state.command_mode = CommandMode::Composer;
                Ok(ApplicationTransition::BeginConnecting)
            }
            SemanticCommandId::PairOpen => {
                self.state.connect_overlay.open_invite_entry();
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
            SemanticCommandId::ConnectRemoveRemote => {
                Ok(self.state.connect_overlay.remove_remote().map_or(
                    ApplicationTransition::Continue,
                    ApplicationTransition::RemoveRemote,
                ))
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
            SemanticCommandId::ApprovalPostureOpen => {
                if let Some(posture) = self.state.session.as_ref().and_then(|session| {
                    (!session.snapshot().session.is_subagent()).then_some(())?;
                    session
                        .snapshot()
                        .session
                        .approval_posture
                        .as_ref()
                        .map(|posture| posture.value)
                }) {
                    self.state.approval_posture_picker.open(posture);
                    self.state.command_mode = CommandMode::Composer;
                }
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::ApprovalPostureCycle => {
                let request = self.state.session.as_ref().and_then(|session| {
                    (!session.snapshot().session.is_subagent()).then_some(())?;
                    session
                        .snapshot()
                        .session
                        .approval_posture
                        .as_ref()
                        .map(|posture| crate::protocol::UpdateApprovalPostureRequest {
                            posture: Some(posture.value.cycle_primary()),
                        })
                });
                Ok(self.approval_posture_transition(request))
            }
            SemanticCommandId::ApprovalPosturePrevious => {
                self.state.approval_posture_picker.previous();
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::ApprovalPostureNext => {
                self.state.approval_posture_picker.next();
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::ApprovalPostureSelect => {
                let request = self.state.approval_posture_picker.choose();
                Ok(self.approval_posture_transition(request))
            }
            SemanticCommandId::ApprovalPostureClose => {
                self.state.approval_posture_picker.close();
                Ok(ApplicationTransition::Continue)
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
            SemanticCommandId::ApprovalOpen => {
                let approval = self
                    .state
                    .pending_approvals()
                    .find_map(|activity| match activity {
                        Activity::Approval { approval, .. }
                            if match &invocation.subject {
                                SemanticSubject::Approval(id) => approval.id == *id,
                                _ => true,
                            } =>
                        {
                            Some(approval.id)
                        }
                        _ => None,
                    });
                if let (Some(session), Some(id)) = (self.state.session_reference.clone(), approval)
                {
                    self.state.questionnaires.hide();
                    self.state.recall_dismissed_interventions();
                    self.state.approvals.open(session, id);
                }
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::ApprovalHide => {
                // A Decision already sent is left to settle: Esc neither
                // hides the panel nor dismisses anything while it is in
                // flight, so nothing can be delivered twice.
                if !self.state.approvals.awaits_confirmation() {
                    self.state.approvals.hide();
                    self.state.dismiss_interventions();
                }
                Ok(ApplicationTransition::Continue)
            }
            command @ (SemanticCommandId::ApprovalChoicePrevious
            | SemanticCommandId::ApprovalChoiceNext
            | SemanticCommandId::ApprovalChoose
            | SemanticCommandId::ApprovalAccept
            | SemanticCommandId::ApprovalAcceptForSession
            | SemanticCommandId::ApprovalDecline
            | SemanticCommandId::ApprovalDeclineAndInterrupt) => {
                if self.state.open_approval().is_some()
                    && let Some((id, decision)) = self.state.approvals.command(command)
                    && let Some(session) = self.state.session_reference.clone()
                {
                    return Ok(ApplicationTransition::SubmitDecision {
                        session,
                        id,
                        decision,
                    });
                }
                Ok(ApplicationTransition::Continue)
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
                    self.state.approvals.hide();
                    self.state.recall_dismissed_interventions();
                    self.state.questionnaires.open(session, &questionnaire);
                }
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::QuestionnaireHide => {
                if !self.state.questionnaires.awaits_confirmation() {
                    self.state.questionnaires.hide();
                    self.state.dismiss_interventions();
                }
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
            SemanticCommandId::WorktreeList
            | SemanticCommandId::WorktreeRemove
            | SemanticCommandId::WorktreeForceRemove
            | SemanticCommandId::WorktreePrevious
            | SemanticCommandId::WorktreeNext
            | SemanticCommandId::WorktreeSelect
            | SemanticCommandId::WorktreeClose => Ok(self.handle_worktree_command(command)),
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
                | SemanticSubject::Approval(_)
                | SemanticSubject::Questionnaire(_)
                | SemanticSubject::Origin(_)
                | SemanticSubject::Hyperlink(_)
                | SemanticSubject::Attachment(_)
                | SemanticSubject::Workspace { .. }
                | SemanticSubject::Text(_) => ApplicationTransition::Continue,
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
                | SemanticSubject::Approval(_)
                | SemanticSubject::Questionnaire(_)
                | SemanticSubject::Origin(_)
                | SemanticSubject::Hyperlink(_)
                | SemanticSubject::Attachment(_)
                | SemanticSubject::Workspace { .. }
                | SemanticSubject::Text(_) => ApplicationTransition::Continue,
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
                    | SemanticSubject::Approval(_)
                    | SemanticSubject::Questionnaire(_)
                    | SemanticSubject::Origin(_)
                    | SemanticSubject::Hyperlink(_)
                    | SemanticSubject::Attachment(_)
                    | SemanticSubject::Workspace { .. }
                    | SemanticSubject::Text(_) => self.session_reference(),
                };
                Ok(named.map_or(ApplicationTransition::Continue, |session| {
                    ApplicationTransition::SettleSession { session, settled }
                }))
            }
            // The Session it names is the picker's target: the Sidebar row's
            // context menu and the header Icon press both build the
            // invocation with one, so the picker never has to guess which
            // Session an unlabeled press meant. Inert while Icons are off,
            // since no Icon Suru derives, stores, or draws would ever show
            // through a Catalog choice made while they are hidden.
            SemanticCommandId::SessionIconChoose => {
                self.state.command_mode = CommandMode::Composer;
                if self.state.settings().appearance.show_icons
                    && let SemanticSubject::Session(session) = invocation.subject
                {
                    self.state
                        .icon_picker
                        .open(IconPickerTarget::Session(session));
                }
                Ok(ApplicationTransition::Continue)
            }
            // The Workspace it names is the picker's target, the same way a
            // Session names it for `SessionIconChoose`: a Sidebar selector
            // entry's context menu and a Workspace Picker row's own menu both
            // build the invocation with one. Inert while Icons are off, for
            // the same reason choosing a Session's Icon is.
            SemanticCommandId::WorkspaceIconChoose => {
                self.state.command_mode = CommandMode::Composer;
                if self.state.settings().appearance.show_icons
                    && let SemanticSubject::Workspace {
                        origin,
                        workspace_id,
                    } = invocation.subject
                {
                    self.state.icon_picker.open(IconPickerTarget::Workspace {
                        origin,
                        workspace_id,
                    });
                }
                Ok(ApplicationTransition::Continue)
            }
            command @ (SemanticCommandId::IconPickerLeft
            | SemanticCommandId::IconPickerRight
            | SemanticCommandId::IconPickerUp
            | SemanticCommandId::IconPickerDown
            | SemanticCommandId::IconPickerChoose
            | SemanticCommandId::IconPickerClose
            | SemanticCommandId::IconPickerSearchInsert
            | SemanticCommandId::IconPickerSearchDelete) => {
                Ok(self.handle_icon_picker_command(command, invocation.subject))
            }
            command @ (SemanticCommandId::WorkspacePickerMenuSelect
            | SemanticCommandId::WorkspacePickerMenuClose) => {
                self.handle_workspace_picker_menu_command(command)
            }
            command @ (SemanticCommandId::SidebarPrevious
            | SemanticCommandId::SidebarNext
            | SemanticCommandId::SidebarAttach
            | SemanticCommandId::SidebarLeave) => Ok(self.handle_sidebar_command(command)),
            command @ (SemanticCommandId::SidebarWiden
            | SemanticCommandId::SidebarNarrow
            | SemanticCommandId::SidebarWidthSet { .. }
            | SemanticCommandId::SidebarWidthReset) => {
                Ok(self.handle_sidebar_width_command(command))
            }
            command @ (SemanticCommandId::SidebarMenuPrevious
            | SemanticCommandId::SidebarMenuNext
            | SemanticCommandId::SidebarMenuSelect
            | SemanticCommandId::SidebarMenuClose) => self.handle_sidebar_menu_command(command),
            command @ (SemanticCommandId::AsideWiden
            | SemanticCommandId::AsideNarrow
            | SemanticCommandId::AsideWidthSet { .. }
            | SemanticCommandId::AsideWidthReset) => {
                let column = self.state.aside.column_mut();
                match command {
                    SemanticCommandId::AsideWiden => column.widen(),
                    SemanticCommandId::AsideNarrow => column.narrow(),
                    SemanticCommandId::AsideWidthSet { columns } => column.set_width(columns),
                    _ => {
                        let initial_width = self.state.settings.aside.initial_width;
                        self.state.aside.column_mut().set_width(initial_width);
                    }
                }
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::AsideToggle => {
                // One of the two columns holds the keys at most, so an Aside
                // taking them takes them from the Sidebar.
                let present = self.state.aside_is_present();
                if self.state.aside.toggle(present) {
                    if self.state.sidebar.column().claims_keys() {
                        self.state.sidebar.hand_back_keys();
                    }
                    // Row focus begins where the reader is, each time.
                    let entries = self.aside_focus_entries();
                    self.state.aside.seed_focus(&entries);
                }
                self.state.command_mode = CommandMode::Composer;
                Ok(ApplicationTransition::Continue)
            }
            SemanticCommandId::AsideLeave => {
                self.state.aside.hand_back_keys();
                Ok(ApplicationTransition::Continue)
            }
            // Row focus answers only while the Aside holds the keys.
            command @ (SemanticCommandId::AsidePrevious | SemanticCommandId::AsideNext) => {
                if self.state.aside.column().claims_keys() {
                    let entries = self.aside_focus_entries();
                    self.state
                        .aside
                        .move_focus(&entries, command == SemanticCommandId::AsideNext);
                }
                Ok(ApplicationTransition::Continue)
            }
            // Enter opens the focused entry through the invocation its row
            // stands for; the open Session's own entry stands for none.
            SemanticCommandId::AsideOpen => {
                if !self.state.aside.column().claims_keys() {
                    return Ok(ApplicationTransition::Continue);
                }
                let entries = self.aside_focus_entries();
                match self.state.aside.focused_invocation(&entries) {
                    Some(invocation) => self.invoke_semantic(invocation),
                    None => Ok(ApplicationTransition::Continue),
                }
            }
            SemanticCommandId::SidebarToggle => {
                // Reaching a Sidebar already on screen takes nothing from it;
                // showing or hiding it gives up a path the reader had offered.
                let cancelled = self.state.sidebar.column().toggle_step() != ToggleStep::TakeKeys
                    && self
                        .state
                        .cancel_workspace_resolution(WorkspaceResolutionSurface::Sidebar);
                let open = self.state.sidebar_highlight();
                self.state.sidebar.toggle(open.as_ref());
                if self.state.sidebar.column().claims_keys() {
                    self.state.aside.hand_back_keys();
                }
                self.state.command_mode = CommandMode::Composer;
                if cancelled {
                    return Ok(ApplicationTransition::CancelWorkspaceResolution(
                        WorkspaceResolutionSurface::Sidebar,
                    ));
                }
                Ok(self.take_session_listing_transition())
            }
            SemanticCommandId::RemoteRetry => Ok(match invocation.subject {
                SemanticSubject::Origin(outlook) => self.retry_origin(outlook),
                // Naming no Origin means the one the Outlook is turned toward,
                // which is what a key press and the banner's own affordance
                // both mean by it.
                SemanticSubject::View => {
                    let outlook = self.state.outlook.clone();
                    self.retry_origin(outlook)
                }
                SemanticSubject::ScreenPosition(_)
                | SemanticSubject::ComposerCursor(_)
                | SemanticSubject::Turn(_)
                | SemanticSubject::Session(_)
                | SemanticSubject::Approval(_)
                | SemanticSubject::Questionnaire(_)
                | SemanticSubject::Hyperlink(_)
                | SemanticSubject::Attachment(_)
                | SemanticSubject::Workspace { .. }
                | SemanticSubject::Text(_) => ApplicationTransition::Continue,
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

    fn approval_posture_transition(
        &self,
        request: Option<crate::protocol::UpdateApprovalPostureRequest>,
    ) -> ApplicationTransition {
        match (self.state.session_reference.clone(), request) {
            (Some(session), Some(request)) => {
                ApplicationTransition::UpdateApprovalPosture { session, request }
            }
            _ => ApplicationTransition::Continue,
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
        self.state.abandon_pending_attach();
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

    /// Handles the Icon Picker commands routed here. Guarded by `is_open`
    /// even though every route that reaches these commands already scopes
    /// them to the picker being open — `command_for_icon_picker_event` binds
    /// them nowhere else, and the Sidebar menu and header press only ever
    /// invoke [`SemanticCommandId::SessionIconChoose`], which opens it —
    /// because a handler answering for state it does not own is the kind of
    /// bug that survives every one of today's callers and bites the first
    /// one added tomorrow.
    fn handle_icon_picker_command(
        &mut self,
        command: SemanticCommandId,
        subject: SemanticSubject,
    ) -> ApplicationTransition {
        if !self.state.icon_picker.is_open() {
            return ApplicationTransition::Continue;
        }
        match command {
            SemanticCommandId::IconPickerLeft => self.state.icon_picker.move_left(),
            SemanticCommandId::IconPickerRight => self.state.icon_picker.move_right(),
            SemanticCommandId::IconPickerUp => self.state.icon_picker.move_up(),
            SemanticCommandId::IconPickerDown => self.state.icon_picker.move_down(),
            SemanticCommandId::IconPickerSearchInsert => {
                if let SemanticSubject::Text(text) = subject {
                    self.state.icon_picker.insert(&text);
                }
            }
            SemanticCommandId::IconPickerSearchDelete => self.state.icon_picker.delete_backward(),
            SemanticCommandId::IconPickerClose => self.state.icon_picker.close(),
            SemanticCommandId::IconPickerChoose => return self.choose_icon(),
            _ => {}
        }
        ApplicationTransition::Continue
    }

    /// Sets the Icon Picker's Icon to its focused glyph and closes it. There
    /// is no clear action: a query that offers nothing leaves nothing
    /// focused, and Enter there does nothing rather than emptying the
    /// target's Icon.
    fn choose_icon(&mut self) -> ApplicationTransition {
        let Some(target) = self.state.icon_picker.target().cloned() else {
            return ApplicationTransition::Continue;
        };
        let Some(icon) = self
            .state
            .icon_picker
            .focused_entry()
            .map(|entry| entry.name.to_owned())
        else {
            return ApplicationTransition::Continue;
        };
        self.state.icon_picker.close();
        match target {
            IconPickerTarget::Session(session) => {
                ApplicationTransition::SetSessionIcon { session, icon }
            }
            IconPickerTarget::Workspace {
                origin,
                workspace_id,
            } => ApplicationTransition::SetWorkspaceIcon {
                origin,
                workspace_id,
                icon,
            },
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
        super::event_loop::frame_backend::finish_frame(
            frame.buffer_mut(),
            !self.state.overlay_owns_input()
                && !self.state.reconnect_overlay_visible()
                && self.state.active_selection_overlay_area().is_none(),
        );
    }

    pub fn handle_terminal_event(&mut self, event: InputEvent) -> Result<ApplicationTransition> {
        self.note_interaction(&event);
        let interaction = is_reader_interaction(&event);
        let command = self.command_for_terminal_input(event);
        if interaction {
            self.settle_armed_removals(command.as_ref());
        }
        command.map_or(Ok(ApplicationTransition::Continue), |command| {
            self.handle_event(ApplicationEvent::Command(command))
        })
    }

    /// Ending a Pairing is armed by one key and put down by every other, so an
    /// overlay never removes anything on a key the reader did not aim at it.
    /// The note a finished removal leaves goes the same way: the next key
    /// clears it.
    fn settle_armed_removals(&mut self, command: Option<&CommandId>) {
        if !matches!(
            command,
            Some(CommandId::InvokeSemantic(
                SemanticCommandId::ConnectRemoveRemote
            ))
        ) {
            self.state.connect_overlay.disarm_removal();
        }
        if !matches!(
            command,
            Some(CommandId::InvokeSemantic(
                SemanticCommandId::ServeRemovePeer
            ))
        ) {
            self.state.serve_overlay.disarm_removal();
        }
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
        if self.state.has_text_selection() {
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
            && self.state.has_text_selection()
        {
            return Some(CommandId::InvokeSemantic(
                SemanticCommandId::TextSelectionClear,
            ));
        }
        if matches!(&event, InputEvent::Resize(..))
            && self
                .state
                .composers
                .selection_range(self.state.composer_key())
                .is_none()
        {
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
        // Every surface that outranks the Intervention panels is asked here,
        // through the one reading self-presentation also consults, so a panel
        // never takes a key from something standing over it.
        match self.state.surface_above_interventions() {
            Some(SurfaceAboveInterventions::ApprovalPosture) => {
                return command_for_approval_posture_picker_event(event);
            }
            Some(SurfaceAboveInterventions::Selection(surface)) => match surface {
                SelectionSurface::Icons => return command_for_icon_picker_event(event),
                SelectionSurface::Connect => {
                    return command_for_connect_overlay_event(
                        event,
                        self.state.connect_overlay.input_mode(),
                    );
                }
                SelectionSurface::Serve => return command_for_serve_overlay_event(event),
                SelectionSurface::Themes => return command_for_theme_picker_event(event),
                SelectionSurface::Models => return command_for_model_picker_event(event),
                SelectionSurface::ModelOptions => return command_for_model_options_event(event),
                SelectionSurface::NumericEditor => {
                    return command_for_numeric_editor_event(event);
                }
                SelectionSurface::Settings => return command_for_settings_panel_event(event),
                SelectionSurface::Worktrees => {
                    return super::keymap::command_for_worktree_picker_event(event);
                }
                SelectionSurface::WorkspacePickerMenu => {
                    return command_for_workspace_picker_menu_event(event);
                }
                SelectionSurface::Workspaces => {
                    return command_for_workspace_picker_event(event);
                }
                SelectionSurface::Sessions => return command_for_session_picker_event(event),
                SelectionSurface::SidebarMenu => return command_for_sidebar_menu_event(event),
                SelectionSurface::Subagents => return command_for_subagent_picker_event(event),
                _ => {}
            },
            // The Sidebar comes after every overlay and before the composer's
            // own surfaces: it stands beside the main view rather than over
            // it, so an overlay a reader opened is still the newer surface and
            // owns the keys, while a completion list left standing over the
            // composer does not.
            Some(SurfaceAboveInterventions::Sidebar) => {
                if matches!(self.state.command_mode, CommandMode::Leader) {
                    return command_for_leader_event(event);
                }
                return command_for_sidebar_event(event);
            }
            // The Aside ranks beside the Sidebar, and holds the keys only
            // while the Sidebar does not.
            Some(SurfaceAboveInterventions::Aside) => {
                if matches!(self.state.command_mode, CommandMode::Leader) {
                    return command_for_leader_event(event);
                }
                return command_for_aside_event(event);
            }
            // Nothing stands over the panels here; they simply have nothing to
            // present, so the keys go on to the composer as they always would.
            Some(SurfaceAboveInterventions::UnreachableOrigin) | None => {}
        }
        if self
            .state
            .approvals
            .is_open(self.state.session_reference.as_ref())
        {
            if matches!(self.state.command_mode, CommandMode::Leader) {
                return command_for_leader_event(event);
            }
            if let Some(command) = super::approval::key(&event) {
                return self.state.armed_against(command);
            }
            return match command_for_terminal_event(event) {
                Some(
                    command @ (CommandId::ScrollTranscriptPageUp
                    | CommandId::ScrollTranscriptPageDown
                    | CommandId::WheelAt { .. }
                    | CommandId::FollowLatest
                    | CommandId::BeginLeader
                    | CommandId::InvokeSemantic(_)
                    | CommandId::ClickAt { .. }
                    | CommandId::OpenContextMenuAt { .. }),
                ) => Some(command),
                _ => None,
            };
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
                return self
                    .state
                    .armed_against(CommandId::QuestionnaireInsert(text.clone()));
            }
            if let Some(command) = super::questionnaire::key(&event) {
                return self.state.armed_against(command);
            }
            return match command_for_terminal_event(event) {
                Some(
                    command @ (CommandId::ScrollTranscriptPageUp
                    | CommandId::ScrollTranscriptPageDown
                    | CommandId::WheelAt { .. }
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
            // A settled Subagent whose Watches outlive it is still stopped
            // from its own Session: Escape there arms stopping the Watches in
            // its subtree and none above it, and leaves the view once there
            // is nothing left to stop (ADR 0030). Nothing else in this view
            // is interrupted from here.
            let monitoring = self.state.interrupt_target() == Some(InterruptTarget::Watches);
            return match self.state.command_mode {
                // The Leader reaches the Aside — the way around a Subagent's
                // tree — from inside it, and nothing else.
                CommandMode::Leader => command_for_subagent_view_leader_event(event),
                CommandMode::InterruptConfirmation { .. } if monitoring => {
                    command_for_interrupt_confirmation_event(event)
                }
                _ if monitoring => command_for_monitoring_subagent_view_event(event),
                _ => command_for_subagent_view_event(event),
            };
        }
        match self.state.command_mode {
            CommandMode::Composer => {
                if matches!(&event, InputEvent::Key(key)
                    if key.kind == KeyEventKind::Press
                        && key.code == KeyCode::Char('x')
                        && key.modifiers == KeyModifiers::CONTROL)
                    && self
                        .state
                        .composers
                        .selection_range(self.state.composer_key())
                        .is_some()
                {
                    Some(CommandId::CutSelection)
                } else {
                    command_for_terminal_event(event)
                }
            }
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

    /// The Session the per-tree subscription should be asked through, or
    /// `None` when the Aside has nothing to answer for. The run loop keeps
    /// one subscription open for as long as this names the same Session.
    /// Every entry the Aside's row focus can stand on for the open Session.
    fn aside_focus_entries(&self) -> Vec<super::aside::FocusEntry> {
        let Some(open) = self.state.route.as_ref() else {
            return Vec::new();
        };
        self.state.aside.focus_entries(
            open,
            AsidePresentation {
                theme: &self.theme,
                spinner_frame: self.state.spinner_frame,
                shimmer: &self.state.shimmer_clock,
                truecolor: self.terminal_facts.truecolor,
                now: self.state.presentation_clock.now(),
                session_now: self.state.session_now(),
            },
        )
    }

    pub(super) fn subagent_tree_request(&self) -> Option<SessionReference> {
        self.state.aside.tree_request(self.state.route.as_ref())
    }

    /// When the Aside's Subagents Section may next say Loading for a tree
    /// still arriving, while that moment is ahead: the one wakeup the run
    /// loop arms for it, so a quiet period passing redraws the frame.
    pub(super) fn subagent_tree_loading_deadline(&self) -> Option<Instant> {
        self.state.aside.loading_deadline(
            self.state.route.as_ref(),
            self.state.presentation_clock.now(),
        )
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

    /// Every Origin still owed its grace period, which is what the run loop
    /// arms one timer apiece from.
    pub(super) fn origins_awaiting_grace(&self) -> std::collections::BTreeSet<Outlook> {
        self.state.origins_awaiting_grace()
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
            // And live work in the Aside — a working Subagent, or a Working
            // top-level Session — whose Marker spins and whose time rises.
            || self.state.aside.shows_live_work()
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
    crate::paths::canonical(workspace).unwrap_or_else(|_| workspace.to_owned())
}

/// Everything the Session owes its reader, in Transcript order across both
/// kinds. Each kind is read through its own module's pending reading, so
/// presentation, dismissal, and the notices above the composer never disagree
/// about what is still owed.
fn pending_interventions(snapshot: &SessionSnapshot) -> impl Iterator<Item = InterventionId> {
    let approvals: Vec<_> = super::approval::pending(snapshot)
        .filter_map(|activity| match activity {
            Activity::Approval { approval, .. } => Some(approval.id),
            _ => None,
        })
        .collect();
    let questionnaires: Vec<_> = super::questionnaire::pending(snapshot)
        .map(|questionnaire| questionnaire.id)
        .collect();
    snapshot
        .activities
        .iter()
        .filter_map(move |activity| match activity {
            Activity::Approval { approval, .. } if approvals.contains(&approval.id) => {
                Some(InterventionId::Approval(approval.id))
            }
            Activity::Questionnaire { questionnaire, .. }
                if questionnaires.contains(&questionnaire.id) =>
            {
                Some(InterventionId::Questionnaire(questionnaire.id))
            }
            _ => None,
        })
}

/// Whether a terminal event is the reader acting rather than the terminal
/// reporting: a key press, a click, or a paste is theirs; a resize, a focus
/// change, and the mouse merely passing over the window are not.
/// Whether a command is one of the two that leave. They are answered even
/// under the whole-frame modal this machine's own Server's loss raises, so a
/// reader is never held inside a Client that cannot reach anything.
fn leaves_the_application(command: &CommandId) -> bool {
    matches!(
        command,
        CommandId::ClearOrExit | CommandId::InvokeSemantic(SemanticCommandId::ApplicationExit)
    )
}

fn is_reader_interaction(event: &InputEvent) -> bool {
    match event {
        InputEvent::Key(key) => key.kind == KeyEventKind::Press,
        InputEvent::Mouse(mouse) => !matches!(mouse.kind, MouseEventKind::Moved),
        InputEvent::Paste(_) => true,
        InputEvent::Resize(..) | InputEvent::FocusGained | InputEvent::FocusLost => false,
    }
}

impl TuiState {
    /// Which surface above the Intervention panels owns the keys, if any.
    /// The routing in [`Application::command_for_input_mode`] and the
    /// self-presentation step read the ladder through this one answer, so
    /// "something else owns the keys" is said once.
    ///
    /// A completion list left standing over the composer is not one of them:
    /// it is the composer's own, and the panels outrank it.
    fn surface_above_interventions(&self) -> Option<SurfaceAboveInterventions> {
        if self.interventions_are_held() {
            return Some(SurfaceAboveInterventions::UnreachableOrigin);
        }
        if self.approval_posture_picker.is_open() {
            return Some(SurfaceAboveInterventions::ApprovalPosture);
        }
        if let Some(surface) = self
            .top_selection_overlay()
            .filter(|surface| *surface != SelectionSurface::Completions)
        {
            return Some(SurfaceAboveInterventions::Selection(surface));
        }
        if self.sidebar_owns_input() {
            return Some(SurfaceAboveInterventions::Sidebar);
        }
        self.aside_owns_input()
            .then_some(SurfaceAboveInterventions::Aside)
    }

    /// Whether a panel that presented itself is still inside the moment it
    /// takes no key for.
    fn intervention_is_arming(&self) -> bool {
        self.intervention_armed_until
            .is_some_and(|until| self.presentation_clock.now() < until)
    }

    /// Drops a key the panel itself would act on while it is still arming, so
    /// a reader already mid-keystroke cannot answer something they have not
    /// read. Only what the panel would consume is dropped: Esc still puts it
    /// away, and everything the panel lets past — reading the Transcript, the
    /// Leader, the pointer — goes on reaching what it always did.
    fn armed_against(&self, command: CommandId) -> Option<CommandId> {
        let dismissal = matches!(
            command,
            CommandId::InvokeSemantic(
                SemanticCommandId::ApprovalHide | SemanticCommandId::QuestionnaireHide
            )
        );
        (dismissal || !self.intervention_is_arming()).then_some(command)
    }

    /// The open Session's oldest Intervention the reader has not dismissed,
    /// ready for its panel. Only the Session the reader has open answers
    /// here, whether it is a Subagent's or not: a Subagent's Interventions
    /// are its own Session's, so they present themselves once the reader is
    /// in it, and only mark the listing from anywhere else.
    fn next_intervention(&self) -> Option<PresentableIntervention> {
        let owner = self.session_reference.as_ref()?;
        let snapshot = self.session.as_ref()?.snapshot();
        let dismissed = self.dismissed_interventions.get(owner);
        let next = pending_interventions(snapshot).find(|intervention| {
            if dismissed.is_some_and(|dismissed| dismissed.contains(intervention)) {
                return false;
            }
            match intervention {
                InterventionId::Approval(_) => true,
                // A Questionnaire another Client has taken off the catalog is
                // no longer one this Client can answer.
                InterventionId::Questionnaire(id) => self.questionnaires.available(owner, *id),
            }
        })?;
        Some(match next {
            InterventionId::Approval(id) => PresentableIntervention::Approval(id),
            InterventionId::Questionnaire(id) => PresentableIntervention::Questionnaire(
                super::questionnaire::pending(snapshot)
                    .find(|questionnaire| questionnaire.id == id)?
                    .clone(),
            ),
        })
    }

    /// Records every Intervention the open Session has pending right now as
    /// dismissed, which is what Esc means: not this one, but none of them —
    /// until one arrives the reader has not seen.
    fn dismiss_interventions(&mut self) {
        let Some(owner) = self.session_reference.clone() else {
            return;
        };
        let Some(snapshot) = self.session.as_ref().map(SessionProjection::snapshot) else {
            return;
        };
        let pending: Vec<_> = pending_interventions(snapshot).collect();
        self.dismissed_interventions
            .entry(owner)
            .or_default()
            .extend(pending);
        self.intervention_armed_until = None;
    }

    /// Forgets what the open Session no longer owes. A dismissal stands for
    /// one Intervention, so it has nothing left to say once that Intervention
    /// is answered, withdrawn, or gone with its Turn.
    fn prune_dismissed_interventions(&mut self) {
        if self.dismissed_interventions.is_empty() {
            return;
        }
        let Some(owner) = self.session_reference.clone() else {
            return;
        };
        let Some(snapshot) = self.session.as_ref().map(SessionProjection::snapshot) else {
            return;
        };
        let pending: HashSet<_> = pending_interventions(snapshot).collect();
        if let Some(dismissed) = self.dismissed_interventions.get_mut(&owner) {
            dismissed.retain(|intervention| pending.contains(intervention));
            if dismissed.is_empty() {
                self.dismissed_interventions.remove(&owner);
            }
        }
    }

    /// Forgets what the reader dismissed in the open Session. Asking for a
    /// panel by key, slash command, or click is asking for its Session's
    /// Interventions back.
    fn recall_dismissed_interventions(&mut self) {
        if let Some(owner) = self.session_reference.as_ref() {
            self.dismissed_interventions.remove(owner);
        }
        self.intervention_armed_until = None;
    }

    /// Whether the open Session's Interventions are waiting out of sight
    /// because its Origin has stopped answering. A Decision taken now would go
    /// nowhere, so neither the panels nor the notices that name their keys
    /// have anything to say until the Remote answers.
    pub(super) fn interventions_are_held(&self) -> bool {
        self.session_reference
            .as_ref()
            .is_some_and(|session| self.is_unreachable(&session.origin))
    }

    pub(super) fn pending_approvals(&self) -> impl Iterator<Item = &Activity> {
        self.session
            .as_ref()
            .filter(|_| !self.interventions_are_held())
            .into_iter()
            .flat_map(|session| super::approval::pending(session.snapshot()))
    }

    pub(super) fn open_approval(&self) -> Option<&Activity> {
        if !self.approvals.is_open(self.session_reference.as_ref()) {
            return None;
        }
        let id = self.approvals.id()?;
        let snapshot = self.session.as_ref()?.snapshot();
        if !snapshot.pending_approvals.contains(&id) && !snapshot.submitting_approvals.contains(&id)
        {
            return None;
        }
        snapshot.activities.iter().find(
            |activity| matches!(activity, Activity::Approval { approval, .. } if approval.id == id),
        )
    }

    pub(super) fn pending_questionnaires(
        &self,
    ) -> impl Iterator<Item = &crate::protocol::Questionnaire> {
        self.session
            .as_ref()
            .filter(|_| !self.interventions_are_held())
            .into_iter()
            .flat_map(|session| {
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
