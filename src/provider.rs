//! Provider-neutral runtime and per-Session execution interfaces.

use std::{
    error::Error,
    fmt,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, OnceLock},
};

use futures_util::Stream;
use serde_json::Value;
use tokio::sync::watch;

use crate::protocol::{
    AgentIdentity, AgentSelection, Cost, EffectiveSettings, FileChange, ModelDescriptor,
    ModelOptionKind, ModelOptionRole, ProviderId, ProviderUnavailability, SkillCatalog,
    SkillCatalogCapabilities, SkillCatalogStatus, SkillId, SkillInvocation, SkillMarkerSpan,
    SkillPromptDelivery, Usage, Workspace,
};

mod claude;
mod codex;
mod copilot;
pub(crate) mod harness;
mod orchestration;
mod reasoning;
mod shell_wrapper;
mod version;

pub use claude::ClaudeRuntime;
pub use codex::CodexRuntime;
pub use copilot::CopilotRuntime;
pub(crate) use orchestration::{ProviderOrchestrator, ProviderUpdateGate};

/// What one successful Provider catalog discovery found. Models are the
/// selectable inventory every Provider supplies; `warning` is a non-blocking
/// compatibility condition the Provider wants surfaced alongside that
/// inventory without making it unavailable.
pub struct ProviderModelDiscovery {
    pub models: Vec<ModelDescriptor>,
    pub warning: Option<String>,
}

impl ProviderModelDiscovery {
    pub fn new(models: Vec<ModelDescriptor>) -> Self {
        Self {
            models,
            warning: None,
        }
    }

    pub fn with_warning(mut self, warning: impl Into<String>) -> Self {
        self.warning = Some(warning.into());
        self
    }
}

/// One built-in Provider as a surface listing Providers reads it: which
/// Provider it is, and what its runtime calls it.
pub struct BuiltInProvider {
    pub id: ProviderId,
    pub display_name: String,
    /// Whether one of this Provider's working Subagents can be stopped on its
    /// own. Read off the runtime's own declaration, so the surface offering
    /// the stop and the server honoring it can never disagree.
    pub supports_subagent_stop: bool,
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
                supports_subagent_stop: runtime.supports_subagent_stop(),
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
    Pin<Box<dyn Stream<Item = Result<AttributedProviderEvent, ProviderError>> + Send>>;

/// One Provider event together with the attribution naming the Session it
/// lands in. A Provider knows nothing of Suru Sessions, so the attribution
/// speaks in the Provider's own terms — the conversation itself, or a Subagent
/// it delegated to — and orchestration resolves it to a Session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttributedProviderEvent {
    pub attribution: ProviderEventAttribution,
    pub event: ProviderEvent,
}

impl From<ProviderEvent> for AttributedProviderEvent {
    fn from(event: ProviderEvent) -> Self {
        Self {
            attribution: ProviderEventAttribution::OwningSession,
            event,
        }
    }
}

/// Names the Session a Provider event lands in.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ProviderEventAttribution {
    /// The Session that owns the Provider connection — the conversation the
    /// Provider was started for, and the attribution every event defaults to.
    OwningSession,
    /// A Subagent the conversation's agent delegated work to, named by the
    /// Provider's own identity for the delegation. Orchestration resolves the
    /// identity to that Subagent's own Session; an identity it holds no
    /// Session for lands nowhere.
    Subagent(ProviderSubagentId),
}

/// The Provider's own opaque identity for one Subagent it is running —
/// Claude's spawning tool-use id, Codex's child thread id, Copilot's agent
/// id. Like [`ProviderActivityId`], it is the Provider's to choose:
/// orchestration only ever compares it, and only the Provider that minted it
/// reads it back, as a stop request hands it the identity to resolve.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ProviderSubagentId(String);

impl ProviderSubagentId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

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
pub struct ProviderPrompt {
    pub text: String,
    pub skill_invocations: Vec<ProviderSkillInvocation>,
}

