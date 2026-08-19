use std::{fmt, path::PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u32 = 13;
pub const SERVER_SHUTDOWN_EVENT: &str = "server_shutdown";
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderCatalogStatus {
    Fresh,
    Refreshing,
    Stale { message: String },
    Failed { message: String },
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
        exit_status: Option<i32>,
    },
    FileChange {
        id: ActivityId,
        turn_id: TurnId,
        status: ActivityStatus,
        changes: Vec<FileChange>,
    },
}

impl Activity {
    pub const fn id(&self) -> ActivityId {
        match self {
            Self::Status { id, .. }
            | Self::Error { id, .. }
            | Self::Command { id, .. }
            | Self::FileChange { id, .. } => *id,
        }
    }

    pub const fn turn_id(&self) -> TurnId {
        match self {
            Self::Status { turn_id, .. }
            | Self::Error { turn_id, .. }
            | Self::Command { turn_id, .. }
            | Self::FileChange { turn_id, .. } => *turn_id,
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
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub id: MessageId,
    pub turn_id: TurnId,
    pub role: MessageRole,
    pub status: MessageStatus,
    pub content: String,
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
    TurnStatusChanged {
        turn_id: TurnId,
        status: TurnStatus,
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
