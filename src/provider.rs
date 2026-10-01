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

use crate::pricing::{EstimatedCost, PricingSource};
use crate::protocol::{
    AgentIdentity, AgentSelection, ApprovalPosture, Cost, CostBasis, CostCoverage,
    EffectiveSettings, FileChange, ModelDescriptor, ModelOptionKind, ModelOptionRole, ProviderId,
    ProviderUnavailability, SkillCatalog, SkillCatalogCapabilities, SkillCatalogStatus, SkillId,
    SkillInvocation, SkillPromptDelivery, TextSpan, Usage,
};

mod claude;
mod codex;
mod command_presentation;
mod copilot;
pub(crate) mod harness;
mod orchestration;
mod reasoning;
mod report;
mod tool_call_presentation;
mod version;

pub use crate::broker::{BrokerEndpoint, BrokerHandoff, BrokerToken};
pub use claude::ClaudeRuntime;
pub use codex::CodexRuntime;
pub use copilot::CopilotRuntime;
pub(crate) use orchestration::{
    BrokeredDelivery, BrokeredSendRefusal, BrokeredSpawnRefusal, BrokeredStop,
    BrokeredSubagentRequest, ProviderOrchestrator, ProviderUpdateGate,
};
pub use report::{SubagentReport, SubagentReportOutcome};

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

/// One built-in Provider as client surfaces present it: its identity, name,
/// optional icon, and interaction capabilities.
pub struct BuiltInProvider {
    pub id: ProviderId,
    pub display_name: String,
    /// The Nerd Font glyph this Provider uses on icon-enabled client
    /// surfaces. Optional at the generic boundary so a Provider without a
    /// suitable glyph still has a complete text presentation.
    pub nerd_font_icon: Option<char>,
    /// Whether one of this Provider's working Subagents can be stopped on its
    /// own. Read off the runtime's own declaration, so the surface offering
    /// the stop and the server honoring it can never disagree.
    pub supports_subagent_stop: bool,
    /// How far this Provider compacts a Session's context on request, read
    /// off the runtime's own declaration for the same reason, so `/compact`
    /// is explained before it is sent wherever the answer is already known.
    pub manual_compaction: ManualCompaction,
}

/// How far a Provider compacts a Session's context when the user asks, which
/// `/compact` needs of it. Every Provider compacts when it chooses to; this
/// says only whether it also does so on request. A Provider declares nothing
/// unless it implements [`ProviderSession::compact`], so one added later
/// works without it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ManualCompaction {
    /// It compacts only when it chooses, so a request is refused rather than
    /// left waiting on nothing.
    #[default]
    Unsupported,
    /// It compacts when asked, though it takes no instructions on what the
    /// summary should keep.
    Supported,
}

impl ManualCompaction {
    pub const fn is_supported(self) -> bool {
        matches!(self, Self::Supported)
    }

    /// Whether a request may carry instructions for the summary. No Provider
    /// takes them yet, so a request carrying any is refused rather than
    /// having them dropped.
    pub const fn takes_instructions(self) -> bool {
        false
    }

    /// What this capability alone refuses a Compaction request for, carrying
    /// instructions or not: the one question the server answers before it
    /// begins a Turn and a client before it sends anything, so the two never
    /// disagree.
    pub const fn refusal(self, with_instructions: bool) -> Option<ManualCompactionRefusal> {
        if !self.is_supported() {
            Some(ManualCompactionRefusal::Unsupported)
        } else if with_instructions && !self.takes_instructions() {
            Some(ManualCompactionRefusal::InstructionsUnsupported)
        } else {
            None
        }
    }
}

/// Why a Provider's declared [`ManualCompaction`] refuses a request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManualCompactionRefusal {
    /// The Provider compacts only when it chooses to.
    Unsupported,
    /// The request carries instructions for the summary, and the Provider
    /// takes none.
    InstructionsUnsupported,
}

/// The Provider runtimes a production server hosts, in the fixed built-in
/// order a fresh Landing defaults from. This is the one place Suru names a
/// concrete Provider. `data_dir` is where durable Provider-neutral caches —
/// the models.dev rate table a Cost is estimated from — live between runs.
pub fn built_in_runtimes(data_dir: &Path) -> Vec<Arc<dyn ProviderRuntime>> {
    runtimes(Some(Arc::new(PricingSource::new(data_dir))))
}