impl ProviderPrompt {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            skill_invocations: Vec::new(),
        }
    }

    pub(crate) fn from_user_prompt(text: String, skill_invocations: Vec<SkillInvocation>) -> Self {
        let mut ordered = Vec::<ProviderSkillInvocation>::new();
        for invocation in skill_invocations {
            if let Some(existing) = ordered
                .iter_mut()
                .find(|existing| existing.skill_id == invocation.skill_id)
            {
                existing.marker_spans.push(invocation.marker);
            } else {
                ordered.push(ProviderSkillInvocation {
                    skill_id: invocation.skill_id,
                    marker_spans: vec![invocation.marker],
                });
            }
        }
        Self {
            text,
            skill_invocations: ordered,
        }
    }

    pub(crate) fn without_skill_markers(
        self,
        provider_name: &str,
    ) -> Result<String, ProviderError> {
        let mut spans = self
            .skill_invocations
            .into_iter()
            .flat_map(|invocation| invocation.marker_spans)
            .collect::<Vec<_>>();
        spans.sort_by_key(|span| std::cmp::Reverse((span.start, span.end)));

        let mut text = self.text;
        let mut next_start = text.len();
        for span in spans {
            let start = span.start as usize;
            let end = span.end as usize;
            if start >= end
                || end > next_start
                || !text.is_char_boundary(start)
                || !text.is_char_boundary(end)
            {
                return Err(ProviderError::new(format!(
                    "{provider_name} Skill Invocation contains an invalid marker range"
                )));
            }
            text.replace_range(start..end, "");
            next_start = start;
        }
        Ok(text)
    }
}

