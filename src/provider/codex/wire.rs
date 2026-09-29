//! Serde types for the Codex app-server V2 protocol and their Suru conversions.
//!
//! Everything Suru writes to or reads from the app-server is shaped here, together with the
//! mappings between those native shapes and Suru's own protocol types. Modules above this one
//! work in Suru terms and never touch raw JSON.

use std::{collections::BTreeMap, path::PathBuf};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{DEFAULT_SERVICE_TIER_CHOICE_ID, REASONING_EFFORT_OPTION_ID, SERVICE_TIER_OPTION_ID};
use crate::{
    broker::{BROKER_CALL_TIMEOUT, BROKER_SERVER_NAME, instruction_note},
    protocol::{
        AgentSelection, CodexApprovalPolicy, CodexSandboxMode, FileChange, ModelAvailability,
        ModelDescriptor, ModelId, ModelOptionChoice, ModelOptionChoiceId, ModelOptionDescriptor,
        ModelOptionId, ModelOptionKind, ModelOptionRole, ModelOptionValue, ProviderId,
        ReasoningSummaryDetail, Usage,
    },
    provider::{
        BrokerHandoff, ProviderAttachment, ProviderError, exclusive_count, humanized_wire_id,
        reported_count,
    },
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

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct CodexPosture {
    pub(super) approval_policy: CodexApprovalPolicy,
    pub(super) sandbox_mode: CodexSandboxMode,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) config: Option<&'a ThreadConfig>,
    /// What the thread's Agent is told beyond Codex's own instructions, in place of the developer
    /// instructions the user's configuration sets.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) developer_instructions: Option<&'a str>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ThreadResumeParams<'a> {
    pub(super) thread_id: &'a str,
    pub(super) cwd: &'a str,
    pub(super) approval_policy: &'a str,
    pub(super) sandbox: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) config: Option<&'a ThreadConfig>,
    /// What the resumed thread's Agent is told beyond Codex's own instructions, in place of the
    /// developer instructions the user's configuration sets. A resumed thread reads them from the
    /// configuration it is resumed under rather than from its history, and a compaction rebuilds
    /// its opening context from them, so a resume that carried none would lose what the start said
    /// at the thread's first compaction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) developer_instructions: Option<&'a str>,
}

/// The Codex configuration a thread is started or resumed under beyond the user's own: each entry
/// keyed by the dotted path it sets, applied the way a `-c` override is, so an entry replaces that
/// one value and leaves the rest of the user's configuration standing.
pub(super) type ThreadConfig = serde_json::Map<String, Value>;

/// The Broker as one thread's MCP server, which is all a thread's `config` carries: the entry
/// `mcp_servers.suru`, reached over streamable HTTP with the token as a static `Authorization`
/// header — never `bearer_token`, which Codex refuses for streamable HTTP — with a per-call timeout
/// above the Broker's longest call, since progress does not extend it, and every Tool approved by
/// default, since otherwise a call waits on Codex's automatic reviewer or raises an elicitation
/// Suru refuses (docs/validation/0408-codex-per-thread-mcp-config.md). Codex keeps none of it with
/// the thread, so a resume is handed it again.
pub(super) fn broker_thread_config(handoff: &BrokerHandoff) -> ThreadConfig {
    let (header, value) = handoff.authorization_header();
    let server = NativeBrokerServer {
        url: handoff.endpoint().as_str(),
        http_headers: BTreeMap::from([(header, value)]),
        tool_timeout_sec: BROKER_CALL_TIMEOUT.as_secs_f64(),
        default_tools_approval_mode: "approve",
    };
    ThreadConfig::from_iter([(
        format!("mcp_servers.{BROKER_SERVER_NAME}"),
        serde_json::to_value(server).expect("a Broker server entry serializes"),
    )])
}

