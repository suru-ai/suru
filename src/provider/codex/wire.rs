//! Serde types for the Codex app-server V2 protocol and their Suru conversions.
//!
//! Everything Suru writes to or reads from the app-server is shaped here, together with the
//! mappings between those native shapes and Suru's own protocol types. Modules above this one
//! work in Suru terms and never touch raw JSON.

use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{DEFAULT_SERVICE_TIER_CHOICE_ID, REASONING_EFFORT_OPTION_ID, SERVICE_TIER_OPTION_ID};
use crate::{
    protocol::{
        AgentSelection, CodexApprovalPolicy, CodexSandboxMode, FileChange, ModelAvailability,
        ModelDescriptor, ModelId, ModelOptionChoice, ModelOptionChoiceId, ModelOptionDescriptor,
        ModelOptionId, ModelOptionKind, ModelOptionRole, ModelOptionValue, ProviderId,
        ReasoningSummaryDetail, Usage,
    },
    provider::{ProviderError, exclusive_count, humanized_wire_id, reported_count},
};

// Native user-input requests preserve both JSON-RPC and thread/Turn/item correlation.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UserInputParams {
    pub(super) thread_id: String,
    pub(super) turn_id: String,
    pub(super) item_id: String,
    pub(super) questions: Vec<UserInputQuestion>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UserInputQuestion {
    pub(super) id: String,
    pub(super) header: String,
    pub(super) question: String,
    #[serde(default)]
    pub(super) is_other: bool,
    #[serde(default)]
    pub(super) is_secret: bool,
    pub(super) options: Option<Vec<UserInputOption>>,
}
#[derive(Deserialize)]
pub(super) struct UserInputOption {
    pub(super) label: String,
    pub(super) description: String,
}
#[derive(Serialize)]
pub(super) struct UserInputResponse {
    pub(super) answers: BTreeMap<String, UserInputAnswer>,
}
#[derive(Serialize)]
pub(super) struct UserInputAnswer {
    pub(super) answers: Vec<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ServerRequestResolvedParams {
    pub(super) thread_id: String,
    pub(super) request_id: RequestId,
}
#[derive(Serialize)]
pub(super) struct ClientResponse<T> {
    pub(super) id: RequestId,
    pub(super) result: T,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CommandApprovalParams {
    pub(super) thread_id: String,
    pub(super) turn_id: String,
    pub(super) item_id: String,
    #[serde(default)]
    pub(super) reason: Option<String>,
    #[serde(default)]
    pub(super) network_approval_context: Option<NetworkApprovalContext>,
    #[serde(default)]
    pub(super) command: Option<String>,
    #[serde(default)]
    pub(super) cwd: Option<PathBuf>,
    #[serde(default)]
    pub(super) command_actions: Option<Vec<Value>>,
}

#[derive(Deserialize)]
pub(super) struct NetworkApprovalContext {
    pub(super) host: String,
    pub(super) protocol: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct FileChangeApprovalParams {
    pub(super) thread_id: String,
    pub(super) turn_id: String,
    pub(super) item_id: String,
    pub(super) reason: Option<String>,
    pub(super) grant_root: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PermissionsApprovalParams {
    pub(super) thread_id: String,
    pub(super) turn_id: String,
    pub(super) item_id: String,
    pub(super) reason: Option<String>,
    pub(super) permissions: Value,
}

// JSON-RPC envelope.

#[derive(Serialize)]
pub(super) struct ClientRequest<'a, T> {
    pub(super) id: i64,
    pub(super) method: &'a str,
    pub(super) params: T,
}

#[derive(Serialize)]
pub(super) struct ClientNotification<'a> {
    pub(super) method: &'a str,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub(super) enum RequestId {
    String(String),
    Integer(i64),
    Unsigned(u64),
}

impl RequestId {
    pub(super) fn correlation_key(&self) -> String {
        match self {
            Self::String(id) => id.clone(),
            Self::Integer(id) => id.to_string(),
            Self::Unsigned(id) => id.to_string(),
        }
    }
}

#[derive(Deserialize)]
pub(super) struct IncomingMessage {
    pub(super) id: Option<RequestId>,
    pub(super) method: Option<String>,
    pub(super) params: Option<Value>,
    pub(super) result: Option<Value>,
    pub(super) error: Option<RemoteError>,
}

#[derive(Deserialize)]
pub(super) struct RemoteError {
    pub(super) message: String,
}

#[derive(Serialize)]
pub(super) struct ClientErrorResponse {
    pub(super) id: RequestId,
    pub(super) error: ClientError,
}

#[derive(Serialize)]
pub(super) struct ClientError {
    pub(super) code: i64,
    pub(super) message: String,
}

// Handshake.

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct InitializeParams {
    pub(super) client_info: ClientInfo,
    pub(super) capabilities: InitializeCapabilities,
}

#[derive(Serialize)]
pub(super) struct ClientInfo {
    pub(super) name: &'static str,
    pub(super) title: &'static str,
    pub(super) version: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct InitializeCapabilities {
    pub(super) experimental_api: bool,
}

// Model catalog.

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ModelListParams<'a> {
    pub(super) cursor: Option<&'a str>,
    pub(super) limit: Option<u32>,
    pub(super) include_hidden: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NativeModelList {
    data: Vec<NativeModel>,
    pub(super) next_cursor: Option<String>,
}

impl NativeModelList {
    /// Drops the Models Codex hides from its own clients and normalizes the rest.
    pub(super) fn visible_models(self) -> impl Iterator<Item = ModelDescriptor> {
        self.data
            .into_iter()
            .filter(|model| !model.hidden)
            .map(ModelDescriptor::from)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeModel {
    id: String,
    display_name: String,
    description: String,
    hidden: bool,
    supported_reasoning_efforts: Vec<NativeReasoningEffort>,
    default_reasoning_effort: String,
    #[serde(default)]
    service_tiers: Vec<NativeServiceTier>,
    #[serde(default)]
    default_service_tier: Option<String>,
    is_default: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeReasoningEffort {
    reasoning_effort: String,
    description: String,
}

#[derive(Deserialize)]
struct NativeServiceTier {
    id: String,
    name: String,
    description: String,
}

impl From<NativeModel> for ModelDescriptor {
    fn from(model: NativeModel) -> Self {
        let provider = ProviderId::new("codex");
        let mut options = Vec::new();
        if !model.supported_reasoning_efforts.is_empty() {
            options.push(ModelOptionDescriptor {
                id: ModelOptionId::new(REASONING_EFFORT_OPTION_ID),
                label: "Reasoning effort".to_owned(),
                description: None,
                role: ModelOptionRole::ReasoningEffort,
                kind: ModelOptionKind::Select {
                    choices: model
                        .supported_reasoning_efforts
                        .into_iter()
                        .map(|effort| ModelOptionChoice {
                            label: humanized_wire_id(&effort.reasoning_effort),
                            id: ModelOptionChoiceId::new(effort.reasoning_effort),
                            description: Some(effort.description),
                            availability: ModelAvailability::Available,
                        })
                        .collect(),
                    default: ModelOptionChoiceId::new(model.default_reasoning_effort),
                },
            });
        }
        if !model.service_tiers.is_empty() {
            let mut choices = model
                .service_tiers
                .into_iter()
                .map(|tier| ModelOptionChoice {
                    id: ModelOptionChoiceId::new(tier.id),
                    label: tier.name,
                    description: Some(tier.description),
                    availability: ModelAvailability::Available,
                })
                .collect::<Vec<_>>();
            let default = model
                .default_service_tier
                .unwrap_or_else(|| "default".to_owned());
            if !choices.iter().any(|choice| choice.id.as_str() == default) && default == "default" {
                choices.insert(
                    0,
                    ModelOptionChoice {
                        id: ModelOptionChoiceId::new(DEFAULT_SERVICE_TIER_CHOICE_ID),
                        label: "Default".to_owned(),
                        description: None,
                        availability: ModelAvailability::Available,
                    },
                );
            }
            options.push(ModelOptionDescriptor {
                id: ModelOptionId::new(SERVICE_TIER_OPTION_ID),
                label: "Speed".to_owned(),
                description: None,
                role: ModelOptionRole::Speed,
                kind: ModelOptionKind::Select {
                    choices,
                    default: ModelOptionChoiceId::new(default),
                },
            });
        }
        Self {
            provider,
            id: ModelId::new(model.id),
            display_name: model.display_name,
            description: model.description,
            is_default: model.is_default,
            availability: ModelAvailability::Available,
            options,
        }
    }
}

// Thread lifecycle.

#[derive(Clone, Copy, Debug)]
pub(super) struct CodexPosture {
    pub(super) approval_policy: CodexApprovalPolicy,
    pub(super) sandbox_mode: CodexSandboxMode,
}

impl Default for CodexPosture {
    fn default() -> Self {
        Self {
            approval_policy: CodexApprovalPolicy::default(),
            sandbox_mode: CodexSandboxMode::default(),
        }
    }
}

impl CodexPosture {
    pub(super) const fn approval_policy(self) -> &'static str {
        match self.approval_policy {
            CodexApprovalPolicy::Untrusted => "untrusted",
            CodexApprovalPolicy::OnRequest => "on-request",
            CodexApprovalPolicy::Never => "never",
        }
    }

    pub(super) const fn sandbox(self) -> &'static str {
        match self.sandbox_mode {
            CodexSandboxMode::ReadOnly => "read-only",
            CodexSandboxMode::WorkspaceWrite => "workspace-write",
            CodexSandboxMode::DangerFullAccess => "danger-full-access",
        }
    }

    pub(super) const fn sandbox_policy(self) -> NativeSandboxPolicy {
        match self.sandbox_mode {
            CodexSandboxMode::ReadOnly => NativeSandboxPolicy::ReadOnly,
            CodexSandboxMode::WorkspaceWrite => NativeSandboxPolicy::WorkspaceWrite,
            CodexSandboxMode::DangerFullAccess => NativeSandboxPolicy::DangerFullAccess,
        }
    }
}

#[derive(Clone, Copy, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(super) enum NativeSandboxPolicy {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ThreadStartParams<'a> {
    pub(super) cwd: &'a str,
    pub(super) approval_policy: &'a str,
    pub(super) sandbox: &'a str,
    pub(super) ephemeral: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ThreadResumeParams<'a> {
    pub(super) thread_id: &'a str,
    pub(super) cwd: &'a str,
    pub(super) approval_policy: &'a str,
    pub(super) sandbox: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ThreadConnectionResult {
    pub(super) thread: NativeThread,
    pub(super) model: String,
    #[serde(default, deserialize_with = "deserialize_native_field")]
    pub(super) reasoning_effort: NativeField<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_native_field")]
    pub(super) service_tier: NativeField<Option<String>>,
}

#[derive(Deserialize)]
pub(super) struct NativeThread {
    pub(super) id: String,
}

// Skill discovery.

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SkillsListParams<'a> {
    pub(super) cwds: [&'a std::path::Path; 1],
    pub(super) force_reload: bool,
}

#[derive(Deserialize)]
pub(super) struct NativeSkillsList {
    pub(super) data: Vec<NativeSkillsListEntry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NativeSkillsListEntry {
    pub(super) cwd: PathBuf,
    pub(super) skills: Vec<NativeSkillMetadata>,
    #[serde(default)]
    pub(super) errors: Vec<NativeSkillError>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NativeSkillMetadata {
    pub(super) name: String,
    pub(super) description: String,
    pub(super) path: PathBuf,
    pub(super) scope: NativeSkillScope,
    pub(super) enabled: bool,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum NativeSkillScope {
    User,
    Repo,
    System,
    Admin,
}

#[derive(Deserialize)]
pub(super) struct NativeSkillError {
    #[serde(rename = "message")]
    pub(super) _message: String,
}

// Turn lifecycle.

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TurnStartParams<'a> {
    pub(super) thread_id: &'a str,
    pub(super) input: &'a [UserInput],
    pub(super) model: &'a str,
    pub(super) approval_policy: &'a str,
    /// How much detail Codex should summarize its Reasoning in. A Turn that
    /// omits it inherits the Model's own default, which is why Suru always
    /// states one.
    pub(super) summary: &'static str,
    pub(super) sandbox_policy: NativeSandboxPolicy,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) effort: Option<&'a str>,
    #[serde(
        skip_serializing_if = "NativeServiceTierOverride::is_omitted",
        serialize_with = "serialize_native_service_tier"
    )]
    pub(super) service_tier: NativeServiceTierOverride<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TurnSteerParams<'a> {
    pub(super) thread_id: &'a str,
    pub(super) input: &'a [UserInput],
    pub(super) expected_turn_id: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TurnInterruptParams<'a> {
    pub(super) thread_id: &'a str,
    pub(super) turn_id: &'a str,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(super) enum UserInput {
    Text { text: String },
    Skill { name: String, path: PathBuf },
}

#[derive(Deserialize)]
pub(super) struct TurnStartResult {
    pub(super) turn: NativeTurn,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TurnSteerResult {
    pub(super) turn_id: String,
}

#[derive(Deserialize)]
pub(super) struct NativeTurn {
    pub(super) id: String,
}

pub(super) struct NativeTurnOptions<'a> {
    pub(super) effort: Option<&'a str>,
    pub(super) service_tier: NativeServiceTierOverride<'a>,
}

pub(super) enum NativeServiceTierOverride<'a> {
    Omitted,
    Clear,
    Value(&'a str),
}

impl NativeServiceTierOverride<'_> {
    fn is_omitted(&self) -> bool {
        matches!(self, Self::Omitted)
    }
}

fn serialize_native_service_tier<S>(
    service_tier: &NativeServiceTierOverride<'_>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match service_tier {
        NativeServiceTierOverride::Omitted | NativeServiceTierOverride::Clear => {
            serializer.serialize_none()
        }
        NativeServiceTierOverride::Value(value) => serializer.serialize_str(value),
    }
}

/// Lowers the Reasoning summary Setting onto the value Codex names it by. The
/// two vocabularies agree today, which is why the Setting's accepted values
/// are what they are, but the mapping stays explicit so a rename on either
/// side is caught here rather than silently sent on the wire.
pub(super) fn lower_reasoning_summary(detail: ReasoningSummaryDetail) -> &'static str {
    match detail {
        ReasoningSummaryDetail::Auto => "auto",
        ReasoningSummaryDetail::Concise => "concise",
        ReasoningSummaryDetail::Detailed => "detailed",
        ReasoningSummaryDetail::None => "none",
    }
}

