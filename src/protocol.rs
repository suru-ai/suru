use std::{fmt, path::PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u32 = 18;
pub const SERVER_SHUTDOWN_EVENT: &str = "server_shutdown";
pub const SETTINGS_SNAPSHOT_EVENT: &str = "settings_snapshot";
pub const SESSION_CATALOG_SNAPSHOT_EVENT: &str = "session_catalog_snapshot";
pub const SESSION_CATALOG_UPDATED_EVENT: &str = "session_catalog_updated";
pub const SESSION_SNAPSHOT_EVENT: &str = "session_snapshot";
pub const SESSION_UPDATED_EVENT: &str = "session_updated";

macro_rules! session_identity {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }

            pub const fn from_uuid(value: Uuid) -> Self {
                Self(value)
            }

            pub const fn as_uuid(self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

session_identity!(SessionId);
session_identity!(PromptId);
session_identity!(TurnId);
session_identity!(MessageId);
session_identity!(ActivityId);
session_identity!(AgentSelectionOperationId);

macro_rules! named_identity {
    ($name:ident) => {
        #[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

named_identity!(AgentId);
named_identity!(ProviderId);
named_identity!(ModelId);
named_identity!(ModelOptionId);
named_identity!(ModelOptionChoiceId);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelAvailability {
    Available,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelOptionRole {
    ReasoningEffort,
    Speed,
    Context,
    Verbosity,
    Other,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelOptionChoice {
    pub id: ModelOptionChoiceId,
    pub label: String,
    pub description: Option<String>,
    pub availability: ModelAvailability,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelOptionKind {
    Select {
        choices: Vec<ModelOptionChoice>,
        default: ModelOptionChoiceId,
    },
    Toggle {
        default: bool,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelOptionDescriptor {
    pub id: ModelOptionId,
    pub label: String,
    pub description: Option<String>,
    pub role: ModelOptionRole,
    pub kind: ModelOptionKind,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelDescriptor {
    pub provider: ProviderId,
    pub id: ModelId,
    pub display_name: String,
    pub description: String,
    pub is_default: bool,
    pub availability: ModelAvailability,
    pub options: Vec<ModelOptionDescriptor>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentSelectionMaterializationError {
    MissingOption { option: ModelOptionId },
    DuplicateOption { option: ModelOptionId },
    UnknownOption { option: ModelOptionId },
    InvalidOptionValue { option: ModelOptionId },
}

impl fmt::Display for AgentSelectionMaterializationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingOption { option } => {
                write!(formatter, "Model Option `{option}` is missing")
            }
            Self::DuplicateOption { option } => {
                write!(
                    formatter,
                    "Model Option `{option}` is selected more than once"
                )
            }
            Self::UnknownOption { option } => {
                write!(formatter, "Model Option `{option}` is unknown")
            }
            Self::InvalidOptionValue { option } => {
                write!(
                    formatter,
                    "Model Option `{option}` has no such available value"
                )
            }
        }
    }
}

impl std::error::Error for AgentSelectionMaterializationError {}

impl ModelDescriptor {
    pub fn default_agent_selection(&self) -> AgentSelection {
        AgentSelection {
            provider: self.provider.clone(),
            model: self.id.clone(),
            options: self
                .options
                .iter()
                .map(ModelOptionDescriptor::default_selection)
                .collect(),
        }
    }

    pub fn materialize_agent_selection(
        &self,
        current: Option<&AgentSelection>,
    ) -> Result<AgentSelection, AgentSelectionMaterializationError> {
        let Some(current) = current
            .filter(|selection| selection.provider == self.provider && selection.model == self.id)
        else {
            return Ok(self.default_agent_selection());
        };
        let options = self
            .options
            .iter()
            .map(|descriptor| {
                let mut matching = current
                    .options
                    .iter()
                    .filter(|selection| selection.id == descriptor.id);
                let selection = matching.next().ok_or_else(|| {
                    AgentSelectionMaterializationError::MissingOption {
                        option: descriptor.id.clone(),
                    }
                })?;
                if matching.next().is_some() {
                    return Err(AgentSelectionMaterializationError::DuplicateOption {
                        option: descriptor.id.clone(),
                    });
                }
                if !descriptor.value_is_available(&selection.value) {
                    return Err(AgentSelectionMaterializationError::InvalidOptionValue {
                        option: descriptor.id.clone(),
                    });
                }
                Ok(selection.clone())
            })
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(selection) = current.options.iter().find(|selection| {
            !self
                .options
                .iter()
                .any(|descriptor| descriptor.id == selection.id)
        }) {
            return Err(AgentSelectionMaterializationError::UnknownOption {
                option: selection.id.clone(),
            });
        }
        Ok(AgentSelection {
            provider: self.provider.clone(),
            model: self.id.clone(),
            options,
        })
    }
}

impl ModelOptionDescriptor {
    fn default_selection(&self) -> ModelOptionSelection {
        ModelOptionSelection {
            id: self.id.clone(),
            value: match &self.kind {
                ModelOptionKind::Select { default, .. } => ModelOptionValue::Select {
                    choice: default.clone(),
                },
                ModelOptionKind::Toggle { default } => {
                    ModelOptionValue::Toggle { enabled: *default }
                }
            },
        }
    }

    fn value_is_available(&self, value: &ModelOptionValue) -> bool {
        match (&self.kind, value) {
            (ModelOptionKind::Toggle { .. }, ModelOptionValue::Toggle { .. }) => true,
            (ModelOptionKind::Select { choices, .. }, ModelOptionValue::Select { choice }) => {
                choices.iter().any(|candidate| {
                    candidate.id == *choice
                        && candidate.availability == ModelAvailability::Available
                })
            }
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelOptionValue {
    Select { choice: ModelOptionChoiceId },
    Toggle { enabled: bool },
}

/// Why a Provider cannot be used right now. Each reason names a condition the
/// user fixes outside Suru — installing the Provider's CLI, signing in to it,
/// or moving to a version Suru speaks — which the next catalog refresh
/// re-evaluates.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderUnavailability {
    NotInstalled,
    NotSignedIn,
    IncompatibleVersion,
}

impl ProviderUnavailability {
    /// The reason as a client states it to the user.
    pub fn label(self) -> &'static str {
        match self {
            Self::NotInstalled => "not installed",
            Self::NotSignedIn => "not signed in",
            Self::IncompatibleVersion => "incompatible version",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderCatalogStatus {
    Fresh,
    Refreshing,
    Stale {
        message: String,
    },
    Failed {
        message: String,
    },
    /// The Provider cannot be used at all until the user fixes `reason`
    /// outside Suru. Whatever Models the catalog still holds stay listed so
    /// the Provider keeps its place, but none of them may be selected.
    Unavailable {
        reason: ProviderUnavailability,
        message: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderModelCatalog {
    pub provider: ProviderId,
    pub models: Vec<ModelDescriptor>,
    pub status: ProviderCatalogStatus,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCatalog {
    pub providers: Vec<ProviderModelCatalog>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SessionRevision(pub u64);

impl SessionRevision {
    pub const INITIAL: Self = Self(1);

    pub fn immediately_follows(self, previous: Self) -> bool {
        previous.0.checked_add(1) == Some(self.0)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SessionCatalogRevision(pub u64);

impl SessionCatalogRevision {
    pub const INITIAL: Self = Self(1);

    pub fn immediately_follows(self, previous: Self) -> bool {
        previous.0.checked_add(1) == Some(self.0)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct PromptOrder(pub u64);

impl PromptOrder {
    pub const INITIAL: Self = Self(1);
}

/// Milliseconds since the Unix epoch.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SessionTimestamp(pub u64);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Workspace {
    pub path: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelOptionSelection {
    pub id: ModelOptionId,
    pub value: ModelOptionValue,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSelection {
    pub provider: ProviderId,
    pub model: ModelId,
    pub options: Vec<ModelOptionSelection>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentIdentity {
    pub agent: AgentId,
    pub selection: AgentSelection,
}

/// Where a Setting applies: a Client Setting governs a client's presentation,
/// a Server Setting governs server or Provider behavior. Dormant data until a
/// machine-local overlay distinguishes the two, but declared from day one so
/// that overlay never reshapes the schema.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingScope {
    Client,
    Server,
}

/// The default Fold posture a Session view opens with.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FoldPosture {
    #[default]
    Folded,
    Expanded,
}

/// How much Reasoning summary detail a Turn requests from Codex.
///
/// Codex resolves a Turn's summary detail as what the Turn asked for or,
/// failing that, the Model's own default — and every Model in the current
/// catalog ships that default as `none`. A Turn that states no preference
/// therefore gets Reasoning with no summary at all: nothing streams and the
/// completed block carries an empty summary. Suru's default is `auto`, which
/// lets the Model choose how much to say.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningSummaryDetail {
    #[default]
    Auto,
    Concise,
    Detailed,
    None,
}

/// Whether a Transcript draws the Reasoning a Session stored.
///
/// Hidden is the built-in default: thinking is the agent's working-out, and a
/// Transcript leads with the work and the answer rather than the account of
/// how the agent got there. Showing it is a reader's deliberate choice.
///
/// Either way it is presentation and nothing more: the blocks keep arriving,
/// keep being stored, and keep being what a Provider was asked for, so a
/// reader who turns Reasoning on is shown every block that arrived while it
/// was off. Distinct from the `provider.codex.reasoningSummary` Setting, which
/// decides how much Reasoning a Turn asks for in the first place.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningVisibility {
    #[default]
    Hidden,
    Shown,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptSettings {
    pub default_fold_posture: FoldPosture,
    pub reasoning_visibility: ReasoningVisibility,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CodexSettings {
    pub reasoning_summary: ReasoningSummaryDetail,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSettings {
    pub codex: CodexSettings,
}

/// The effective value of every defined Setting: what a Config Document
/// pinned where it did, the built-in default everywhere else.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveSettings {
    pub transcript: TranscriptSettings,
    pub provider: ProviderSettings,
}

/// A typed change to exactly one Setting: the whole surface through which a
/// client edits a Config Document. Every Setting the schema defines has its own
/// variant carrying its own value type, so a client can neither misspell a key
/// path nor pin a value the Setting cannot hold. A `value` pins that value even
/// when it equals the built-in default, so a deliberate choice survives a later
/// change of that default; `null` removes the pin and lets the default resume.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "setting", rename_all = "snake_case", deny_unknown_fields)]
pub enum SettingMutation {
    TranscriptDefaultFoldPosture {
        value: Option<FoldPosture>,
    },
    TranscriptReasoningVisibility {
        value: Option<ReasoningVisibility>,
    },
    ProviderCodexReasoningSummary {
        value: Option<ReasoningSummaryDetail>,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsDiagnosticSeverity {
    /// One key was ignored; the rest of the Config Document applied.
    Warning,
    /// A whole Config Document was ignored.
    Error,
}

/// One configuration problem found while loading Config Documents, carrying
/// enough that the Log alone is sufficient to fix it: the file, the key path
/// when the problem is scoped to one key, and why the value was ignored.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsDiagnostic {
    pub severity: SettingsDiagnosticSeverity,
    pub file: PathBuf,
    pub key: Option<String>,
    pub message: String,
}

/// The effective-settings view the server pushes to every client on connect.
/// Carries the startup diagnostics so a client can surface configuration
/// problems without a side channel.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsSnapshot {
    pub settings: EffectiveSettings,
    /// Dotted schema key paths a Config Document pins, so a client can tell a
    /// deliberate choice from a built-in default without reading the file.
    pub pinned: Vec<String>,
    pub diagnostics: Vec<SettingsDiagnostic>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Idle,
    Active,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptStatus {
    Pending,
    Delivered,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptDelivery {
    Steer,
    Queue,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Active,
    Completed,
    Failed,
    Interrupted,
}

impl TurnStatus {
    /// Whether the Turn has Settled, and so accepts no further Provider output.
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Active)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Agent,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageStatus {
    Streaming,
    Completed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityStatus {
    Active,
    Completed,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FileChange {
    Add {
        path: PathBuf,
    },
    Delete {
        path: PathBuf,
    },
    Update {
        path: PathBuf,
        moved_to: Option<PathBuf>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Activity {
    Status {
        id: ActivityId,
        turn_id: TurnId,
        text: String,
    },
    Error {
        id: ActivityId,
        turn_id: TurnId,
        text: String,
    },
    Command {
        id: ActivityId,
        turn_id: TurnId,
        status: ActivityStatus,
        command: String,
        cwd: Option<PathBuf>,
        output: String,
        /// Whether Suru's cap cut the stored output short of what the Provider
        /// sent, so a client can say so without reading it out of `output`.
        output_truncated: bool,
        exit_status: Option<i32>,
    },
    FileChange {
        id: ActivityId,
        turn_id: TurnId,
        status: ActivityStatus,
        changes: Vec<FileChange>,
    },
    /// One block of Reasoning the Provider reported while working the Turn.
    Reasoning {
        id: ActivityId,
        turn_id: TurnId,
        status: ActivityStatus,
        /// The heading the Provider led the block with, once it has sent one.
        /// Carried apart from `content` so a client can head a folded block
        /// with it rather than parsing it back out of the prose.
        title: Option<String>,
        content: String,
        /// Whether Suru's cap cut the stored content short of what the Provider
        /// sent, so a client can say so without reading it out of `content`.
        content_truncated: bool,
        /// How long the Provider spent on the block, known only once it
        /// settles and only when it settled by completing.
        duration_ms: Option<u64>,
    },
}

impl Activity {
    pub const fn id(&self) -> ActivityId {
        match self {
            Self::Status { id, .. }
            | Self::Error { id, .. }
            | Self::Command { id, .. }
            | Self::FileChange { id, .. }
            | Self::Reasoning { id, .. } => *id,
        }
    }

    pub const fn turn_id(&self) -> TurnId {
        match self {
            Self::Status { turn_id, .. }
            | Self::Error { turn_id, .. }
            | Self::Command { turn_id, .. }
            | Self::FileChange { turn_id, .. }
            | Self::Reasoning { turn_id, .. } => *turn_id,
        }
    }

    /// The lifecycle this Activity settles through, or `None` for the kinds
    /// that report a moment rather than work in progress.
    pub const fn status(&self) -> Option<ActivityStatus> {
        match self {
            Self::Status { .. } | Self::Error { .. } => None,
            Self::Command { status, .. }
            | Self::FileChange { status, .. }
            | Self::Reasoning { status, .. } => Some(*status),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    pub id: SessionId,
    pub workspace: Workspace,
    pub agent_selection: Option<AgentSelection>,
    pub agent_selection_availability: ModelAvailability,
    pub status: SessionStatus,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSummary {
    #[serde(flatten)]
    pub session: Session,
    pub title: String,
    pub created_at: SessionTimestamp,
    pub updated_at: SessionTimestamp,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "readability",
    content = "summary",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum SessionListItem {
    Readable(SessionSummary),
    Unreadable(UnreadableSessionSummary),
}

impl SessionListItem {
    pub const fn id(&self) -> SessionId {
        match self {
            Self::Readable(summary) => summary.session.id,
            Self::Unreadable(summary) => summary.id,
        }
    }

    pub fn title(&self) -> &str {
        match self {
            Self::Readable(summary) => &summary.title,
            Self::Unreadable(summary) => &summary.title,
        }
    }

    pub const fn updated_at(&self) -> SessionTimestamp {
        match self {
            Self::Readable(summary) => summary.updated_at,
            Self::Unreadable(summary) => summary.updated_at,
        }
    }

    pub const fn readable(&self) -> Option<&SessionSummary> {
        match self {
            Self::Readable(summary) => Some(summary),
            Self::Unreadable(_) => None,
        }
    }

    pub const fn workspace(&self) -> Option<&Workspace> {
        match self {
            Self::Readable(summary) => Some(&summary.session.workspace),
            Self::Unreadable(summary) => summary.workspace.as_ref(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UnreadableSessionSummary {
    pub id: SessionId,
    pub title: String,
    pub created_at: SessionTimestamp,
    pub updated_at: SessionTimestamp,
    pub workspace: Option<Workspace>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCatalogSnapshot {
    pub revision: SessionCatalogRevision,
    pub session_ids: Vec<SessionId>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionCatalogChange {
    Created { session_id: SessionId },
    Deleted { session_id: SessionId },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCatalogUpdate {
    pub revision: SessionCatalogRevision,
    pub change: SessionCatalogChange,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Prompt {
    pub id: PromptId,
    pub text: String,
    pub delivery: PromptDelivery,
    pub admission_order: PromptOrder,
    pub status: PromptStatus,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Turn {
    pub id: TurnId,
    pub prompt_id: PromptId,
    pub agent: Option<AgentIdentity>,
    pub status: TurnStatus,
    /// When the commit that delivered this Turn's opening Prompt landed, and
    /// when the commit that settled it landed. Both are absent on a Turn stored
    /// before Suru recorded Turn timing, so a client states how long a Turn
    /// worked only when it knows.
    pub started_at: Option<SessionTimestamp>,
    pub settled_at: Option<SessionTimestamp>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub id: MessageId,
    pub turn_id: TurnId,
    pub role: MessageRole,
    pub status: MessageStatus,
    pub content: String,
    /// Whether Suru's cap cut the stored content short of what the Provider
    /// sent, so a client can say so without reading it out of `content`.
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TranscriptItem {
    Message { message_id: MessageId },
    Activity { activity_id: ActivityId },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSnapshot {
    pub session: Session,
    pub revision: SessionRevision,
    pub prompts: Vec<Prompt>,
    pub turns: Vec<Turn>,
    pub messages: Vec<Message>,
    pub activities: Vec<Activity>,
    pub transcript: Vec<TranscriptItem>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionUpdate {
    pub session_id: SessionId,
    pub revision: SessionRevision,
    pub changes: Vec<SessionChange>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionChange {
    AgentSelectionChanged {
        selection: AgentSelection,
    },
    AgentSelectionAvailabilityChanged {
        availability: ModelAvailability,
    },
    PromptAdded {
        prompt: Prompt,
    },
    PromptDeliveryChanged {
        prompt_id: PromptId,
        delivery: PromptDelivery,
    },
    PromptStatusChanged {
        prompt_id: PromptId,
        status: PromptStatus,
    },
    TurnAdded {
        turn: Turn,
    },
    TurnAgentChanged {
        turn_id: TurnId,
        agent: AgentIdentity,
    },
    MessageAdded {
        message: Message,
    },
    MessageContentAppended {
        message_id: MessageId,
        content: String,
    },
    MessageTruncated {
        message_id: MessageId,
    },
    MessageCompleted {
        message_id: MessageId,
    },
    ActivityAdded {
        activity: Activity,
    },
    CommandOutputAppended {
        activity_id: ActivityId,
        content: String,
    },
    CommandOutputTruncated {
        activity_id: ActivityId,
    },
    CommandStatusChanged {
        activity_id: ActivityId,
        status: ActivityStatus,
        exit_status: Option<i32>,
    },
    FileChangeUpdated {
        activity_id: ActivityId,
        changes: Vec<FileChange>,
    },
    FileChangeStatusChanged {
        activity_id: ActivityId,
        status: ActivityStatus,
    },
    ReasoningTitleChanged {
        activity_id: ActivityId,
        title: String,
    },
    ReasoningContentAppended {
        activity_id: ActivityId,
        content: String,
    },
    ReasoningContentTruncated {
        activity_id: ActivityId,
    },
    ReasoningStatusChanged {
        activity_id: ActivityId,
        status: ActivityStatus,
        duration_ms: Option<u64>,
    },
    TurnStatusChanged {
        turn_id: TurnId,
        status: TurnStatus,
        /// Stamped by the settle commit itself, so every client settles the
        /// Turn at the moment the server did rather than when it read the
        /// change.
        settled_at: Option<SessionTimestamp>,
    },
    SessionStatusChanged {
        status: SessionStatus,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InitialPrompt {
    pub id: PromptId,
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSessionRequest {
    pub agent_selection: Option<AgentSelection>,
    pub workspace: Workspace,
    pub prompt: InitialPrompt,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdmitPromptRequest {
    pub prompt: InitialPrompt,
    pub delivery: PromptDelivery,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateAgentSelectionRequest {
    pub operation_id: AgentSelectionOperationId,
    pub selection: AgentSelection,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionErrorCode {
    InvalidCommand,
    EmptyPrompt,
    InvalidWorkspace,
    SessionNotFound,
    PromptConflict,
    PromptNotFound,
    PromptNotPending,
    TurnNotFound,
    TurnNotActive,
    TurnInterruptionFailed,
    AgentSelectionOperationConflict,
    AgentSelectionProviderConflict,
    ConfigRootUnavailable,
    ConfigDocumentNotEditable,
    ConfigDocumentWriteFailed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionError {
    pub code: SessionErrorCode,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    Starting,
    Ready,
    Stopping,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServerIdentity {
    pub instance_id: Uuid,
    pub pid: u32,
    pub protocol_version: u32,
    pub build_identity: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Health {
    pub lifecycle: LifecycleState,
    pub landing_agent_selection: Option<AgentSelection>,
    #[serde(flatten)]
    pub identity: ServerIdentity,
}

impl Health {
    pub fn new(identity: ServerIdentity, lifecycle: LifecycleState) -> Self {
        Self {
            lifecycle,
            landing_agent_selection: None,
            identity,
        }
    }

    pub fn with_landing_agent_selection(
        mut self,
        landing_agent_selection: Option<AgentSelection>,
    ) -> Self {
        self.landing_agent_selection = landing_agent_selection;
        self
    }
}

impl std::ops::Deref for Health {
    type Target = ServerIdentity;

    fn deref(&self) -> &Self::Target {
        &self.identity
    }
}

impl std::ops::DerefMut for Health {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.identity
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HealthWire {
    instance_id: Uuid,
    pid: u32,
    lifecycle: LifecycleState,
    protocol_version: u32,
    build_identity: String,
    #[serde(default)]
    landing_agent_selection: Option<AgentSelection>,
}

impl<'de> Deserialize<'de> for Health {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = HealthWire::deserialize(deserializer)?;
        Ok(Self::new(
            ServerIdentity {
                instance_id: wire.instance_id,
                pid: wire.pid,
                protocol_version: wire.protocol_version,
                build_identity: wire.build_identity,
            },
            wire.lifecycle,
        )
        .with_landing_agent_selection(wire.landing_agent_selection))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RuntimeDescriptor {
    pub base_url: String,
    pub token: String,
    #[serde(flatten)]
    pub identity: ServerIdentity,
}

impl RuntimeDescriptor {
    pub fn new(base_url: String, token: String, identity: ServerIdentity) -> Self {
        Self {
            base_url,
            token,
            identity,
        }
    }

    pub fn health(&self, lifecycle: LifecycleState) -> Health {
        Health::new(self.identity.clone(), lifecycle)
    }
}

impl std::ops::Deref for RuntimeDescriptor {
    type Target = ServerIdentity;

    fn deref(&self) -> &Self::Target {
        &self.identity
    }
}

impl std::ops::DerefMut for RuntimeDescriptor {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.identity
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeDescriptorWire {
    base_url: String,
    token: String,
    instance_id: Uuid,
    pid: u32,
    protocol_version: u32,
    build_identity: String,
}

impl<'de> Deserialize<'de> for RuntimeDescriptor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = RuntimeDescriptorWire::deserialize(deserializer)?;
        Ok(Self::new(
            wire.base_url,
            wire.token,
            ServerIdentity {
                instance_id: wire.instance_id,
                pid: wire.pid,
                protocol_version: wire.protocol_version,
                build_identity: wire.build_identity,
            },
        ))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShutdownReason {
    Manual,
    Replacement,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerShutdown {
    pub instance_id: Uuid,
    pub reason: ShutdownReason,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionDeleted {
    pub session_id: SessionId,
}