/// The same set, with the rate table left out for callers that only ask the
/// runtimes what they are called. Nothing built this way runs a Turn, so
/// nothing built this way needs somewhere to cache prices.
fn runtimes(pricing: Option<Arc<PricingSource>>) -> Vec<Arc<dyn ProviderRuntime>> {
    let codex = CodexRuntime::from_environment();
    vec![
        Arc::new(match pricing {
            Some(pricing) => codex.with_pricing_source(pricing),
            None => codex,
        }),
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
        runtimes(None)
            .iter()
            .map(|runtime| BuiltInProvider {
                id: runtime.provider_id(),
                display_name: runtime.display_name().to_owned(),
                nerd_font_icon: runtime.nerd_font_icon(),
                supports_subagent_stop: runtime.supports_subagent_stop(),
                manual_compaction: runtime.manual_compaction(),
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
        .unwrap_or_else(|| {
            #[cfg(windows)]
            if let (Some(path), Some(local_app_data)) = (
                std::env::var_os("PATH"),
                std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from),
            ) {
                let windows_apps = local_app_data.join("Microsoft").join("WindowsApps");
                if let Some(executable) =
                    executable_shadowed_by_windows_app_alias(name, &path, &windows_apps)
                {
                    return executable;
                }
            }

            std::ffi::OsString::from(name)
        })
}

/// Finds the next executable on `PATH` when a Windows app execution alias would otherwise win.
///
/// The Microsoft Store's consumer Copilot app publishes `copilot.exe` under `WindowsApps`, the
/// same command name as GitHub Copilot CLI. Launching that app alias as a child process fails with
/// `APPMODEL_ERROR_NO_PACKAGE` on Windows, even when a working CLI appears later on `PATH`. Leave
/// ordinary command lookup untouched unless that exact class of alias is the first match.
#[cfg(windows)]
fn executable_shadowed_by_windows_app_alias(
    name: &str,
    path: &std::ffi::OsStr,
    windows_apps: &std::path::Path,
) -> Option<std::ffi::OsString> {
    let mut found_app_alias = false;

    for directory in std::env::split_paths(path) {
        for candidate in windows_executable_candidates(&directory, name) {
            if !candidate.is_file() {
                continue;
            }
            if candidate
                .parent()
                .is_some_and(|parent| windows_paths_equal(parent, windows_apps))
            {
                found_app_alias = true;
                continue;
            }
            if found_app_alias {
                return Some(candidate.into_os_string());
            }
            // The normal lookup found a command before WindowsApps, so there is no alias to bypass
            // and Command should retain its native lookup behavior.
            return None;
        }
    }
    None
}

#[cfg(windows)]
fn windows_executable_candidates(
    directory: &std::path::Path,
    name: &str,
) -> Vec<std::path::PathBuf> {
    let candidate = directory.join(name);
    if std::path::Path::new(name).extension().is_some() {
        vec![candidate]
    } else {
        vec![candidate, directory.join(format!("{name}.exe"))]
    }
}

#[cfg(windows)]
fn windows_paths_equal(left: &std::path::Path, right: &std::path::Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
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

/// How a resume's row describes it when the Provider's resume carries a message and no summary
/// of it (ADR 0031): the first non-empty line of the Delegation, or nothing when it has none.
pub(crate) fn first_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

pub type ProviderFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, ProviderError>> + Send + 'a>>;
pub type ProviderEventStream =
    Pin<Box<dyn Stream<Item = Result<AttributedProviderEvent, ProviderError>> + Send>>;

/// A Decision the Provider has definitively received, together with any work
/// that must wait until Suru has durably recorded that delivery. Owning the
/// follow-up future also owns its cleanup: cancellation drops it rather than
/// stranding a Provider-side stream barrier.
pub struct ProviderDecisionDelivery {
    after_settled: ProviderFuture<'static, ()>,
}

impl ProviderDecisionDelivery {
    pub fn complete() -> Self {
        Self::with_follow_up(Box::pin(async { Ok(()) }))
    }

    pub fn with_follow_up(after_settled: ProviderFuture<'static, ()>) -> Self {
        Self { after_settled }
    }

    pub(crate) async fn finish(self) -> Result<(), ProviderError> {
        self.after_settled.await
    }
}

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

/// The Provider's own opaque identity for one Subagent it runs — Claude's
/// task id, Codex's child thread id, Copilot's agent id. It names the agent
/// rather than any one stretch of its work, so a resume of a settled Subagent
/// names it again. Like [`ProviderActivityId`], it is the Provider's to
/// choose: orchestration only ever compares it, and only the Provider that
/// minted it reads it back, as a stop request hands it the identity to
/// resolve.
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

/// The Provider's own opaque identity for one Watch it runs — Claude's task
/// id for a background shell or a Monitor. It is unique within the Provider
/// connection that minted it, and orchestration only ever compares it: a
/// Watch's settle names the Watch its start did.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProviderWatchId(String);

impl ProviderWatchId {
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
    QuestionnaireRejected,
    DecisionRejected,
    /// The Provider knows the native request was already resolved elsewhere.
    DecisionWithdrawn,
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

    /// Delivery definitely did not occur and the native request remains live.
    /// Only this failure permits an explicit Questionnaire retry.
    pub fn questionnaire_rejected(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            session_lost: false,
            kind: ProviderErrorKind::QuestionnaireRejected,
        }
    }

    pub(crate) fn is_questionnaire_rejected(&self) -> bool {
        self.kind == ProviderErrorKind::QuestionnaireRejected
    }

    /// Delivery definitely did not occur and the native Approval remains live.
    /// Only this failure permits an explicit Decision retry.
    pub fn decision_rejected(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            session_lost: false,
            kind: ProviderErrorKind::DecisionRejected,
        }
    }

    pub(crate) fn is_decision_rejected(&self) -> bool {
        self.kind == ProviderErrorKind::DecisionRejected
    }

    pub fn decision_withdrawn(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            session_lost: false,
            kind: ProviderErrorKind::DecisionWithdrawn,
        }
    }

    pub(crate) fn is_decision_withdrawn(&self) -> bool {
        self.kind == ProviderErrorKind::DecisionWithdrawn
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
            ProviderErrorKind::Failure
            | ProviderErrorKind::SelectionRejected
            | ProviderErrorKind::QuestionnaireRejected
            | ProviderErrorKind::DecisionRejected
            | ProviderErrorKind::DecisionWithdrawn => None,
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
    pub execution_directory: PathBuf,
    pub resume_state: Option<ProviderResumeState>,
    pub approval_posture: Option<ApprovalPosture>,
    /// The Broker endpoint and the bearer token naming the Session this start
    /// is for, which the harness offers its Agent as an MCP server beside its
    /// own Tools. The token is minted for this start alone and retired when
    /// the connection it opens closes. `None` while the `broker.enabled`
    /// Setting is off, and always for a session an Errand opens, since
    /// Errands carry no Tools.
    pub broker: Option<BrokerHandoff>,
}

/// One Errand: a single Provider call Suru makes for its own purposes rather
/// than the user's. It carries one Prompt, the shape the answer should take,
/// and the Agent Selection to run under — and no Tools, no Session, and nothing
/// the Provider is expected to remember afterwards. So it carries no Broker
/// handoff either: a runtime running an Errand through a session of its own
/// starts that session with `broker: None`.
///
/// The schema is a request rather than a guarantee: a runtime falling back to a
/// Provider-side session has no way to be handed one, so whatever asks for an
/// Errand validates the reply itself (ADR 0011).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderErrand {
    pub prompt: String,
    pub schema: Value,
    pub selection: AgentSelection,
    /// The directory the Errand runs in, so the Execution Directory's agent
    /// instructions can inform the answer and no harness refuses to run
    /// outside a repository.
    pub execution_directory: PathBuf,
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
    /// The Attachments the Prompt carries, in the order their labels stand in
    /// `text`, which keeps every label literal.
    pub attachments: Vec<ProviderAttachment>,
}

