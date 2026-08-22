//! Provider-neutral runtime and per-Session execution interfaces.

use std::{error::Error, fmt, future::Future, path::PathBuf, pin::Pin, sync::Arc};

use futures_util::Stream;
use serde_json::Value;
use tokio::sync::watch;

use crate::protocol::{
    AgentIdentity, AgentSelection, EffectiveSettings, FileChange, ModelDescriptor, ModelOptionKind,
    ModelOptionRole, ProviderId, SessionId,
};

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
    kind: ProviderErrorKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderErrorKind {
    Failure,
    SelectionRejected,
}

impl ProviderError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            session_lost: false,
            kind: ProviderErrorKind::Failure,
        }
    }

    pub fn selection_rejected(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            session_lost: false,
            kind: ProviderErrorKind::SelectionRejected,
        }
    }

    pub(crate) fn mark_session_lost(mut self) -> Self {
        self.session_lost = true;
        self
    }

    pub(crate) fn is_session_lost(&self) -> bool {
        self.session_lost
    }

    pub(crate) fn mark_selection_rejected(mut self) -> Self {
        self.kind = ProviderErrorKind::SelectionRejected;
        self
    }

    pub(crate) fn is_selection_rejected(&self) -> bool {
        self.kind == ProviderErrorKind::SelectionRejected
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
    pub resume_state: Option<ProviderResumeState>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderResumeState(Value);

impl ProviderResumeState {
    pub fn new(payload: Value) -> Self {
        Self(payload)
    }

    pub fn payload(&self) -> &Value {
        &self.0
    }

    pub fn into_payload(self) -> Value {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderTurnInput {
    pub prompt: String,
    pub selection: AgentSelection,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderSteerInput {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderFileChangeStatus {
    Completed,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderEvent {
    AgentSelectionChanged {
        selection: AgentSelection,
    },
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
    FileChangeStarted {
        activity_id: ProviderActivityId,
        changes: Vec<FileChange>,
    },
    FileChangeUpdated {
        activity_id: ProviderActivityId,
        changes: Vec<FileChange>,
    },
    FileChangeCompleted {
        activity_id: ProviderActivityId,
        status: ProviderFileChangeStatus,
    },
    /// The Provider began a block of Reasoning. Its content follows as deltas,
    /// and its title arrives separately because a Provider that leads with one
    /// only reveals it once enough of the block has streamed.
    ReasoningStarted {
        activity_id: ProviderActivityId,
    },
    ReasoningTitleChanged {
        activity_id: ProviderActivityId,
        title: String,
    },
    ReasoningDelta {
        activity_id: ProviderActivityId,
        content: String,
    },
    ReasoningCompleted {
        activity_id: ProviderActivityId,
    },
    TurnCompleted,
    TurnInterrupted,
    AgentSelectionRejected {
        message: String,
    },
    TurnFailed {
        message: String,
    },
}

pub trait ProviderRuntime: Send + Sync + 'static {
    fn provider_id(&self) -> ProviderId;

    fn list_models(&self) -> ProviderFuture<'_, Vec<ModelDescriptor>>;

    fn start_session(
        &self,
        request: ProviderSessionRequest,
    ) -> ProviderFuture<'_, ProviderSessionConnection>;

    /// Stops in-progress Session startups and releases runtime-owned resources.
    fn shutdown(&self) -> ProviderFuture<'_, ()>;

    /// Hands the runtime the effective Settings. A runtime honors the Server
    /// Settings under its own `provider.<id>` key and ignores the rest, and it
    /// reads them when it acts rather than when a Session began, so a Setting
    /// that changes mid-run governs the next Turn rather than only the next
    /// Session. Startup is the only caller today; Setting mutations and a
    /// Config Document watcher hand over a replaced view the same way.
    /// Runtimes that honor no Setting need not implement it.
    fn apply_settings(&self, settings: &EffectiveSettings) {
        let _ = settings;
    }
}

pub(crate) fn validate_models(models: &[ModelDescriptor]) -> Result<(), ProviderError> {
    let mut model_ids = std::collections::HashSet::new();
    for model in models {
        if model.id.as_str().is_empty() || !model_ids.insert((&model.provider, &model.id)) {
            return Err(ProviderError::new(format!(
                "Model catalog contains an empty or duplicate Model ID `{}`",
                model.id
            )));
        }
        let mut roles = std::collections::HashSet::new();
        let mut option_ids = std::collections::HashSet::new();
        for option in &model.options {
            if option.id.as_str().is_empty() || !option_ids.insert(&option.id) {
                return Err(ProviderError::new(format!(
                    "Model `{}` contains an empty or duplicate option ID `{}`",
                    model.id, option.id
                )));
            }
            if option.role != ModelOptionRole::Other && !roles.insert(option.role) {
                return Err(ProviderError::new(format!(
                    "Model `{}` has duplicate {:?} options",
                    model.id, option.role
                )));
            }
            if let ModelOptionKind::Select { choices, default } = &option.kind {
                let mut choice_ids = std::collections::HashSet::new();
                if choices
                    .iter()
                    .any(|choice| choice.id.as_str().is_empty() || !choice_ids.insert(&choice.id))
                {
                    return Err(ProviderError::new(format!(
                        "Model `{}` option `{}` contains an empty or duplicate choice ID",
                        model.id, option.id
                    )));
                }
                let Some(default_choice) = choices.iter().find(|choice| choice.id == *default)
                else {
                    return Err(ProviderError::new(format!(
                        "Model `{}` option `{}` has a default that is not one of its choices",
                        model.id, option.id
                    )));
                };
                if default_choice.availability != crate::protocol::ModelAvailability::Available {
                    return Err(ProviderError::new(format!(
                        "Model `{}` option `{}` has an unavailable default",
                        model.id, option.id
                    )));
                }
            }
        }
    }
    Ok(())
}

pub trait ProviderSession: Send + Sync + 'static {
    fn start_turn(&self, input: ProviderTurnInput) -> ProviderFuture<'_, ()>;

    fn steer_turn(&self, input: ProviderSteerInput) -> ProviderFuture<'_, ()>;

    fn interrupt_turn(&self) -> ProviderFuture<'_, ()>;

    /// Stops accepting Provider work and releases the Session's resources within a bounded time.
    fn shutdown(&self) -> ProviderFuture<'_, ()>;
}

pub struct ProviderSessionConnection {
    identity: AgentIdentity,
    resume_state: Option<ProviderResumeState>,
    session: Arc<dyn ProviderSession>,
    events: ProviderEventStream,
}

impl ProviderSessionConnection {
    pub fn new(
        identity: AgentIdentity,
        resume_state: Option<ProviderResumeState>,
        session: Arc<dyn ProviderSession>,
        events: ProviderEventStream,
    ) -> Self {
        Self {
            identity,
            resume_state,
            session,
            events,
        }
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        AgentIdentity,
        Option<ProviderResumeState>,
        Arc<dyn ProviderSession>,
        ProviderEventStream,
    ) {
        (self.identity, self.resume_state, self.session, self.events)
    }
}

pub(crate) async fn wait_for_shutdown(signal: &mut watch::Receiver<bool>) {
    while !*signal.borrow() {
        if signal.changed().await.is_err() {
            break;
        }
    }
}
