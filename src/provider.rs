//! Provider-neutral runtime and per-Session execution interfaces.

use std::{
    error::Error,
    fmt,
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, OnceLock},
};

use futures_util::Stream;
use serde_json::Value;
use tokio::sync::watch;

use crate::protocol::{
    AgentIdentity, AgentSelection, EffectiveSettings, FileChange, ModelDescriptor, ModelOptionKind,
    ModelOptionRole, ProviderId, ProviderUnavailability,
};

mod claude;
mod codex;
mod copilot;
pub(crate) mod harness;
mod orchestration;
mod reasoning;
mod shell_wrapper;

pub use claude::ClaudeRuntime;
pub use codex::CodexRuntime;
pub use copilot::CopilotRuntime;
pub(crate) use orchestration::{ProviderOrchestrator, ProviderUpdateGate};

/// One built-in Provider as a surface listing Providers reads it: which
/// Provider it is, and what its runtime calls it.
pub struct BuiltInProvider {
    pub id: ProviderId,
    pub display_name: String,
}

/// The Provider runtimes a production server hosts, in the fixed built-in
/// order a fresh Landing defaults from. This is the one place Suru names a
/// concrete Provider.
pub fn built_in_runtimes() -> Vec<Arc<dyn ProviderRuntime>> {
    vec![
        Arc::new(CodexRuntime::from_environment()),
        Arc::new(CopilotRuntime::from_environment()),
        // Appended last so the Landing default order the earlier Providers set is unchanged.
        Arc::new(ClaudeRuntime::from_environment()),
    ]
}

/// The built-in Providers as a client lists them, in that same fixed order.
/// Read off the runtimes themselves, so a surface presenting Providers can
/// neither miss one the server hosts nor call one a name no runtime answers
/// to. Built once, because neither the set nor a name changes while Suru runs.
pub fn built_in_providers() -> &'static [BuiltInProvider] {
    static PROVIDERS: OnceLock<Vec<BuiltInProvider>> = OnceLock::new();
    PROVIDERS.get_or_init(|| {
        built_in_runtimes()
            .iter()
            .map(|runtime| BuiltInProvider {
                id: runtime.provider_id(),
                display_name: runtime.display_name().to_owned(),
            })
            .collect()
    })
}

/// A Provider-authored message is user-visible once it surfaces as a Provider failure, so it is
/// capped.
const MAX_REMOTE_ERROR_CHARS: usize = 1_024;

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

/// What a runtime needs to start one Provider-side session. It deliberately
/// names no Suru Session: a runtime fulfilling an Errand through the
/// session-shaped fallback (ADR 0011) has no Suru Session to name, and no
/// harness ever read the identifier this used to carry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderSessionRequest {
    pub workspace: PathBuf,
    pub resume_state: Option<ProviderResumeState>,
}