impl ProviderPrompt {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
        }
    }

    pub(crate) fn from_user_prompt(
        text: String,
        skill_invocations: Vec<SkillInvocation>,
        attachments: Vec<ProviderAttachment>,
    ) -> Self {
        let mut ordered = Vec::<ProviderSkillInvocation>::new();
        for invocation in skill_invocations {
            if let Some(existing) = ordered
                .iter_mut()
                .find(|existing| existing.skill_id == invocation.skill_id)
            {
                existing.spans.push(invocation.span);
            } else {
                ordered.push(ProviderSkillInvocation {
                    skill_id: invocation.skill_id,
                    spans: vec![invocation.span],
                });
            }
        }
        Self {
            text,
            skill_invocations: ordered,
            attachments,
        }
    }

    pub(crate) fn without_skill_markers(
        self,
        provider_name: &str,
    ) -> Result<String, ProviderError> {
        let mut spans = self
            .skill_invocations
            .into_iter()
            .flat_map(|invocation| invocation.spans)
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

/// An Attachment as a Provider is handed it: the bytes, read from storage as
/// the Prompt carrying it is delivered, the type they were sniffed as, and the
/// label standing for it in the Prompt's text. Each adapter encodes the bytes
/// as its harness takes them.
#[derive(Clone, Eq, PartialEq)]
pub struct ProviderAttachment {
    pub label: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

impl fmt::Debug for ProviderAttachment {
    /// Names the bytes by their length alone, so input written to the Log
    /// never carries an image.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderAttachment")
            .field("label", &self.label)
            .field("mime_type", &self.mime_type)
            .field("byte_length", &self.bytes.len())
            .finish()
    }
}

/// One distinct Provider-neutral Skill Invocation, in first-appearance order,
/// with the span of every `$skill-name` that selected it retained for
/// Provider-specific lowering.
/// Native paths and command names remain inside the Provider adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderSkillInvocation {
    pub skill_id: SkillId,
    pub spans: Vec<TextSpan>,
}

/// What Suru hands a Provider as its Agent's own input, to begin a Turn or to
/// steer one: the Subagent Reports it delivers, standing at the head, then the
/// Prompt — or, in a brokered Subagent's Session, the Delegation — beside
/// them. Input with Reports and no Prompt is how a Report alone wakes an idle
/// Agent into a Continuation (ADR 0035). Suru never hands a Provider input
/// holding neither.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderInput {
    pub reports: Vec<SubagentReport>,
    pub prompt: Option<ProviderPrompt>,
}

impl ProviderInput {
    /// A Prompt, or a Delegation, with no Report beside it.
    pub fn from_prompt(prompt: ProviderPrompt) -> Self {
        Self {
            reports: Vec::new(),
            prompt: Some(prompt),
        }
    }

    /// Subagent Reports alone.
    pub fn from_reports(reports: Vec<SubagentReport>) -> Self {
        Self {
            reports,
            prompt: None,
        }
    }

    /// This input with `reports` standing at its head, ahead of any it
    /// already carried.
    pub fn headed_by(mut self, mut reports: Vec<SubagentReport>) -> Self {
        reports.append(&mut self.reports);
        self.reports = reports;
        self
    }

    /// Lowers this input to what its harness sends, in the one order every
    /// harness keeps: the Reports' text at the head, then the Prompt — or
    /// Delegation. `headed_prompt` is handed the Prompt and the Reports' text
    /// to stand ahead of it, and places that text ahead of the Prompt's own
    /// words as its transport allows (see [`headed_text`]); input with
    /// Reports and no Prompt is `reports_alone`'s to lower from their text.
    /// `provider` names the harness in the failure for input holding
    /// neither, which Suru never hands a Provider.
    pub(crate) async fn lower<T>(
        self,
        provider: &str,
        reports_alone: impl FnOnce(String) -> T,
        headed_prompt: impl AsyncFnOnce(ProviderPrompt, Option<String>) -> Result<T, ProviderError>,
    ) -> Result<T, ProviderError> {
        let head = self.report_text();
        match self.prompt {
            Some(prompt) => headed_prompt(prompt, head).await,
            None => head.map(reports_alone).ok_or_else(|| {
                ProviderError::new(format!("{provider} was handed no input for its Turn"))
            }),
        }
    }

    /// The Reports as the Agent reads them — each in the one text Suru
    /// renders it as, a blank line apart — or `None` for input carrying none.
    pub fn report_text(&self) -> Option<String> {
        (!self.reports.is_empty()).then(|| {
            self.reports
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n\n")
        })
    }
}