/// Lowers an Agent Selection's Model Options onto the native turn parameters that carry them.
pub(super) fn lower_turn_options(
    selection: &AgentSelection,
) -> Result<NativeTurnOptions<'_>, ProviderError> {
    let mut effort = None;
    let mut service_tier = NativeServiceTierOverride::Omitted;
    for option in &selection.options {
        let ModelOptionValue::Select { choice } = &option.value else {
            return Err(ProviderError::selection_rejected(format!(
                "Codex does not support toggle Model Option `{}`",
                option.id
            )));
        };
        match option.id.as_str() {
            REASONING_EFFORT_OPTION_ID if effort.is_none() => {
                effort = Some(choice.as_str());
            }
            SERVICE_TIER_OPTION_ID if service_tier.is_omitted() => {
                service_tier = if choice.as_str() == DEFAULT_SERVICE_TIER_CHOICE_ID {
                    NativeServiceTierOverride::Clear
                } else {
                    NativeServiceTierOverride::Value(choice.as_str())
                };
            }
            REASONING_EFFORT_OPTION_ID | SERVICE_TIER_OPTION_ID => {
                return Err(ProviderError::selection_rejected(format!(
                    "Codex Model Option `{}` was selected more than once",
                    option.id
                )));
            }
            _ => {
                return Err(ProviderError::selection_rejected(format!(
                    "Codex does not support Model Option `{}`",
                    option.id
                )));
            }
        }
    }
    Ok(NativeTurnOptions {
        effort,
        service_tier,
    })
}