/// One Errand: a single Provider call Suru makes for its own purposes rather
/// than the user's. It carries one Prompt, the shape the answer should take,
/// and the Agent Selection to run under — and no Tools, no Session, and nothing
/// the Provider is expected to remember afterwards.
///
/// The schema is a request rather than a guarantee: a runtime falling back to a
/// Provider-side session has no way to be handed one, so whatever asks for an
/// Errand validates the reply itself (ADR 0011).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderErrand {
    pub prompt: String,
    pub schema: Value,
    pub selection: AgentSelection,
    /// The directory the Errand runs in, so a Workspace's own agent
    /// instructions can inform the answer and no harness refuses to run
    /// outside a repository.
    pub workspace: PathBuf,
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

    /// The Provider's name as the user reads it — "Codex", never `codex`.
    /// Declared here so the runtime is the single source of the name every
    /// surface prints, and required rather than defaulted so a new Provider
    /// cannot ship without one.
    fn display_name(&self) -> &str;

    fn list_models(&self) -> ProviderFuture<'_, Vec<ModelDescriptor>>;

    fn start_session(
        &self,
        request: ProviderSessionRequest,
    ) -> ProviderFuture<'_, ProviderSessionConnection>;

    /// Runs one Errand: delivers its Prompt, asks for its shape, and answers
    /// with the JSON the Provider replied. How that happens is the runtime's
    /// own business — a native one-shot mode where the harness offers one, and
    /// otherwise a Provider-side session it starts and discards (ADR 0011) —
    /// and the caller never learns which path ran.
    ///
    /// This knows nothing about Titles. It is a general "run this once, answer
    /// in this shape" capability, so a later compaction or summarization Errand
    /// reuses it unchanged. It is not defaulted, so a new Provider has to decide
    /// how it runs Errands rather than silently running none.
    fn run_errand(&self, errand: ProviderErrand) -> ProviderFuture<'_, Value>;

    /// The Errand Selection this Provider declares: the Model its own Errands
    /// run at, chosen for cheapness and speed rather than capability, and the
    /// Model Options to run it under. Nothing, for a Provider that knows
    /// nothing cheaper than the Model it already defaults to — whoever asks for
    /// an Errand then falls back to that default.
    ///
    /// It is a whole Agent Selection rather than a Model identifier because
    /// nothing in the Model Option types conveys magnitude: reasoning efforts
    /// are relayed in the Provider's own publication order under the Provider's
    /// own wire identifiers, so "the least effort" is not derivable and has to
    /// be declared by the runtime that understands its own vocabulary. Matching
    /// effort identifiers by convention instead would break silently the day a
    /// Provider ships an effort named something new.
    ///
    /// This is a declaration and not a resolution: the Model it names may have
    /// been withdrawn since, and whoever asks for an Errand resolves it against
    /// the live catalog every time. It is deliberately separate from
    /// [`ModelDescriptor::is_default`] — the Model a user should converse with
    /// and the Model that should write six words are different questions. It is
    /// not defaulted, so a new Provider has to answer the second one.
    fn errand_selection(&self) -> Option<AgentSelection>;

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

    /// Takes the connection apart. Public because a runtime fulfilling an
    /// Errand through the session-shaped fallback (ADR 0011) starts a
    /// Provider-side session of its own, drives it, and discards it — which
    /// means taking apart a connection it never hands to a caller.
    pub fn into_parts(
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

    use super::{
        MAX_REMOTE_ERROR_CHARS, built_in_providers, built_in_runtimes, concise_remote_message,
        resolve_executable,
    };
    use crate::{protocol::EffectiveSettings, settings::provider_enablement};

    /// Every Provider's binary resolution shares this scope, so they share its lock.
    static ENVIRONMENT: Mutex<()> = Mutex::new(());

    const FIXTURE_PATH_ENV: &str = "SURU_FIXTURE_PROVIDER_PATH";

    #[test]
    fn a_remote_error_keeps_actionable_detail_past_the_old_cap() {
        let message = format!("{} invalid_json_schema", "x".repeat(500));

        assert!(
            concise_remote_message(&message, "provider failed").ends_with("invalid_json_schema"),
            "the Provider's actionable suffix survives its diagnostic preamble"
        );
    }

    #[test]
    fn an_overlong_remote_error_stays_bounded_and_marked() {
        let concise =
            concise_remote_message(&"x".repeat(MAX_REMOTE_ERROR_CHARS + 1), "provider failed");

        assert_eq!(concise.chars().count(), MAX_REMOTE_ERROR_CHARS + 1);
        assert!(concise.ends_with('\u{2026}'));
    }

    /// The other guard a Provider must clear before shipping: client surfaces
    /// print whatever name the runtime declares, so a Provider with a blank
    /// display name would surface to the user as nothing at all. The trait
    /// makes declaring one mandatory; this guards what is declared, and that
    /// the list clients read carries every Provider the server hosts.
    #[test]
    fn every_built_in_provider_has_a_display_name() {
        for runtime in built_in_runtimes() {
            let provider = runtime.provider_id();
            assert!(
                !runtime.display_name().trim().is_empty(),
                "Provider `{provider}` declares a blank display name"
            );
            assert!(
                built_in_providers()
                    .iter()
                    .any(|listed| listed.id == provider
                        && listed.display_name == runtime.display_name()),
                "Provider `{provider}` is missing from the list clients read"
            );
        }
    }

    /// A Provider added to the built-in set without an `enabled` Setting would
    /// be one the user cannot turn off, and would read as enabled forever
    /// through the fallback [`EffectiveSettings::provider_enabled`] keeps for
    /// Providers the schema does not name — a failure that presents as nothing
    /// at all. Enablement is a hand-written table rather than a compile-time
    /// one, so this is the guard that walks a developer to the schema entry,
    /// the mutation, and the settings field. Its other half, that such an entry
    /// actually reaches the gate, lives beside the schema in `settings`.
    #[test]
    fn every_built_in_provider_has_an_enabled_setting() {
        for runtime in built_in_runtimes() {
            let provider = runtime.provider_id();
            assert!(
                provider_enablement(&provider).is_some(),
                "Provider `{provider}` has no provider.{provider}.enabled Setting, so nothing can turn it off"
            );
            assert!(
                EffectiveSettings::default().provider_enabled(&provider),
                "Provider `{provider}` must be enabled unless the user says otherwise"
            );
        }
    }

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