/// One distinct Provider-neutral Skill Invocation, in first-appearance order,
/// with every marker that selected it retained for Provider-specific lowering.
/// Native paths and command names remain inside the Provider adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderSkillInvocation {
    pub skill_id: SkillId,
    pub marker_spans: Vec<SkillMarkerSpan>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderTurnInput {
    pub prompt: ProviderPrompt,
    pub selection: AgentSelection,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderSteerInput {
    pub prompt: ProviderPrompt,
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

/// How a Provider reported one of its Subagents settling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderSubagentStatus {
    Completed,
    Failed,
    /// The Subagent was stopped rather than finishing or failing — by a stop
    /// Suru asked for, or by one the Provider ran on its own account.
    Interrupted,
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
    /// The Turn's Agent delegated work to a Subagent. The event lands in the
    /// spawner's Session — the owning Session, or a Subagent's own for a
    /// nested spawn — and orchestration answers it by opening the Subagent's
    /// child Session and the row that stands for it.
    SubagentStarted {
        subagent_id: ProviderSubagentId,
        name: String,
        description: String,
    },
    /// The Provider revised what a working Subagent is doing.
    SubagentUpdated {
        subagent_id: ProviderSubagentId,
        description: String,
    },
    /// The Provider reported a Subagent settling. This settles the Subagent's
    /// row and its child Session's Turn together; events attributed to the
    /// Subagent after it land nowhere.
    SubagentCompleted {
        subagent_id: ProviderSubagentId,
        status: ProviderSubagentStatus,
    },
    /// The Provider's latest complete reading of the active Turn. Each absent
    /// field remains absent through the protocol; a stated Cost is frozen by
    /// the Session store with a Reported Basis.
    Usage {
        usage: Usage,
        reported_cost: Option<Cost>,
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

/// The cumulative Usage and Provider-reported Cost for one active Turn. A
/// field stays known only while every contributing Provider report states a
/// valid value; overflow and omission degrade that field to absence rather
/// than manufacturing a number.
pub(super) struct ReportedTurnMetering {
    usage: Usage,
    reported_cost: Option<Cost>,
    native_meter_overflowed: bool,
}

impl ReportedTurnMetering {
    pub(super) fn new(usage: Usage, reported_cost: Option<Cost>) -> Self {
        Self {
            usage,
            reported_cost,
            native_meter_overflowed: false,
        }
    }

    pub(super) fn add(&mut self, usage: Usage, reported_cost: Option<Cost>) {
        self.usage.fresh_input_tokens =
            add_reported_counts(self.usage.fresh_input_tokens, usage.fresh_input_tokens);
        self.usage.cache_read_tokens =
            add_reported_counts(self.usage.cache_read_tokens, usage.cache_read_tokens);
        self.usage.cache_write_tokens =
            add_reported_counts(self.usage.cache_write_tokens, usage.cache_write_tokens);
        self.usage.output_tokens =
            add_reported_counts(self.usage.output_tokens, usage.output_tokens);
        self.usage.reasoning_tokens =
            add_reported_counts(self.usage.reasoning_tokens, usage.reasoning_tokens);
        if !self.native_meter_overflowed {
            self.usage.native_meter = match (self.usage.native_meter, usage.native_meter) {
                (Some(current), Some(next)) => match current.checked_add(next) {
                    Some(total) => Some(total),
                    None => {
                        self.native_meter_overflowed = true;
                        None
                    }
                },
                (Some(current), None) => Some(current),
                (None, Some(next)) => Some(next),
                (None, None) => None,
            };
        }
        self.usage.model_context_window =
            match (self.usage.model_context_window, usage.model_context_window) {
                (Some(current), Some(next)) => Some(current.max(next)),
                (Some(current), None) => Some(current),
                (None, Some(next)) => Some(next),
                (None, None) => None,
            };
        self.reported_cost = self
            .reported_cost
            .zip(reported_cost)
            .and_then(|(current, next)| current.checked_add(next));
    }

    pub(super) fn event(&self) -> ProviderEvent {
        ProviderEvent::Usage {
            usage: self.usage.clone(),
            reported_cost: self.reported_cost,
        }
    }
}

fn add_reported_counts(current: Option<u64>, next: Option<u64>) -> Option<u64> {
    current
        .zip(next)
        .and_then(|(current, next)| current.checked_add(next))
}

pub trait ProviderRuntime: Send + Sync + 'static {
    fn provider_id(&self) -> ProviderId;

    /// The Provider's name as the user reads it — "Codex", never `codex`.
    /// Declared here so the runtime is the single source of the name every
    /// surface prints, and required rather than defaulted so a new Provider
    /// cannot ship without one.
    fn display_name(&self) -> &str;

    fn list_models(&self) -> ProviderFuture<'_, ProviderModelDiscovery>;

    /// Offers the effective user-invocable Skill Catalog for exactly one
    /// Workspace. Discovery and native identifiers remain inside the Provider;
    /// the generic boundary returns only opaque identities and safe metadata.
    /// Providers may inherit the unavailable catalog while their adapter has no
    /// Skill implementation, which keeps ordinary Prompt behavior independent
    /// of Skill discovery.
    fn skill_catalog(&self, workspace: &Path) -> ProviderFuture<'_, SkillCatalog> {
        let catalog = SkillCatalog {
            provider: self.provider_id(),
            workspace: Workspace {
                path: workspace.to_owned(),
            },
            skills: Vec::new(),
            capabilities: SkillCatalogCapabilities {
                max_distinct_invocations: None,
                supported_deliveries: Vec::new(),
            },
            status: SkillCatalogStatus::Unavailable {
                message: "Skill discovery is not implemented for this Provider".to_owned(),
            },
        };
        Box::pin(async move { Ok(catalog) })
    }

    /// Forces the Provider to refresh its native Skill authority. Providers
    /// without a native cache inherit ordinary discovery; adapters such as
    /// Codex override this to request their native force-refresh operation.
    fn refresh_skill_catalog(&self, workspace: &Path) -> ProviderFuture<'_, SkillCatalog> {
        self.skill_catalog(workspace)
    }

    /// Adds Provider-specific recovery guidance when a Skill delivery mode is unavailable.
    /// Capability enforcement stays generic; only the adapter knows whether another delivery is
    /// a meaningful alternative for its native invocation mechanism.
    fn skill_delivery_rejection_guidance(
        &self,
        _delivery: SkillPromptDelivery,
    ) -> Option<&'static str> {
        None
    }

    /// Reports Provider-native Skill changes as invalidations. The generation
    /// value is deliberately opaque: the server refreshes every cached
    /// Workspace for this Provider instead of interpreting native details.
    fn subscribe_skill_catalog_invalidations(&self) -> Option<watch::Receiver<u64>> {
        None
    }

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

    /// Whether this Provider can stop one working Subagent on its own,
    /// leaving the rest of the Session running. Defaulted to `false` so a
    /// Provider without the capability offers nothing, and every surface that
    /// would offer the stop reads this one declaration.
    fn supports_subagent_stop(&self) -> bool {
        false
    }

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

    /// Interrupts the running Turn. A Provider stops the background work the
    /// Turn spawned — its Subagents included — before it stops the loop, the
    /// established ordering, because an interrupt alone leaves that work
    /// running.
    fn interrupt_turn(&self) -> ProviderFuture<'_, ()>;

    /// Stops every Subagent still working under this connection, for the
    /// interrupt that arrives after the Turn settled and finds only Subagents
    /// running. Required rather than defaulted, so a new Provider has to
    /// decide what stopping its late-running delegations means.
    fn stop_subagents(&self) -> ProviderFuture<'_, ()>;

    /// Stops the one working Subagent the identity names, leaving everything
    /// else running. Defaulted to a refusal to match the runtime's
    /// [`ProviderRuntime::supports_subagent_stop`] default; a runtime that
    /// declares the capability overrides this with its native stop.
    fn stop_subagent(&self, subagent_id: ProviderSubagentId) -> ProviderFuture<'_, ()> {
        let _ = subagent_id;
        Box::pin(async {
            Err(ProviderError::new(
                "This Provider offers no per-Subagent stop",
            ))
        })
    }

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