// Notification payloads.

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ItemNotificationParams {
    pub(super) thread_id: String,
    pub(super) turn_id: String,
    pub(super) item: NativeItem,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(super) enum NativeItem {
    AgentMessage {
        id: String,
        #[serde(default)]
        text: String,
    },
    CommandExecution {
        id: String,
        command: String,
        #[serde(default)]
        cwd: Option<PathBuf>,
        status: NativeCommandStatus,
        #[serde(default, rename = "aggregatedOutput")]
        aggregated_output: Option<String>,
        #[serde(default, rename = "exitCode")]
        exit_code: Option<i32>,
    },
    FileChange {
        id: String,
        changes: Vec<NativeFileChange>,
        status: NativeFileChangeStatus,
    },
    /// A block of Reasoning. `summary` holds the readable sections Codex
    /// streams; the raw `content` beside it is the unsummarized form, which
    /// Suru neither asks for nor stores.
    Reasoning {
        id: String,
        #[serde(default)]
        summary: Vec<String>,
    },
    /// A collab tool call the thread's agent made — the multi-agent suite's
    /// spawns, waits, and closes. The completed item names the call's receiver
    /// threads and Codex's latest view of each receiver's lifecycle.
    #[serde(rename_all = "camelCase")]
    CollabAgentToolCall {
        tool: NativeCollabTool,
        status: NativeCollabCallStatus,
        #[serde(default)]
        receiver_thread_ids: Vec<String>,
        #[serde(default)]
        prompt: Option<String>,
        #[serde(default)]
        agents_states: BTreeMap<String, NativeCollabAgentState>,
    },
    /// One step of a spawned agent's lifecycle, reported on its spawner's
    /// thread — how Codex's newer multi-agent routing announces a child
    /// thread beginning, being spoken to, and ending.
    #[serde(rename_all = "camelCase")]
    SubAgentActivity {
        kind: NativeSubagentActivityKind,
        agent_thread_id: String,
        agent_path: String,
    },
    #[serde(other)]
    Unknown,
}

/// The collab tools whose calls Suru reads something from; the rest of the
/// suite decodes as [`NativeCollabTool::Other`] and is passed over.
#[derive(Clone, Copy, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(super) enum NativeCollabTool {
    SpawnAgent,
    SendInput,
    #[serde(other)]
    Other,
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(super) enum NativeCollabCallStatus {
    InProgress,
    Completed,
    Failed,
    Interrupted,
    /// A status this build does not know. Read as "not completed" rather than
    /// failing the Session, because the wire grows freely.
    #[serde(other)]
    Other,
}

/// Codex's last known lifecycle state for one spawned agent, carried on the
/// collab calls that observe it.
#[derive(Clone, Deserialize)]
pub(super) struct NativeCollabAgentState {
    pub(super) status: NativeCollabAgentStatus,
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(super) enum NativeCollabAgentStatus {
    PendingInit,
    Running,
    Interrupted,
    Completed,
    Errored,
    Shutdown,
    NotFound,
    #[serde(other)]
    Other,
}

impl NativeCollabAgentStatus {
    pub(super) fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Interrupted | Self::Completed | Self::Errored | Self::Shutdown | Self::NotFound
        )
    }
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(super) enum NativeSubagentActivityKind {
    Started,
    Interacted,
    Interrupted,
    Completed,
    #[serde(other)]
    Other,
}

impl NativeSubagentActivityKind {
    pub(super) fn is_terminal(self) -> bool {
        matches!(self, Self::Interrupted | Self::Completed)
    }
}

/// One step of an item Codex is streaming: which item, and the text it added.
/// Every streamed item kind shares this shape, so it is named for the shape
/// rather than for whichever kind was decoded through it first.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ItemDeltaParams {
    pub(super) thread_id: String,
    pub(super) turn_id: String,
    pub(super) item_id: String,
    pub(super) delta: String,
}

