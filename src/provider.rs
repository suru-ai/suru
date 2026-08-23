//! Provider-neutral runtime and per-Session execution interfaces.

use std::{error::Error, fmt, future::Future, path::PathBuf, pin::Pin, sync::Arc};

use futures_util::Stream;
use serde_json::Value;
use tokio::sync::watch;

use crate::protocol::{
    AgentIdentity, AgentSelection, EffectiveSettings, FileChange, ModelDescriptor, ModelOptionKind,
    ModelOptionRole, ProviderId, ProviderUnavailability, SessionId,
};

mod codex;
mod copilot;
pub(crate) mod harness;
mod orchestration;
mod reasoning;

pub use codex::CodexRuntime;
pub use copilot::CopilotRuntime;
pub(crate) use orchestration::{ProviderOrchestrator, ProviderUpdateGate};

/// A Provider-authored message is user-visible once it surfaces as a Provider failure, so it is
/// capped.
const MAX_REMOTE_ERROR_CHARS: usize = 384;

/// Collapses a Provider-authored message onto a single bounded line fit for a Provider failure.
pub(crate) fn concise_remote_message(message: &str, fallback: &str) -> String {
    let single_line = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let message = if single_line.is_empty() {
        fallback
    } else {
        &single_line
    };
    let mut chars = message.chars();
    let mut concise = chars
        .by_ref()
        .take(MAX_REMOTE_ERROR_CHARS)
        .collect::<String>();
    if chars.next().is_some() {
        concise.push('\u{2026}');
    }
    concise
}

/// Resolves a Provider's harness executable: the `variable` override first, then `name` for the
/// PATH to answer. Every Provider follows this convention, and Suru installs or updates none of
/// them — the user owns their own CLI.
pub(crate) fn resolve_executable(variable: &str, name: &str) -> std::ffi::OsString {
    std::env::var_os(variable)
        .filter(|path| !path.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from(name))
}

/// The reading version of an identifier a Provider names in wire case, such as `long_context`.
pub(crate) fn humanized_wire_id(value: &str) -> String {
    let spaced = value.replace(['_', '-'], " ");
    let mut characters = spaced.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().chain(characters).collect(),
        None => String::new(),
    }
}

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
    /// The Provider itself cannot be used yet, for a reason the user fixes
    /// outside Suru. Carried on the error so whatever asked the runtime to
    /// work — Model discovery above all — can report the condition rather
    /// than a bare failure.
    Unavailable(ProviderUnavailability),
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

    pub fn unavailable(reason: ProviderUnavailability, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            session_lost: false,
            kind: ProviderErrorKind::Unavailable(reason),
        }
    }

    pub(crate) fn mark_session_lost(mut self) -> Self {
        self.session_lost = true;
        self
    }

    pub(crate) fn is_session_lost(&self) -> bool {
        self.session_lost
    }

    pub(crate) fn is_selection_rejected(&self) -> bool {
        self.kind == ProviderErrorKind::SelectionRejected
    }

    /// The typed reason the Provider is unusable, when the failure carries one.
    pub(crate) fn unavailability(&self) -> Option<ProviderUnavailability> {
        match self.kind {
            ProviderErrorKind::Unavailable(reason) => Some(reason),
            ProviderErrorKind::Failure | ProviderErrorKind::SelectionRejected => None,
        }
    }

    pub(crate) fn mark_unavailable(mut self, reason: ProviderUnavailability) -> Self {
        self.kind = ProviderErrorKind::Unavailable(reason);
        self
    }

    /// Restates the failure in `message`, keeping how it is classified. Lets a
    /// Provider wrap a failure in the operation that met it without having to
    /// know — and re-apply — every facet the original carried.
    pub(crate) fn reworded(mut self, message: impl Into<String>) -> Self {
        self.message = message.into();
        self
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

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, sync::Mutex};

    use super::resolve_executable;

    /// Every Provider's binary resolution shares this scope, so they share its lock.
    static ENVIRONMENT: Mutex<()> = Mutex::new(());

    const FIXTURE_PATH_ENV: &str = "SURU_FIXTURE_PROVIDER_PATH";

    #[test]
    fn an_executable_resolves_from_the_override_and_otherwise_from_path() {
        let _environment = ENVIRONMENT
            .lock()
            .expect("Provider environment test lock is not poisoned");
        let original = std::env::var_os(FIXTURE_PATH_ENV);

        // SAFETY: this unit test serializes every mutation of this process variable and restores it
        // before releasing the lock. No production task is running in the unit-test process.
        unsafe {
            std::env::set_var(FIXTURE_PATH_ENV, "/fixture/custom-harness");
        }
        assert_eq!(
            resolve_executable(FIXTURE_PATH_ENV, "harness"),
            OsString::from("/fixture/custom-harness")
        );

        // SAFETY: covered by the serialized test scope described above.
        unsafe {
            std::env::set_var(FIXTURE_PATH_ENV, "");
        }
        assert_eq!(
            resolve_executable(FIXTURE_PATH_ENV, "harness"),
            OsString::from("harness"),
            "an empty override is no override"
        );

        // SAFETY: covered by the serialized test scope described above.
        unsafe {
            std::env::remove_var(FIXTURE_PATH_ENV);
        }
        assert_eq!(
            resolve_executable(FIXTURE_PATH_ENV, "harness"),
            OsString::from("harness")
        );

        // SAFETY: restore the exact environment observed before the serialized test scope.
        unsafe {
            match original {
                Some(original) => std::env::set_var(FIXTURE_PATH_ENV, original),
                None => std::env::remove_var(FIXTURE_PATH_ENV),
            }
        }
    }
}