/// `text` with `head` standing ahead of it, a blank line apart: how the
/// Reports a Turn's input opens with head the text its Prompt lowers to.
pub(crate) fn headed_text(head: Option<String>, text: String) -> String {
    match head {
        Some(head) if text.is_empty() => head,
        Some(head) => format!("{head}\n\n{text}"),
        None => text,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderTurnInput {
    /// Correlates optional asynchronous Context Fill requests with this Turn.
    pub turn_id: crate::protocol::TurnId,
    pub input: ProviderInput,
    pub selection: AgentSelection,
    pub approval_posture: Option<ApprovalPosture>,
}

/// What asking a Provider to compact the Session's context now takes: the
/// Turn the request began (ADR 0041), the Agent Selection and Approval Posture
/// that Turn runs under, as a Prompt's would, and the user's instructions on
/// what the summary should keep, carried only to a Provider that declares it
/// takes them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderCompactionInput {
    pub turn_id: crate::protocol::TurnId,
    pub selection: AgentSelection,
    pub approval_posture: Option<ApprovalPosture>,
    pub instructions: Option<String>,
}

/// A live Provider connection's answer to an Approval Posture change.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderPostureApplication {
    Applied,
    NextTurn,
}

/// Input delivered into a Turn still working, without beginning another: a
/// steer Prompt, or a Subagent Report reaching an Agent mid-Turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderSteerInput {
    pub input: ProviderInput,
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
    /// The command was still running when its Turn was interrupted.
    Interrupted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderFileChangeStatus {
    Completed,
    Failed,
    /// The change was still being made when its Turn was interrupted.
    Interrupted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderToolCallStatus {
    Completed,
    /// The Tool reported an error, whose text is the Tool Call's output.
    Failed,
    /// The Tool was still running when its Turn was interrupted.
    Interrupted,
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

/// How a Provider reported one of its Watches settling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderWatchOutcome {
    Completed,
    Failed,
    /// The Watch was stopped rather than finishing or failing — by a stop
    /// Suru asked for, or by one the Provider or its Agent ran on its own.
    Stopped,
    /// The Watch died with the Provider process that ran it, which will never
    /// report it settling: the process was stopped, or replaced by one that
    /// never heard of it. Its settling wakes nothing.
    Lost,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderEvent {
    /// An ordered, Session-local measurement. Streamed reports may bind to the
    /// currently routed Turn; asynchronous requests must retain their Turn ID.
    ContextFill {
        report: ContextFillReport,
    },
    /// The Provider began a new native Turn without a Prompt. Unlike late
    /// output alone, this Turn has its own interrupt and terminal boundary.
    ContinuationStarted {
        selection: AgentSelection,
    },
    QuestionnaireRequested {
        questionnaire: crate::protocol::Questionnaire,
    },
    QuestionnaireWithdrawn {
        id: crate::protocol::QuestionnaireId,
    },
    ApprovalRequested {
        approval: crate::protocol::Approval,
        /// Native identity of the gated Tool row, when one exists. Session
        /// orchestration resolves it to the public Activity identity.
        tool_activity_id: Option<ProviderActivityId>,
    },
    ApprovalWithdrawn {
        id: crate::protocol::ApprovalId,
    },
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
    /// The Agent began using a Tool that no more specific Activity records.
    /// A Provider opens the row as early as it learns of the use — before an
    /// Approval that gates it can arrive — so the input, rendered for display
    /// by the Provider, may be unknown yet and follow in
    /// [`Self::ToolCallInputKnown`].
    ToolCallStarted {
        activity_id: ProviderActivityId,
        /// The Tool's name as the Provider spells it.
        name: String,
        /// The MCP server hosting the Tool, where it has one.
        server: Option<String>,
        input: Option<String>,
    },
    /// The display rendering of a started Tool Call's input, once known.
    ToolCallInputKnown {
        activity_id: ProviderActivityId,
        input: String,
    },
    ToolCallOutputDelta {
        activity_id: ProviderActivityId,
        content: String,
    },
    ToolCallCompleted {
        activity_id: ProviderActivityId,
        status: ProviderToolCallStatus,
        /// How many parts of the result that were not text — images, audio,
        /// resources — the Provider left out of the output it sent.
        omitted_parts: u32,
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
        /// The spawn's Delegation: the text the spawner's Agent handed the
        /// Subagent, where the Provider reports it. It opens the child
        /// Session's first Turn as a Message from the spawner's Agent.
        delegation: Option<String>,
    },
    /// The Turn's Agent resumed a settled Subagent: the same agent, named by
    /// the identity its spawn carried, continuing the same conversation. Like
    /// a spawn, the event lands in the delegating Session — the owning
    /// Session, or a Subagent's own when a sibling sends the resume — and
    /// orchestration answers it by beginning the next Turn in the Subagent's
    /// existing Session and adding the row that stands for this stretch of
    /// its work. One the owning Session delegates after its Turn settled
    /// begins a Continuation there to hold that row, whatever else is owed,
    /// which the Provider settles at its own boundary like any other. One a
    /// Subagent delegated but that reaches its Subagent only once the
    /// delegating one has settled begins a Continuation of the delegating
    /// Subagent's Session instead, which holds the row and settles at once
    /// (ADR 0033). The
    /// identity finds that Session even when an earlier process spawned the
    /// Subagent, because it is stored with the Session. `description` is what
    /// the resume asked for; the Subagent's Title stays what its spawn said,
    /// and its rows keep its spawn's name. `name` is used only when Suru holds
    /// no Session for the identity at all, and so records the resume as a new
    /// Subagent rather than losing it.
    SubagentResumed {
        subagent_id: ProviderSubagentId,
        name: String,
        description: String,
        /// The resume's Delegation: the text the delegating Agent sent the
        /// Subagent, where the Provider reports it. It opens the Turn the
        /// resume begins as a Message from the delegating Agent.
        delegation: Option<String>,
    },
    /// A Delegation the Provider delivered into a Subagent's working Turn:
    /// a steer (ADR 0032), reported at the point the Subagent received it.
    /// Like a spawn or a resume, the event is attributed to the delegating
    /// Agent — the owning Session's, or a sibling Subagent's — and names the
    /// Subagent it reached, but it begins no Turn and adds nothing to the
    /// delegating Transcript: orchestration adds the Delegation, as a Message
    /// from the delegating Agent, to the Turn the Subagent is working in. It
    /// is no output of any Turn of the delegating Session's, so it never
    /// begins a Continuation there. A steer naming a Subagent that is not
    /// working — stopped, or settled — was never delivered, and stands
    /// nowhere.
    SubagentSteered {
        subagent_id: ProviderSubagentId,
        delegation: String,
    },
    /// The Delegation that began a Subagent's working stretch, reported only
    /// after the `SubagentStarted` or `SubagentResumed` that began the
    /// stretch, that event having carried none — as when a Provider announces
    /// a spawn by the agent it started and hands the Subagent its task as the
    /// Subagent's own first input. Like a steer, the event is attributed to
    /// the delegating Agent — the owning Session's, or a sibling Subagent's —
    /// names the Subagent it reached, and begins no Turn; but it is no steer,
    /// because the Subagent received it before anything else of the stretch:
    /// orchestration adds it, as a Message from the delegating Agent, at the
    /// head of the Turn the stretch works in, where a Delegation reported with
    /// the spawn or resume would have opened it, and gives the stretch's row
    /// the description it lacked — the Delegation's first line. The one way
    /// it stands apart from a Delegation reported with a resume is behind any
    /// Watch Outcome that resume released into the Turn: the Outcome that
    /// woke the Subagent heads the Turn it next works in, whatever began it,
    /// and is recorded as the resume begins that Turn, before this event can
    /// arrive. It is no output of any Turn of the delegating Session's, so it
    /// never begins a Continuation there. One naming a Subagent Suru stopped
    /// trails it as a late echo, and stands nowhere; one naming a Subagent
    /// with no working stretch otherwise is an invalid event, which fails the
    /// Turn it would have landed in by its attribution, as any invalid event
    /// does.
    SubagentDelegated {
        subagent_id: ProviderSubagentId,
        delegation: String,
    },
    /// A settled Subagent's own Watch woke it, or input Suru handed it itself
    /// — a Subagent Report (ADR 0035) — began a turn of its: the same agent
    /// works on in the same conversation, though no Agent delegated anything
    /// to it. So it is no resume (ADR 0031) and gains no row in any
    /// Transcript: its work lands in a Continuation of the Subagent's own
    /// Session, headed by any Watch Outcome that woke it, and its Session
    /// Works again — and with it every Session above it. A Subagent already
    /// woken, or still working, has nothing more to wake. The Provider's next `SubagentCompleted` for it settles
    /// that Continuation as it settles any stretch. Like the rest of a
    /// Subagent's lifecycle it names the Subagent by `subagent_id` whatever its
    /// attribution, and it is no output of any Turn of the delegating
    /// Session's: nothing there caused it.
    SubagentWoken {
        subagent_id: ProviderSubagentId,
    },
    /// The Provider revised what a working Subagent is doing.
    SubagentUpdated {
        subagent_id: ProviderSubagentId,
        description: String,
    },
    /// Provider evidence of the Model presently running one Subagent.
    SubagentModelChanged {
        subagent_id: ProviderSubagentId,
        model: crate::protocol::ModelId,
    },
    /// The Provider reported a Subagent's current stretch of work settling.
    /// This settles the stretch's row and the child Session's Turn it worked
    /// in together. Later output is discarded until a resume begins another
    /// stretch; ordered Context Fill measurements may still refresh the child
    /// Session.
    SubagentCompleted {
        subagent_id: ProviderSubagentId,
        status: ProviderSubagentStatus,
    },
    /// The Agent left work running that may wake it into a Continuation — a
    /// Watch: a background shell or a monitor, whose settling or report the
    /// Provider delivers to the Agent. It lands in the Session whose Agent
    /// started it, named by the attribution. A Watch is never output of any
    /// Turn: the Command that started it already stands in the Transcript, so
    /// the event only keeps the Session Monitoring once nothing is Working.
    WatchStarted {
        watch_id: ProviderWatchId,
        /// What the Watch is doing, in the words the Provider gives it.
        description: String,
    },
    /// A Watch settled, however it ended. `woke_agent` says whether its
    /// settling is delivered to the Agent, which then works on in a
    /// Continuation the Provider begins; a Watch stopped by an interrupt or
    /// lost with its process wakes nothing. Like its start it is no output of
    /// any Turn.
    WatchSettled {
        watch_id: ProviderWatchId,
        outcome: ProviderWatchOutcome,
        /// The Provider's own account of how the Watch settled, where it gave
        /// one. It is display text, never parsed.
        summary: Option<String>,
        woke_agent: bool,
    },
    /// The Provider began compacting the context of the conversation the
    /// event is attributed to: replacing what its Agent remembers with a
    /// summary. It records a Compaction, Active while the Provider
    /// summarises, in the Turn it fell in — the owning Session's, a working
    /// Subagent's, or a Continuation it begins when no Turn is active there.
    /// A Provider may restate that it is still compacting; while a Compaction
    /// is Active, that is the same occasion rather than another. A Provider
    /// whose compaction runs in a native turn it began on its own reports
    /// [`Self::ContinuationStarted`] first, so an interrupt — or the next
    /// Prompt's delivery — stops that turn rather than only the Continuation
    /// Suru holds it in.
    CompactionStarted,
    /// The Provider's Compaction completed, with the Context Fill before and
    /// after in tokens where it reported them. A side it leaves out is read
    /// from the Session's own Context Fill readings instead, so a Provider
    /// that measures neither need only keep reporting [`Self::ContextFill`].
    /// One reported with no start before it records a Compaction settled from
    /// the moment it stands. `summary` is what the Agent was left with, where
    /// the Provider gave it, as the Provider's summarising call wrote it —
    /// unwrapped of whatever the Provider put around it for the Agent, and
    /// uncapped, since orchestration stores it under Suru's cap.
    CompactionCompleted {
        before_tokens: Option<u64>,
        after_tokens: Option<u64>,
        summary: Option<String>,
    },
    /// The Provider's Compaction failed, with its account of why where it gave
    /// one. Only the Compaction fails: the Turn it fell in Settles as the
    /// Provider says. Like a completion, one with no start records a
    /// Compaction settled from the moment it stands.
    CompactionFailed {
        error: Option<String>,
    },
    /// The Provider revised what it needs to carry the owning Session's
    /// conversation across a restart, replacing the Resume State its startup
    /// reported. Only the Provider reads the payload back, at the next start;
    /// orchestration stores it for the owning Session whatever the event's
    /// attribution, and it is no output of any Turn.
    ResumeStateChanged {
        resume_state: ProviderResumeState,
    },
    /// The Provider's latest complete reading of the active Turn. Each absent
    /// field remains absent through the protocol, and the Cost beside them is
    /// frozen by the Session store on the Basis the event names.
    Usage {
        usage: Usage,
        cost: Option<MeteredCost>,
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

/// Reports replace the measurement, including when occupancy decreases.
/// Allocate sequence numbers when observations/requests begin, not when an
/// asynchronous response completes. Numbers must increase within each Turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContextFillReport {
    /// `None` binds a synchronous, ordered stream event to its routed Turn.
    /// Delayed requests must use the ID from `ProviderTurnInput` instead.
    pub turn_id: Option<crate::protocol::TurnId>,
    pub sequence: u64,
    pub fill: crate::protocol::ContextFill,
}

/// A Cost one Provider event carries, together with the Basis that says how
/// far to trust it. Pairing the two makes a Cost impossible to record without
/// stating who computed it, so a rate-table estimate can never be stored as a
/// Provider's own figure.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct MeteredCost {
    cost: Cost,
    basis: CostBasis,
    coverage: CostCoverage,
    is_partial: bool,
}

impl MeteredCost {
    /// A dollar figure the Provider stated itself.
    pub const fn reported(cost: Cost) -> Self {
        Self {
            cost,
            basis: CostBasis::Reported,
            coverage: CostCoverage::Turn,
            is_partial: false,
        }
    }

    pub fn reported_subtree(cost: Cost, reporting_lifetime: impl Into<String>) -> Self {
        Self {
            cost,
            basis: CostBasis::Reported,
            coverage: CostCoverage::SessionSubtree {
                reporting_lifetime: reporting_lifetime.into(),
            },
            is_partial: false,
        }
    }

    pub fn partial(mut self) -> Self {
        self.is_partial = true;
        self
    }

    pub const fn cost(&self) -> Cost {
        self.cost
    }

    pub const fn basis(&self) -> CostBasis {
        self.basis
    }

    pub fn coverage(&self) -> &CostCoverage {
        &self.coverage
    }

    pub const fn is_partial(&self) -> bool {
        self.is_partial
    }
}

impl From<EstimatedCost> for MeteredCost {
    fn from(estimated: EstimatedCost) -> Self {
        Self {
            cost: estimated.cost(),
            basis: estimated.basis(),
            coverage: CostCoverage::Turn,
            is_partial: false,
        }
    }
}

/// The cumulative Usage and Provider-reported Cost for one active Turn. A
/// field stays known only while every contributing Provider report states a
/// valid value; overflow and omission degrade that field to absence rather
/// than manufacturing a number.
pub(super) struct ReportedTurnMetering {
    usage: Usage,
    reported_cost: Option<Cost>,
    cost_complete: bool,
    native_meter_overflowed: bool,
}

impl ReportedTurnMetering {
    pub(super) fn new(usage: Usage, reported_cost: Option<Cost>) -> Self {
        Self {
            usage,
            reported_cost,
            cost_complete: reported_cost.is_some(),
            native_meter_overflowed: false,
        }
    }

    pub(super) fn add(&mut self, usage: Usage, reported_cost: Option<Cost>) {
        self.add_usage(usage);
        match reported_cost {
            Some(next) => {
                self.reported_cost = match self.reported_cost {
                    Some(current) => current.checked_add(next),
                    None => Some(next),
                };
                self.cost_complete &= self.reported_cost.is_some();
            }
            None => self.cost_complete = false,
        }
    }

    fn add_usage(&mut self, usage: Usage) {
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
    }

    pub(super) fn add_usage_with_cumulative_cost(
        &mut self,
        usage: Usage,
        reported_cost: Option<Cost>,
    ) {
        self.add_usage(usage);
        match reported_cost {
            Some(cost)
                if self
                    .reported_cost
                    .is_none_or(|current| cost.nano_usd() >= current.nano_usd()) =>
            {
                self.reported_cost = Some(cost);
                self.cost_complete = true;
            }
            Some(_) | None => self.cost_complete = false,
        }
    }

    pub(super) fn event(&self) -> ProviderEvent {
        ProviderEvent::Usage {
            usage: self.usage.clone(),
            cost: self.reported_cost.map(|cost| {
                let metered = MeteredCost::reported(cost);
                if self.cost_complete {
                    metered
                } else {
                    metered.partial()
                }
            }),
        }
    }

    pub(super) fn subtree_event(&self, reporting_lifetime: &str) -> ProviderEvent {
        ProviderEvent::Usage {
            usage: self.usage.clone(),
            cost: self
                .cost_complete
                .then_some(self.reported_cost)
                .flatten()
                .map(|cost| MeteredCost::reported_subtree(cost, reporting_lifetime)),
        }
    }
}

/// A token count a Provider stated, or absence where it did not or where the
/// figure it stated cannot be one. Every Provider meters in signed counts, and
/// a negative one is a Provider bug: reading it as absent degrades the record
/// rather than corrupting it.
pub(super) fn reported_count(count: Option<i64>) -> Option<u64> {
    count.and_then(|count| u64::try_from(count).ok())
}

/// A stated total with the counts nested inside it taken back out, so the five
/// parts a [`Usage`] stores never overlap and no consumer downstream ever has
/// to subtract. A subset larger than its total is as unreadable as a negative
/// count, and degrades the part to absence the same way.
pub(super) fn exclusive_count(
    total: Option<i64>,
    subsets: impl IntoIterator<Item = Option<i64>>,
) -> Option<u64> {
    let total = reported_count(total)?;
    subsets.into_iter().try_fold(total, |remaining, subset| {
        let subset = match subset {
            Some(count) => u64::try_from(count).ok()?,
            None => 0,
        };
        remaining.checked_sub(subset)
    })
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

    /// The glyph shown beside this Provider when the client has Nerd Font
    /// icons enabled. Providers may remain text-only where no suitable glyph
    /// exists.
    fn nerd_font_icon(&self) -> Option<char> {
        None
    }

    fn list_models(&self) -> ProviderFuture<'_, ProviderModelDiscovery>;

    /// Offers the effective user-invocable Skill Catalog for exactly one
    /// Execution Directory. Discovery and native identifiers remain inside the Provider;
    /// the generic boundary returns only opaque identities and safe metadata.
    /// Providers may inherit the unavailable catalog while their adapter has no
    /// Skill implementation, which keeps ordinary Prompt behavior independent
    /// of Skill discovery.
    fn skill_catalog(&self, execution_directory: &Path) -> ProviderFuture<'_, SkillCatalog> {
        let catalog = SkillCatalog {
            provider: self.provider_id(),
            execution_directory: crate::protocol::ExecutionDirectory {
                path: execution_directory.to_owned(),
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
    fn refresh_skill_catalog(
        &self,
        execution_directory: &Path,
    ) -> ProviderFuture<'_, SkillCatalog> {
        self.skill_catalog(execution_directory)
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
    /// Execution Directory for this Provider instead of interpreting native details.
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

    /// How far this Provider compacts a Session's context on request.
    /// Defaulted to [`ManualCompaction::Unsupported`], matching the refusal
    /// [`ProviderSession::compact`] defaults to, so a Provider without the
    /// capability offers nothing and every surface explaining `/compact`
    /// reads this one declaration.
    fn manual_compaction(&self) -> ManualCompaction {
        ManualCompaction::Unsupported
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
    /// Applies a changed Approval Posture as far as this Provider permits.
    /// `has_active_work` includes routed Subagents that outlive the root Turn.
    fn update_approval_posture(
        &self,
        posture: ApprovalPosture,
        has_active_work: bool,
    ) -> ProviderFuture<'_, ProviderPostureApplication>;

    /// Delivers a user's Decision to a live Provider Approval. Providers that
    /// do not expose Approvals refuse by default.
    fn submit_decision(
        &self,
        id: crate::protocol::ApprovalId,
        decision: crate::protocol::Decision,
    ) -> ProviderFuture<'_, ProviderDecisionDelivery> {
        let _ = (id, decision);
        Box::pin(async { Err(ProviderError::new("Approval is unavailable")) })
    }

    fn submit_questionnaire(
        &self,
        id: crate::protocol::QuestionnaireId,
        submission: crate::protocol::QuestionnaireSubmission,
    ) -> ProviderFuture<'_, ()> {
        let _ = (id, submission);
        Box::pin(async { Err(ProviderError::new("Questionnaire is unavailable")) })
    }

    fn start_turn(&self, input: ProviderTurnInput) -> ProviderFuture<'_, ()>;

    fn steer_turn(&self, input: ProviderSteerInput) -> ProviderFuture<'_, ()>;

    /// Asks the Provider to compact the Session's context now, beginning the
    /// native work the Turn a Compaction request opened stands for (ADR
    /// 0041). Answering `Ok` means the Provider took the request; how the
    /// Compaction goes arrives on the event stream like any other, as
    /// [`ProviderEvent::CompactionStarted`] and its completion or failure,
    /// and the Turn's terminal event follows. Suru asks only while the
    /// Session is idle, which it judges itself rather than leaving to the
    /// Provider. Defaulted to a refusal to match the runtime's
    /// [`ProviderRuntime::manual_compaction`] default; a runtime that declares
    /// the capability overrides this with its native request.
    fn compact(&self, input: ProviderCompactionInput) -> ProviderFuture<'_, ()> {
        let _ = input;
        Box::pin(async {
            Err(ProviderError::new(
                "This Provider offers no Compaction on request",
            ))
        })
    }

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

    /// Delivers `input` to the one native Subagent the identity names, as
    /// that Subagent's own input, through the Provider's route to it: a
    /// Subagent Report to a native Subagent that delegated through the Broker
    /// (ADR 0035). A settled Subagent wakes into a Continuation of its own
    /// Session, which Suru opens once the Provider takes the input, and whose
    /// events the Provider attributes to the Subagent until its next settle;
    /// a working one is steered. Defaulted to a refusal, so a Provider that
    /// lets Suru address its Subagents has to say how.
    fn deliver_to_subagent(
        &self,
        subagent_id: ProviderSubagentId,
        input: ProviderInput,
    ) -> ProviderFuture<'_, ()> {
        let _ = (subagent_id, input);
        Box::pin(async {
            Err(ProviderError::new(
                "This Provider offers no route to deliver input to one of its Subagents",
            ))
        })
    }

    /// Stops the Watches named, for the interrupt of a Session that is only
    /// Monitoring (ADR 0030). The caller names the set — the Watches live in
    /// the interrupted Session's subtree — so stopping all of a connection's
    /// Watches and stopping one subtree's are the same request. It stops no
    /// Turn and no Subagent. A Watch the Provider no longer runs has nothing
    /// left to stop, which succeeds.
    ///
    /// Answering `Ok` means the Provider stopped them; each stopped Watch's
    /// settling still arrives on the event stream as a `WatchSettled` whose
    /// outcome is stopped and which wakes nothing, because that is where every
    /// Watch settles. Defaulted to a refusal, so a Provider that begins
    /// reporting Watches has to say what stopping one means.
    fn stop_watches(&self, watches: Vec<ProviderWatchId>) -> ProviderFuture<'_, ()> {
        let _ = watches;
        Box::pin(async { Err(ProviderError::new("This Provider offers no Watch stop")) })
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

    #[tokio::test]
    async fn every_harness_lowers_its_input_reports_first_whatever_it_carries() {
        use super::{ProviderInput, ProviderPrompt, SubagentReport, SubagentReportOutcome};
        let report = SubagentReport::new(
            crate::protocol::SessionId::new(),
            "Researcher",
            SubagentReportOutcome::Completed,
            Some(1_000),
            Some("Done."),
            None,
        );
        let lower = async |input: ProviderInput| {
            input
                .lower(
                    "Fixture",
                    |reports| format!("alone: {reports}"),
                    async |prompt, head| Ok(format!("{head:?} then {}", prompt.text)),
                )
                .await
        };

        assert_eq!(
            lower(ProviderInput::from_reports(vec![report.clone()]))
                .await
                .expect("Reports alone lower"),
            format!("alone: {report}")
        );
        assert_eq!(
            lower(
                ProviderInput::from_prompt(ProviderPrompt::plain("Go."))
                    .headed_by(vec![report.clone()])
            )
            .await
            .expect("a Prompt headed by Reports lowers"),
            format!("{:?} then Go.", Some(report.to_string())),
            "the Prompt's lowering is handed the Reports to stand at its head"
        );
        assert_eq!(
            lower(ProviderInput::from_prompt(ProviderPrompt::plain("Go.")))
                .await
                .expect("a Prompt alone lowers"),
            "None then Go."
        );
        let neither = lower(ProviderInput::from_reports(Vec::new()))
            .await
            .expect_err("input holding neither is refused");
        assert!(neither.to_string().contains("Fixture was handed no input"));
    }

    #[cfg(windows)]
    use super::executable_shadowed_by_windows_app_alias;
    use super::{
        MAX_REMOTE_ERROR_CHARS, ManualCompaction, built_in_providers, concise_remote_message,
        resolve_executable, runtimes,
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
        for runtime in runtimes(None) {
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

    /// `/compact` is explained before it is sent from what clients read here,
    /// so the list must say what each runtime declares: every built-in
    /// Provider compacts on request.
    #[test]
    fn clients_read_each_providers_manual_compaction_as_its_runtime_declares_it() {
        let declared = built_in_providers()
            .iter()
            .map(|provider| (provider.id.as_str().to_owned(), provider.manual_compaction))
            .collect::<Vec<_>>();
        assert_eq!(
            declared,
            [
                ("codex".to_owned(), ManualCompaction::Supported),
                ("copilot".to_owned(), ManualCompaction::Supported),
                ("claude".to_owned(), ManualCompaction::Supported),
            ]
        );
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
        for runtime in runtimes(None) {
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

    #[cfg(windows)]
    #[test]
    fn a_windows_app_alias_does_not_shadow_a_later_cli_executable() {
        let directory = tempfile::tempdir().expect("create executable lookup fixture");
        let windows_apps = directory
            .path()
            .join("LocalAppData")
            .join("Microsoft")
            .join("WindowsApps");
        let cli_directory = directory.path().join("WinGet").join("Links");
        std::fs::create_dir_all(&windows_apps).expect("create WindowsApps fixture directory");
        std::fs::create_dir_all(&cli_directory).expect("create CLI fixture directory");
        std::fs::write(windows_apps.join("copilot.exe"), []).expect("write app alias fixture");
        let cli = cli_directory.join("copilot.exe");
        std::fs::write(&cli, []).expect("write CLI executable fixture");
        let path = std::env::join_paths([&windows_apps, &windows_apps, &cli_directory])
            .expect("join executable lookup fixture PATH");

        assert_eq!(
            executable_shadowed_by_windows_app_alias("copilot", &path, &windows_apps),
            Some(cli.into_os_string()),
            "a Windows app execution alias must not hide an installed command-line program"
        );
    }

    #[cfg(windows)]
    #[test]
    fn ordinary_windows_path_resolution_stays_native() {
        let directory = tempfile::tempdir().expect("create executable lookup fixture");
        let cli_directory = directory.path().join("bin");
        let windows_apps = directory.path().join("WindowsApps");
        std::fs::create_dir_all(&cli_directory).expect("create CLI fixture directory");
        let cli = cli_directory.join("copilot.exe");
        std::fs::write(&cli, []).expect("write CLI executable fixture");
        let path = std::env::join_paths([&cli_directory, &windows_apps])
            .expect("join executable lookup fixture PATH");

        assert_eq!(
            executable_shadowed_by_windows_app_alias("copilot", &path, &windows_apps),
            None,
            "a normal first match is left to Command's native lookup"
        );
    }
}