/// The developer instructions a thread handed the Broker is started and resumed with: those the
/// user's own configuration sets, `user`, and after them, a blank line apart, the Broker's note,
/// naming each Tool as Codex names an MCP server's Tools to its Agent — `mcp__suru__spawn_subagent`.
/// A user who sets none, or only whitespace, gets the note alone.
///
/// Codex takes a thread's developer instructions in place of those the user's configuration sets
/// rather than beside them — the thread's value, where given, wins outright — so the note alone
/// would silently take the user's away. Appending it to theirs is what keeps the note, as on every
/// other harness, an addition to the Agent's instructions; the Session reads `user` through
/// [`ConfigReadParams`] first.
pub(super) fn broker_developer_instructions(user: Option<&str>) -> String {
    let note = instruction_note(|tool| format!("mcp__{BROKER_SERVER_NAME}__{tool}"));
    match user.filter(|user| !user.trim().is_empty()) {
        Some(user) => format!("{user}\n\n{note}"),
        None => note,
    }
}

// The user's own configuration.

/// Asks the app-server for the configuration a thread working in `cwd` is built from: the user's
/// `config.toml`, the project layers between `cwd` and its project root, and any managed layer,
/// merged as Codex merges them for `thread/start` and `thread/resume`. Suru reads it for one value
/// alone — the developer instructions the user sets — because a thread's own take their place, and
/// a Session handed the Broker appends its note to them rather than dropping them
/// ([`broker_developer_instructions`]). It is read on each launch, just before the thread is
/// started or resumed, so an edit the user makes between launches is honored at the next.
#[derive(Serialize)]
pub(super) struct ConfigReadParams<'a> {
    pub(super) cwd: &'a str,
}

/// What `config/read` answers with, as far as Suru reads it.
#[derive(Deserialize)]
pub(super) struct NativeConfigRead {
    pub(super) config: NativeEffectiveConfig,
}

/// The effective configuration, keyed as `config.toml` spells it rather than in the protocol's own
/// camel case.
#[derive(Deserialize)]
pub(super) struct NativeEffectiveConfig {
    pub(super) developer_instructions: Option<String>,
}

/// One streamable-HTTP MCP server entry, in the keys Codex's configuration reads.
#[derive(Serialize)]
struct NativeBrokerServer<'a> {
    url: &'a str,
    http_headers: BTreeMap<&'static str, String>,
    /// Seconds, which Codex reads as a float.
    tool_timeout_sec: f64,
    default_tools_approval_mode: &'static str,
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

/// `turn/start` on a native Subagent's own thread, carrying input Suru hands
/// that Subagent itself — Subagent Reports (ADR 0035). Codex begins a turn
/// with it on a thread with none running and steers the one running
/// otherwise, answering that turn either way; it refuses a multi-agent v2
/// sub-agent's thread any direct input, and takes it on a collab (v1) child's.
/// It names no Model, effort, service tier or Reasoning summary, so the
/// Subagent goes on as its spawn set it running whether the input begins a
/// turn or steers one, and restates only the posture its thread was attached
/// under, since a native Subagent acts under the posture of the Session that
/// spawned it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SubagentTurnStartParams<'a> {
    pub(super) thread_id: &'a str,
    pub(super) input: &'a [UserInput],
    pub(super) approval_policy: &'a str,
    pub(super) sandbox_policy: NativeSandboxPolicy,
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
    Text {
        text: String,
    },
    Skill {
        name: String,
        path: PathBuf,
    },
    /// An image inline, its bytes carried in a base64 `data:` URL. Suru never
    /// sends Codex a remote URL or a local path for one (ADR 0037).
    Image {
        url: String,
    },
}

impl UserInput {
    pub(super) fn image(attachment: &ProviderAttachment) -> Self {
        Self::Image {
            url: format!(
                "data:{};base64,{}",
                attachment.mime_type,
                STANDARD.encode(&attachment.bytes)
            ),
        }
    }
}

