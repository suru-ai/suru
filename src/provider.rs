//! Provider-neutral runtime and per-Session execution interfaces.

use std::{error::Error, fmt, future::Future, path::PathBuf, pin::Pin, sync::Arc};

use futures_util::Stream;
use tokio::sync::watch;

use crate::protocol::{AgentIdentity, SessionId};

mod codex;
mod orchestration;

pub use codex::CodexRuntime;
pub(crate) use orchestration::{ProviderOrchestrator, ProviderUpdateGate};

pub type ProviderFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, ProviderError>> + Send + 'a>>;
pub type ProviderEventStream =
    Pin<Box<dyn Stream<Item = Result<ProviderEvent, ProviderError>> + Send>>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderError {
    message: String,
    session_lost: bool,
}

impl ProviderError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            session_lost: false,
        }
    }

    pub(crate) fn mark_session_lost(mut self) -> Self {
        self.session_lost = true;
        self
    }

    pub(crate) fn is_session_lost(&self) -> bool {
        self.session_lost
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ProviderError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderSessionRequest {
    pub session_id: SessionId,
    pub workspace: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderTurnInput {
    pub prompt: String,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ProviderActivityId(String);

impl ProviderActivityId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderCommandStatus {
    Completed,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderEvent {
    AgentMessageStarted,
    AgentMessageDelta {
        content: String,
    },
    AgentMessageCompleted,
    CommandStarted {
        activity_id: ProviderActivityId,
        command: String,
        cwd: Option<PathBuf>,
    },
    CommandOutputDelta {
        activity_id: ProviderActivityId,
        content: String,
    },
    CommandCompleted {
        activity_id: ProviderActivityId,
        status: ProviderCommandStatus,
        exit_status: Option<i32>,
    },
    TurnCompleted,
    TurnInterrupted,
    TurnFailed {
        message: String,
    },
}

pub trait ProviderRuntime: Send + Sync + 'static {
    fn start_session(
        &self,
        request: ProviderSessionRequest,
    ) -> ProviderFuture<'_, ProviderSessionConnection>;

    /// Stops in-progress Session startups and releases runtime-owned resources.
    fn shutdown(&self) -> ProviderFuture<'_, ()>;
}

pub trait ProviderSession: Send + Sync + 'static {
    fn start_turn(&self, input: ProviderTurnInput) -> ProviderFuture<'_, ()>;

    fn steer_turn(&self, input: ProviderTurnInput) -> ProviderFuture<'_, ()>;

    fn interrupt_turn(&self) -> ProviderFuture<'_, ()>;

    /// Stops accepting Provider work and releases the Session's resources within a bounded time.
    fn shutdown(&self) -> ProviderFuture<'_, ()>;
}

pub struct ProviderSessionConnection {
    identity: AgentIdentity,
    session: Arc<dyn ProviderSession>,
    events: ProviderEventStream,
}

impl ProviderSessionConnection {
    pub fn new(
        identity: AgentIdentity,
        session: Arc<dyn ProviderSession>,
        events: ProviderEventStream,
    ) -> Self {
        Self {
            identity,
            session,
            events,
        }
    }

    pub(crate) fn into_parts(
        self,
    ) -> (AgentIdentity, Arc<dyn ProviderSession>, ProviderEventStream) {
        (self.identity, self.session, self.events)
    }
}

pub(crate) async fn wait_for_shutdown(signal: &mut watch::Receiver<bool>) {
    while !*signal.borrow() {
        if signal.changed().await.is_err() {
            break;
        }
    }
}
