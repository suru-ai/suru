//! Provider-neutral runtime and per-Session execution interfaces.

use std::{error::Error, fmt, future::Future, path::PathBuf, pin::Pin, sync::Arc};

use futures_util::Stream;

use crate::protocol::AgentIdentity;

mod codex;
mod orchestration;

pub use codex::CodexRuntime;
pub(crate) use orchestration::ProviderOrchestrator;

pub type ProviderFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, ProviderError>> + Send + 'a>>;
pub type ProviderEventStream =
    Pin<Box<dyn Stream<Item = Result<ProviderEvent, ProviderError>> + Send>>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderError {
    message: String,
}

impl ProviderError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
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
    pub workspace: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderTurnInput {
    pub prompt: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderEvent {
    AgentMessageStarted,
    AgentMessageDelta { content: String },
    AgentMessageCompleted,
    TurnCompleted,
    TurnInterrupted,
    TurnFailed { message: String },
}

pub trait ProviderRuntime: Send + Sync + 'static {
    fn start_session(
        &self,
        request: ProviderSessionRequest,
    ) -> ProviderFuture<'_, ProviderSessionConnection>;
}

pub trait ProviderSession: Send + Sync + 'static {
    fn start_turn(&self, input: ProviderTurnInput) -> ProviderFuture<'_, ()>;
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
