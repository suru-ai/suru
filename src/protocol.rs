use std::{fmt, path::PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u32 = 2;
pub const SNAPSHOT_EVENT: &str = "snapshot";
pub const COUNTER_UPDATED_EVENT: &str = "counter_updated";
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SessionRevision(pub u64);

impl SessionRevision {
    pub const INITIAL: Self = Self(1);

    pub fn immediately_follows(self, previous: Self) -> bool {
        previous.0.checked_add(1) == Some(self.0)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Workspace {
    pub path: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentIdentity {
    pub agent: AgentId,
    pub provider: ProviderId,
    pub model: ModelId,
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
pub enum ActivityKind {
    Status,
    Error,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    pub id: SessionId,
    pub workspace: Workspace,
    pub agent: Option<AgentIdentity>,
    pub status: SessionStatus,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Prompt {
    pub id: PromptId,
    pub text: String,
    pub status: PromptStatus,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Turn {
    pub id: TurnId,
    pub prompt_id: PromptId,
    pub status: TurnStatus,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub id: MessageId,
    pub turn_id: TurnId,
    pub role: MessageRole,
    pub content: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Activity {
    pub id: ActivityId,
    pub turn_id: TurnId,
    pub kind: ActivityKind,
    pub text: String,
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
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionUpdate {
    pub session_id: SessionId,
    pub revision: SessionRevision,
    pub changes: Vec<SessionChange>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionChange {
    PromptAdded { prompt: Prompt },
    TurnAdded { turn: Turn },
    MessageAdded { message: Message },
    ActivityAdded { activity: Activity },
    TurnStatusChanged { turn_id: TurnId, status: TurnStatus },
    SessionStatusChanged { status: SessionStatus },
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
    pub workspace: Workspace,
    pub prompt: InitialPrompt,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionErrorCode {
    InvalidCommand,
    EmptyPrompt,
    InvalidWorkspace,
    SessionNotFound,
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
    #[serde(flatten)]
    pub identity: ServerIdentity,
}

impl Health {
    pub fn new(identity: ServerIdentity, lifecycle: LifecycleState) -> Self {
        Self {
            lifecycle,
            identity,
        }
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
        ))
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CounterSnapshot {
    pub instance_id: Uuid,
    pub value: u64,
    pub revision: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CounterUpdate {
    pub value: u64,
    pub revision: u64,
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