/// The text the `UserMessage` item a turn's `input` arrives as reads as, as
/// [`user_message_text`] reads it: each piece on a line of its own, a Skill
/// or an image as Codex previews it.
pub(super) fn user_input_text(input: &[UserInput]) -> String {
    input
        .iter()
        .map(|input| match input {
            UserInput::Text { text } => text.clone(),
            UserInput::Skill { name, path } => format!("[skill:${name}]({})", path.display()),
            UserInput::Image { .. } => "[image]".to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
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
    /// Input a thread's agent received: on the Session's own thread, the
    /// user's Prompt; on a child thread, which has no user, a Delegation —
    /// the one a native turn opens with, and each one a running turn drains
    /// afterwards, which is how a `sendInput` into that turn steers it.
    UserMessage {
        #[serde(default)]
        content: Vec<NativeUserInput>,
    },
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
        id: String,
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
    /// A call to a Tool an MCP server hosts — a server the user configured,
    /// or the Broker Suru serves.
    McpToolCall(NativeMcpToolCall),
    /// A search Codex's own web search Tool ran, or a page it opened or
    /// looked through.
    WebSearch(NativeWebSearch),
    /// An image Codex's own Tool showed the Model.
    ImageView(NativeImageView),
    /// An image Codex's own Tool drew.
    ImageGeneration(NativeImageGeneration),
    /// A pause Codex's own Tool took.
    Sleep(NativeSleep),
    /// Every other item, recorded as nothing: those that are no use of a Tool
    /// — a context compaction, a review entered or left, a hook's prompt, a
    /// plan — and a call to a dynamic Tool, which Suru never offers.
    #[serde(other)]
    Unknown,
}

/// One use of a Tool, reported as an item of one of the kinds that report
/// nothing but a Tool's use — a shell run, an edit, and a collab call each
/// have an item kind of their own, which their own Activities read. What
/// each use is to a Transcript is [`super::tools`]'s to say.
pub(super) enum NativeToolUse {
    Mcp(NativeMcpToolCall),
    WebSearch(NativeWebSearch),
    ImageView(NativeImageView),
    ImageGeneration(NativeImageGeneration),
    Sleep(NativeSleep),
}

/// A call to an MCP server's Tool. Only the completed item carries how it
/// went: the Tool's result, or the error of a call Codex could not make.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NativeMcpToolCall {
    pub(super) id: String,
    pub(super) server: String,
    pub(super) tool: String,
    pub(super) status: NativeToolCallStatus,
    #[serde(default)]
    pub(super) arguments: Value,
    #[serde(default)]
    pub(super) result: Option<NativeMcpToolCallResult>,
    #[serde(default)]
    pub(super) error: Option<NativeMcpToolCallError>,
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(super) enum NativeToolCallStatus {
    InProgress,
    Completed,
    Failed,
    /// A status this build does not know, read as no failure rather than
    /// failing the Session, because the wire grows freely.
    #[serde(other)]
    Other,
}

/// The result an MCP server's Tool answered with. Codex reports a result
/// the Tool itself marked as an error as a failed call with the result
/// standing, so a failed call may carry one.
#[derive(Deserialize)]
pub(super) struct NativeMcpToolCallResult {
    #[serde(default)]
    pub(super) content: Vec<NativeMcpContent>,
}

#[derive(Deserialize)]
pub(super) struct NativeMcpToolCallError {
    pub(super) message: String,
}

/// One block of an MCP Tool's result. Only text reads as output; an image,
/// audio, a resource or a link to one — or a block of a kind this build has
/// never heard of — is a part the output leaves out, kept only as its kind
/// rather than as the bytes it carries.
pub(super) enum NativeMcpContent {
    Text(String),
    Omitted,
}

impl<'de> Deserialize<'de> for NativeMcpContent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let block = Value::deserialize(deserializer)?;
        Ok(
            match (
                block.get("type").and_then(Value::as_str),
                block.get("text").and_then(Value::as_str),
            ) {
                (Some("text"), Some(text)) => Self::Text(text.to_owned()),
                _ => Self::Omitted,
            },
        )
    }
}

