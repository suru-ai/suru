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
        AgentSelection, FileChange, ModelAvailability, ModelDescriptor, ModelId, ModelOptionChoice,
        ModelOptionChoiceId, ModelOptionDescriptor, ModelOptionId, ModelOptionKind,
        ModelOptionRole, ModelOptionValue, ProviderId, ReasoningSummaryDetail,
    },
    provider::{ProviderError, humanized_wire_id},
};

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

#[derive(Clone, Debug, Deserialize, Serialize)]
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

/// Codex cannot ask the user anything through Suru, so every thread — the
/// Session's own and each attached collab child — starts pre-approved with the
/// sandbox open. One declaration, so a policy change reaches every site that
/// speaks for a thread.
pub(super) const THREAD_APPROVAL_POLICY: &str = "never";
pub(super) const THREAD_SANDBOX: &str = "danger-full-access";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ThreadStartParams<'a> {
    pub(super) cwd: &'a str,
    pub(super) approval_policy: &'static str,
    pub(super) sandbox: &'static str,
    pub(super) ephemeral: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ThreadResumeParams<'a> {
    pub(super) thread_id: &'a str,
    pub(super) cwd: &'a str,
    pub(super) approval_policy: &'static str,
    pub(super) sandbox: &'static str,
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
    /// How much detail Codex should summarize its Reasoning in. A Turn that
    /// omits it inherits the Model's own default, which is why Suru always
    /// states one.
    pub(super) summary: &'static str,
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
pub(super) struct TurnCompletedParams {
    pub(super) thread_id: String,
    pub(super) turn: CompletedNativeTurn,
}

#[derive(Deserialize)]
pub(super) struct CompletedNativeTurn {
    pub(super) id: String,
    pub(super) status: NativeTurnStatus,
    pub(super) error: Option<NativeTurnError>,
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

/// A Codex notification Suru understands, decoded out of its wire params.
pub(super) enum NativeNotification {
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