/// One step of a Reasoning summary: which item, which of that item's summary
/// sections, and the text the section added. Codex names the section outright,
/// which is what lets Suru give each section a Reasoning Activity of its own
/// without counting the breaks between them.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ReasoningSummaryDeltaParams {
    pub(super) thread_id: String,
    pub(super) turn_id: String,
    pub(super) item_id: String,
    pub(super) delta: String,
    pub(super) summary_index: usize,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum NativeCommandStatus {
    InProgress,
    Completed,
    Failed,
    Declined,
}

#[derive(Clone, Deserialize)]
pub(super) struct NativeFileChange {
    path: PathBuf,
    kind: NativeFileChangeKind,
}

impl NativeFileChange {
    pub(super) fn redact_paths(&mut self, redact: impl Fn(&mut PathBuf)) {
        redact(&mut self.path);
        if let NativeFileChangeKind::Update {
            move_path: Some(path),
        } = &mut self.kind
        {
            redact(path);
        }
    }
}

impl From<NativeFileChange> for FileChange {
    fn from(change: NativeFileChange) -> Self {
        match change.kind {
            NativeFileChangeKind::Add => Self::Add { path: change.path },
            NativeFileChangeKind::Delete => Self::Delete { path: change.path },
            NativeFileChangeKind::Update { move_path } => Self::Update {
                path: change.path,
                moved_to: move_path,
            },
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum NativeFileChangeKind {
    Add,
    Delete,
    Update {
        #[serde(default, rename = "movePath")]
        move_path: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum NativeFileChangeStatus {
    InProgress,
    Completed,
    Failed,
    Declined,
}

/// The break Codex reports between one Reasoning summary section and the next,
/// naming the section the break opens.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ReasoningSectionBreakParams {
    pub(super) thread_id: String,
    pub(super) turn_id: String,
    pub(super) item_id: String,
    pub(super) summary_index: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct FileChangeUpdatedParams {
    pub(super) thread_id: String,
    pub(super) turn_id: String,
    pub(super) item_id: String,
    pub(super) changes: Vec<NativeFileChange>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TurnStartedParams {
    pub(super) thread_id: String,
    pub(super) turn: NativeTurn,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TurnCompletedParams {
    pub(super) thread_id: String,
    pub(super) turn: CompletedNativeTurn,
}

#[derive(Deserialize)]
pub(super) struct CompletedNativeTurn {
    pub(super) id: String,
    pub(super) status: NativeTurnStatus,
    pub(super) error: Option<NativeTurnError>,
    #[serde(default)]
    pub(super) items: Vec<NativeItem>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum NativeTurnStatus {
    Completed,
    Interrupted,
    Failed,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NativeTurnError {
    pub(super) message: String,
    #[serde(default)]
    pub(super) codex_error_info: Option<NativeCodexErrorInfo>,
    #[serde(default)]
    pub(super) additional_details: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum NativeCodexErrorInfo {
    BadRequest,
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ThreadTokenUsageParams {
    pub(super) thread_id: String,
    pub(super) turn_id: String,
    pub(super) token_usage: NativeThreadTokenUsage,
}

/// Codex's metering for one thread. `total` is the thread's running total
/// since it opened; `last` independently states current context occupancy.
/// Repeated latest readings replace Context Fill and never accumulate Usage.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NativeThreadTokenUsage {
    pub(super) total: NativeTokenBreakdown,
    #[serde(default)]
    pub(super) last: NativeTokenBreakdown,
    #[serde(default)]
    pub(super) model_context_window: Option<i64>,
}

/// One Codex token breakdown as the wire states it. Every count is optional so
/// a build that omits one leaves the part absent rather than reading as zero,
/// and the cached and cache-write counts are subsets of `input_tokens` just as
/// the reasoning count is a subset of `output_tokens`.
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NativeTokenBreakdown {
    #[serde(default)]
    total_tokens: Option<i64>,
    #[serde(default)]
    input_tokens: Option<i64>,
    #[serde(default)]
    cached_input_tokens: Option<i64>,
    #[serde(default)]
    cache_write_input_tokens: Option<i64>,
    #[serde(default)]
    output_tokens: Option<i64>,
    #[serde(default)]
    reasoning_output_tokens: Option<i64>,
}

impl NativeThreadTokenUsage {
    pub(super) fn context_fill(&self) -> Option<crate::protocol::ContextFill> {
        Some(crate::protocol::ContextFill {
            occupied_tokens: reported_count(self.last.total_tokens)?,
            capacity_tokens: reported_count(self.model_context_window)
                .filter(|capacity| *capacity > 0),
        })
    }

    /// Suru's disjoint token parts for this reading. Codex's nested counts are
    /// made exclusive here — the cache parts leave `input_tokens`, Reasoning
    /// leaves `output_tokens` — so no consumer downstream ever subtracts, and
    /// a figure that cannot be represented degrades to absence rather than to
    /// a wrong number.
    pub(super) fn into_cumulative(self) -> NativeCumulativeUsage {
        let breakdown = self.total;
        NativeCumulativeUsage {
            fresh_input_tokens: exclusive_count(
                breakdown.input_tokens,
                [
                    breakdown.cached_input_tokens,
                    breakdown.cache_write_input_tokens,
                ],
            ),
            cache_read_tokens: reported_count(breakdown.cached_input_tokens),
            cache_write_tokens: reported_count(breakdown.cache_write_input_tokens),
            output_tokens: exclusive_count(
                breakdown.output_tokens,
                [breakdown.reasoning_output_tokens],
            ),
            reasoning_tokens: reported_count(breakdown.reasoning_output_tokens),
            model_context_window: reported_count(self.model_context_window),
        }
    }
}

/// One thread's running total in Suru's own token parts. Codex meters a thread
/// rather than a Turn, so this is a position rather than an amount: what a Turn
/// consumed is the distance between two of these.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct NativeCumulativeUsage {
    fresh_input_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
    output_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
    model_context_window: Option<u64>,
}

impl NativeCumulativeUsage {
    /// The further-on of two readings of one thread, part by part. A thread's
    /// running total only grows, so a part that went backwards belongs to a
    /// reading that arrived late, and the total already reached stands. The
    /// context window is not a count that accrues, so the stated one wins.
    pub(super) fn furthest_of(self, other: Self) -> Self {
        Self {
            fresh_input_tokens: furthest_count(self.fresh_input_tokens, other.fresh_input_tokens),
            cache_read_tokens: furthest_count(self.cache_read_tokens, other.cache_read_tokens),
            cache_write_tokens: furthest_count(self.cache_write_tokens, other.cache_write_tokens),
            output_tokens: furthest_count(self.output_tokens, other.output_tokens),
            reasoning_tokens: furthest_count(self.reasoning_tokens, other.reasoning_tokens),
            model_context_window: other.model_context_window.or(self.model_context_window),
        }
    }

    /// What was consumed between `baseline` and this reading. The context
    /// window is a property of the Model rather than a count that accrues, so
    /// it is carried across as it stands.
    pub(super) fn since(self, baseline: Self) -> Usage {
        Usage {
            fresh_input_tokens: accrued(self.fresh_input_tokens, baseline.fresh_input_tokens),
            cache_read_tokens: accrued(self.cache_read_tokens, baseline.cache_read_tokens),
            cache_write_tokens: accrued(self.cache_write_tokens, baseline.cache_write_tokens),
            output_tokens: accrued(self.output_tokens, baseline.output_tokens),
            reasoning_tokens: accrued(self.reasoning_tokens, baseline.reasoning_tokens),
            native_meter: None,
            model_context_window: self.model_context_window,
        }
    }
}

/// The larger of two readings of one part, keeping a part only one of them
/// measured rather than losing it.
fn furthest_count(held: Option<u64>, stated: Option<u64>) -> Option<u64> {
    match (held, stated) {
        (Some(held), Some(stated)) => Some(held.max(stated)),
        (held, stated) => held.or(stated),
    }
}

/// One part's distance from where it stood at the baseline. A part the
/// baseline never measured is read as having started at nothing, and a total
/// that went backwards — which Codex has no reason to do — accrues nothing
/// rather than wrapping.
fn accrued(latest: Option<u64>, baseline: Option<u64>) -> Option<u64> {
    Some(latest?.saturating_sub(baseline.unwrap_or(0)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ThreadSettingsUpdatedParams {
    pub(super) thread_id: String,
    pub(super) thread_settings: EffectiveThreadSettings,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct EffectiveThreadSettings {
    pub(super) model: String,
    #[serde(default, deserialize_with = "deserialize_native_field")]
    pub(super) effort: NativeField<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_native_field")]
    pub(super) service_tier: NativeField<Option<String>>,
}

/// Distinguishes a field Codex left out entirely from one it sent, including as `null`.
#[derive(Default)]
pub(super) enum NativeField<T> {
    #[default]
    Omitted,
    Present(T),
}

fn deserialize_native_field<'de, D, T>(deserializer: D) -> Result<NativeField<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(NativeField::Present)
}

// Decoded notifications.

/// The final Agent Message repeated in a successful `turn/completed`
/// notification. Codex carries it as a summary fallback for clients that did
/// not observe the canonical item completion.
pub(super) struct CompletedNativeAgentMessage {
    pub(super) item_id: String,
    pub(super) text: String,
}

/// A Codex notification Suru understands, decoded out of its wire params.
pub(super) enum NativeNotification {
    TurnStarted {
        thread_id: String,
        turn_id: String,
    },
    QuestionnaireRequested {
        id: RequestId,
        params: UserInputParams,
    },
    CommandApprovalRequested {
        id: RequestId,
        params: CommandApprovalParams,
    },
    FileChangeApprovalRequested {
        id: RequestId,
        params: FileChangeApprovalParams,
    },
    PermissionsApprovalRequested {
        id: RequestId,
        params: PermissionsApprovalParams,
        /// Provider-owned callback payload, kept apart from the redacted copy
        /// projected into durable history.
        native_permissions: Value,
    },
    QuestionnaireResolved {
        thread_id: String,
        request_id: RequestId,
    },
    SkillsChanged,
    AgentSelectionChanged {
        thread_id: String,
        model: String,
        effort: NativeField<Option<String>>,
        service_tier: NativeField<Option<String>>,
    },
    AgentMessageStarted {
        thread_id: String,
        turn_id: String,
        item_id: String,
    },
    AgentMessageDelta {
        thread_id: String,
        turn_id: String,
        item_id: String,
        delta: String,
    },
    AgentMessageCompleted {
        thread_id: String,
        turn_id: String,
        item_id: String,
        text: String,
    },
    CommandStarted {
        thread_id: String,
        turn_id: String,
        item_id: String,
        command: String,
        cwd: Option<PathBuf>,
        status: NativeCommandStatus,
    },
    CommandOutputDelta {
        thread_id: String,
        turn_id: String,
        item_id: String,
        delta: String,
    },
    CommandCompleted {
        thread_id: String,
        turn_id: String,
        item_id: String,
        aggregated_output: Option<String>,
        exit_status: Option<i32>,
        status: NativeCommandStatus,
    },
    FileChangeStarted {
        thread_id: String,
        turn_id: String,
        item_id: String,
        changes: Vec<NativeFileChange>,
        status: NativeFileChangeStatus,
    },
    FileChangeUpdated {
        thread_id: String,
        turn_id: String,
        item_id: String,
        changes: Vec<NativeFileChange>,
    },
    FileChangeCompleted {
        thread_id: String,
        turn_id: String,
        item_id: String,
        changes: Vec<NativeFileChange>,
        status: NativeFileChangeStatus,
    },
    ReasoningStarted {
        thread_id: String,
        turn_id: String,
        item_id: String,
    },
    ReasoningDelta {
        thread_id: String,
        turn_id: String,
        item_id: String,
        delta: String,
        summary_index: usize,
    },
    ReasoningSectionBreak {
        thread_id: String,
        turn_id: String,
        item_id: String,
        summary_index: usize,
    },
    ReasoningCompleted {
        thread_id: String,
        turn_id: String,
        item_id: String,
        summary: Vec<String>,
    },
    TurnCompleted {
        thread_id: String,
        turn_id: String,
        outcome: NativeTurnOutcome,
        final_agent_message: Option<CompletedNativeAgentMessage>,
    },
    /// Codex's latest running total for `thread_id`, keyed by the native Turn
    /// it was measured under.
    TokenUsage {
        thread_id: String,
        turn_id: String,
        total: NativeCumulativeUsage,
        context_fill: Option<crate::protocol::ContextFill>,
    },
    /// A collab tool call completing on `thread_id` — a spawn naming the child
    /// threads it opened, or any later call carrying Codex's view of the
    /// spawned agents' lifecycles. The call's own start says nothing Suru
    /// presents, so only the completion is decoded.
    CollabCallCompleted {
        thread_id: String,
        tool: NativeCollabTool,
        status: NativeCollabCallStatus,
        receiver_thread_ids: Vec<String>,
        prompt: Option<String>,
        agents_states: BTreeMap<String, NativeCollabAgentState>,
    },
    /// One step of a spawned agent's lifecycle, reported on the spawner
    /// `thread_id`. The turn it rides under is deliberately not carried: a
    /// child's completion may arrive after the spawner's turn completed, and
    /// is expected then rather than stale.
    SubagentActivity {
        thread_id: String,
        kind: NativeSubagentActivityKind,
        agent_thread_id: String,
        agent_path: String,
    },
}

pub(super) enum NativeTurnOutcome {
    Completed,
    Interrupted,
    Failed {
        message: String,
        kind: NativeTurnFailureKind,
    },
}

pub(super) enum NativeTurnFailureKind {
    BadRequest { additional_details: Option<String> },
    Other,
}