/// A web search. Codex starts the item before it knows what it searches, so
/// only the completed item names the search, in `action`, beside `query`:
/// Codex's own one-line reading of that action.
#[derive(Deserialize)]
pub(super) struct NativeWebSearch {
    pub(super) id: String,
    #[serde(default)]
    pub(super) query: Option<String>,
    #[serde(default)]
    pub(super) action: Option<NativeWebSearchAction>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(super) enum NativeWebSearchAction {
    Search {
        #[serde(default)]
        query: Option<String>,
        #[serde(default)]
        queries: Option<Vec<String>>,
    },
    OpenPage {
        #[serde(default)]
        url: Option<String>,
    },
    FindInPage {
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        pattern: Option<String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
pub(super) struct NativeImageView {
    pub(super) id: String,
    pub(super) path: String,
}

/// An image generation. Codex starts the item before the prompt it draws
/// is settled, so only the completed item carries it, beside the path the
/// image was saved at — or, for a generation that failed, why it failed.
/// The image itself, which the item also carries, is never decoded.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NativeImageGeneration {
    pub(super) id: String,
    #[serde(default)]
    pub(super) status: NativeImageGenerationStatus,
    #[serde(default)]
    pub(super) revised_prompt: Option<String>,
    #[serde(default)]
    pub(super) saved_path: Option<String>,
    #[serde(default)]
    pub(super) failure: Option<NativeImageGenerationFailure>,
}

/// Why an image generation failed: a typed reason — `usageLimitExceeded`
/// is the one Codex sends today — and whatever message comes with it. The
/// reason is kept as Codex spells it rather than decoded against the ones
/// this build knows, because the wire grows freely (ADR 0010); what else
/// a reason carries, such as which limit ran out, is not read.
#[derive(Deserialize)]
pub(super) struct NativeImageGenerationFailure {
    #[serde(rename = "type", default)]
    pub(super) reason: String,
    #[serde(default)]
    pub(super) message: Option<String>,
}

/// How an image generation went, in the Responses API's words Codex passes
/// on — `in_progress`, `generating`, `completed`, `failed` — of which only a
/// failure changes how its Tool Call settles.
#[derive(Clone, Copy, Default, Deserialize, Eq, PartialEq)]
pub(super) enum NativeImageGenerationStatus {
    #[serde(rename = "failed")]
    Failed,
    #[default]
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NativeSleep {
    pub(super) id: String,
    #[serde(default)]
    pub(super) duration_ms: Option<u64>,
}

/// The Tool use an item as Codex's app-server v2 `ThreadItem` serializes it reports, for tests
/// that start from the wire.
#[cfg(test)]
pub(super) fn tool_use_item(item: Value) -> NativeToolUse {
    match serde_json::from_value(item.clone()).expect("the item decodes") {
        NativeItem::McpToolCall(call) => NativeToolUse::Mcp(call),
        NativeItem::WebSearch(search) => NativeToolUse::WebSearch(search),
        NativeItem::ImageView(view) => NativeToolUse::ImageView(view),
        NativeItem::ImageGeneration(generation) => NativeToolUse::ImageGeneration(generation),
        NativeItem::Sleep(sleep) => NativeToolUse::Sleep(sleep),
        _ => panic!("{item} reports no Tool use"),
    }
}

/// One piece of the input a [`NativeItem::UserMessage`] carries. Only text
/// reads as itself; every other kind reads as the placeholder Codex's own
/// `render_input_preview` gives it, which is also the `prompt` a collab call
/// carries for the same input — so a Delegation reads the same from either.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(super) enum NativeUserInput {
    Text {
        text: String,
    },
    Image {},
    LocalImage {
        path: String,
    },
    Audio {},
    LocalAudio {
        path: String,
    },
    Skill {
        name: String,
        path: String,
    },
    Mention {
        name: String,
        path: String,
    },
    #[serde(other)]
    Other,
}

/// The text a [`NativeItem::UserMessage`] reads as: each piece of its input on
/// a line of its own, exactly as Codex previews the input a collab call hands
/// an agent.
pub(super) fn user_message_text(content: &[NativeUserInput]) -> String {
    content
        .iter()
        .map(|input| match input {
            NativeUserInput::Text { text } => text.clone(),
            NativeUserInput::Image {} => "[image]".to_owned(),
            NativeUserInput::LocalImage { path } => format!("[local_image:{path}]"),
            NativeUserInput::Audio {} => "[audio]".to_owned(),
            NativeUserInput::LocalAudio { path } => format!("[local_audio:{path}]"),
            NativeUserInput::Skill { name, path } => format!("[skill:${name}]({path})"),
            NativeUserInput::Mention { name, path } => format!("[mention:${name}]({path})"),
            NativeUserInput::Other => "[input]".to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The collab tools whose calls Suru reads something from; the rest of the
/// suite decodes as [`NativeCollabTool::Other`] and is passed over but for
/// the lifecycle states every call reports. `resumeAgent` is one of the rest:
/// it reloads a closed child under its thread id without starting a turn, so
/// only the `sendInput` after it resumes anything.
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

/// The one `codexErrorInfo` Suru reads a meaning from. Codex's union mixes bare
/// names (`"badRequest"`, `"flexUnavailable"`) with single-key objects that
/// carry details (`{"httpConnectionFailed": {"httpStatusCode": 502}}`) and grows
/// freely, so every shape other than a bad request reads as [`Self::Other`]
/// rather than failing the whole `turn/completed` it rides on.
pub(super) enum NativeCodexErrorInfo {
    BadRequest,
    Other,
}

impl<'de> Deserialize<'de> for NativeCodexErrorInfo {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(match Value::deserialize(deserializer)? {
            Value::String(name) if name == "badRequest" => Self::BadRequest,
            _ => Self::Other,
        })
    }
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
    /// A use of a Tool starting on `thread_id`, as far as its item yet says.
    /// A use whose effect another Activity records is never decoded as one.
    ToolUseStarted {
        thread_id: String,
        turn_id: String,
        tool: NativeToolUse,
    },
    /// A use of a Tool completing on `thread_id`, its item saying how it went.
    ToolUseCompleted {
        thread_id: String,
        turn_id: String,
        tool: NativeToolUse,
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
    /// Input a thread's agent received — on a child thread, a Delegation.
    /// Only the completed item is decoded, since the input arrives whole.
    UserMessage {
        thread_id: String,
        turn_id: String,
        text: String,
    },
    /// A collab tool call starting on `thread_id`. Only a `sendInput`'s start
    /// is read: it names what the call is handing its receiver before Codex
    /// delivers it, so a steer the receiver drains is known by its sender
    /// even when it arrives ahead of the call's completion.
    CollabCallStarted {
        thread_id: String,
        call_id: String,
        tool: NativeCollabTool,
        receiver_thread_ids: Vec<String>,
        prompt: Option<String>,
    },
    /// A collab tool call completing on `thread_id` — a spawn naming the child
    /// threads it opened, or any later call carrying Codex's view of the
    /// spawned agents' lifecycles.
    CollabCallCompleted {
        thread_id: String,
        call_id: String,
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        ConfigReadParams, NativeConfigRead, NativeItem, NativeMcpContent, NativeToolCallStatus,
        NativeWebSearchAction, ThreadResumeParams, ThreadStartParams,
        broker_developer_instructions, broker_thread_config,
    };
    use crate::{broker::instruction_note, provider::BrokerHandoff};

    fn item(item: serde_json::Value) -> NativeItem {
        serde_json::from_value(item.clone()).unwrap_or_else(|_| panic!("{item} decodes"))
    }

    /// The item kinds that are no use of a Tool Suru records, as Codex's app-server v2
    /// `ThreadItem` serializes them, stay unknown: a context compaction, a review entered and
    /// left, a hook's prompt, a plan, and a call to a dynamic Tool, which Suru never offers.
    #[test]
    fn items_that_are_no_tool_use_decode_as_unknown() {
        for unknown in [
            json!({"type": "contextCompaction", "id": "compaction"}),
            json!({"type": "enteredReviewMode", "id": "review", "review": "current changes"}),
            json!({"type": "exitedReviewMode", "id": "review", "review": "Looks good."}),
            json!({
                "type": "hookPrompt", "id": "hook",
                "fragments": [{"text": "Mind the style.", "hookRunId": "run"}],
            }),
            json!({"type": "plan", "id": "plan", "text": "1. Map the seam"}),
            json!({
                "type": "dynamicToolCall", "id": "dynamic", "namespace": null, "tool": "lookup",
                "arguments": {}, "status": "completed", "contentItems": [], "success": true,
                "durationMs": 1,
            }),
        ] {
            assert!(
                matches!(item(unknown.clone()), NativeItem::Unknown),
                "{unknown} is recorded as nothing"
            );
        }
    }

    /// Each Tool use item decodes as its own kind, keeping what its Tool Call reads and passing
    /// over the rest, the generated image's bytes above all.
    #[test]
    fn each_tool_use_item_decodes_as_its_own_kind() {
        let NativeItem::McpToolCall(call) = item(json!({
            "type": "mcpToolCall", "id": "call", "server": "github", "tool": "create_issue",
            "status": "failed", "arguments": {"title": "Fix"},
            "appContext": null, "mcpAppUi": null, "pluginId": null, "readOnlyHint": true,
            "result": {
                "content": [{"type": "text", "text": "No."}, {"type": "image", "data": "AA=="}],
                "structuredContent": null, "_meta": null,
            },
            "error": {"message": "refused"}, "durationMs": 3,
        })) else {
            panic!("an MCP tool call decodes as one");
        };
        assert_eq!(
            (call.id.as_str(), call.server.as_str(), call.tool.as_str()),
            ("call", "github", "create_issue")
        );
        assert!(call.status == NativeToolCallStatus::Failed);
        assert_eq!(call.arguments, json!({"title": "Fix"}));
        let content = &call.result.as_ref().expect("the result decodes").content;
        assert!(matches!(
            content.as_slice(),
            [NativeMcpContent::Text(text), NativeMcpContent::Omitted] if text == "No."
        ));
        assert_eq!(
            call.error.as_ref().map(|error| error.message.as_str()),
            Some("refused")
        );

        let NativeItem::WebSearch(search) = item(json!({
            "type": "webSearch", "id": "ws", "query": "spawn in https://docs.rs",
            "action": {"type": "findInPage", "url": "https://docs.rs", "pattern": "spawn"},
            "results": [{"title": "Tokio"}],
        })) else {
            panic!("a web search decodes as one");
        };
        assert!(matches!(
            search.action,
            Some(NativeWebSearchAction::FindInPage { url: Some(url), pattern: Some(pattern) })
                if url == "https://docs.rs" && pattern == "spawn"
        ));
        let NativeItem::WebSearch(started) =
            item(json!({"type": "webSearch", "id": "ws", "query": "", "action": null}))
        else {
            panic!("a started web search decodes as one");
        };
        assert!(started.action.is_none());

        assert!(matches!(
            item(json!({"type": "imageView", "id": "view", "path": "chart.png"})),
            NativeItem::ImageView(view) if view.path == "chart.png"
        ));
        assert!(matches!(
            item(json!({
                "type": "imageGeneration", "id": "gen", "status": "completed",
                "revisedPrompt": "A fox", "result": "iVBORw0KGgo=", "savedPath": "fox.png",
            })),
            NativeItem::ImageGeneration(generation)
                if generation.revised_prompt.as_deref() == Some("A fox")
                    && generation.saved_path.as_deref() == Some("fox.png")
        ));
        assert!(matches!(
            item(json!({"type": "sleep", "id": "nap", "durationMs": 1500})),
            NativeItem::Sleep(sleep) if sleep.duration_ms == Some(1500)
        ));
    }

    #[test]
    fn the_broker_is_one_dotted_mcp_server_override_approved_by_default() {
        let handoff = BrokerHandoff::for_tests("http://127.0.0.1:1/broker");
        let config = broker_thread_config(&handoff);
        assert_eq!(
            serde_json::to_value(&config).expect("the config serializes"),
            json!({
                "mcp_servers.suru": {
                    "url": "http://127.0.0.1:1/broker",
                    "http_headers": {"Authorization": handoff.token().bearer()},
                    "tool_timeout_sec": 900.0,
                    "default_tools_approval_mode": "approve",
                },
            })
        );
        let note = broker_developer_instructions(None);
        let start = serde_json::to_value(ThreadStartParams {
            cwd: "/workspace",
            approval_policy: "on-request",
            sandbox: "workspace-write",
            ephemeral: false,
            config: Some(&config),
            developer_instructions: Some(&note),
        })
        .expect("thread/start serializes");
        assert_eq!(start["config"], serde_json::to_value(&config).unwrap());
        assert_eq!(start["developerInstructions"], note.as_str());
        assert!(
            note.contains("mcp__suru__spawn_subagent"),
            "the note names the Broker's Tools as Codex does: {note}"
        );
    }

    fn codex_note() -> String {
        instruction_note(|tool| format!("mcp__suru__{tool}"))
    }

    #[test]
    fn the_broker_note_follows_the_users_own_developer_instructions_a_blank_line_apart() {
        assert_eq!(
            broker_developer_instructions(Some("Answer tersely.\nCite paths.")),
            format!("Answer tersely.\nCite paths.\n\n{}", codex_note())
        );
    }

    #[test]
    fn a_user_who_sets_no_developer_instructions_is_handed_the_note_alone() {
        for user in [None, Some(""), Some(" \n\t")] {
            assert_eq!(
                broker_developer_instructions(user),
                codex_note(),
                "{user:?}"
            );
        }
    }

    #[test]
    fn the_configuration_is_read_for_the_threads_directory_and_answered_in_config_toml_keys() {
        assert_eq!(
            serde_json::to_value(ConfigReadParams { cwd: "/workspace" }).unwrap(),
            json!({ "cwd": "/workspace" })
        );
        let read: NativeConfigRead = serde_json::from_value(json!({
            "config": {
                "model": "gpt-5.5",
                "developer_instructions": "Answer tersely.",
                "developerInstructions": "not how Codex keys its configuration",
                "features": { "unknown": true },
            },
            "origins": {},
        }))
        .unwrap();
        assert_eq!(
            read.config.developer_instructions.as_deref(),
            Some("Answer tersely.")
        );
        for config in [json!({}), json!({ "developer_instructions": null })] {
            let read: NativeConfigRead =
                serde_json::from_value(json!({ "config": config, "origins": {} })).unwrap();
            assert_eq!(read.config.developer_instructions, None, "{config}");
        }
    }

    #[test]
    fn a_thread_handed_no_broker_overrides_nothing() {
        let resume = serde_json::to_value(ThreadResumeParams {
            thread_id: "thread",
            cwd: "/workspace",
            approval_policy: "on-request",
            sandbox: "workspace-write",
            config: None,
            developer_instructions: None,
        })
        .expect("thread/resume serializes");
        assert!(resume.get("config").is_none(), "{resume}");
        assert!(resume.get("developerInstructions").is_none(), "{resume}");
    }
}
