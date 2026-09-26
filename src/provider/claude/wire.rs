//! The serde types Suru exchanges with a Claude Code CLI over its stream-json wire.
//!
//! The baseline wire is verified against Claude Code CLI 2.1.237 and Agent SDK type definitions
//! 0.3.241; optional get_context_usage is additionally verified against CLI 2.1.260 (see
//! docs/validation/0300-claude-context-fill.md). The wire is an SDK implementation detail, so drift is
//! ours to absorb (ADR 0010). Decoding tolerates fields and control-response subtypes it does not
//! know, because the CLI grows both freely.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The envelope a control request travels in, correlated by its `request_id`.
#[derive(Serialize)]
pub(super) struct ControlRequestEnvelope<'a> {
    #[serde(rename = "type")]
    pub(super) kind: &'static str,
    pub(super) request_id: &'a str,
    pub(super) request: &'a ControlRequest,
}

impl<'a> ControlRequestEnvelope<'a> {
    pub(super) fn new(request_id: &'a str, request: &'a ControlRequest) -> Self {
        Self {
            kind: "control_request",
            request_id,
            request,
        }
    }
}

/// A control request Suru issues to the CLI.
#[derive(Serialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub(super) enum ControlRequest {
    ListModels,
    /// Optional snapshot of this process’s own conversation (CLI 2.1.260 verified).
    GetContextUsage,
    /// Asks the CLI which version it is, which is what the availability probe checks against the
    /// suggested version ADR 0010 pins.
    GetBinaryVersion,
    /// Opens the session handshake without starting anything: the CLI answers with what it would
    /// run the conversation under, the account it holds credentials for among it. The probe sends
    /// it bare — no hooks, no SDK MCP servers, no agents — because all it reads is the account.
    Initialize,
    /// Refreshes and returns only enabled, user-invocable native Skills. Unlike the broader
    /// `initialize.commands` list, this excludes built-in and other non-Skill slash commands.
    ReloadSkills,
    SetPermissionMode {
        mode: crate::protocol::ClaudePermissionMode,
    },
    /// Stops the running loop. The CLI answers with an interrupt receipt and ends the Turn with a
    /// terminal result of its own.
    Interrupt {
        /// Whether the messages the user queued into the loop are dropped with it. Suru always
        /// asks for that: a steer already queued would otherwise survive the interrupt and be
        /// answered afterwards — work the user has just asked to stop, on a Turn that has Settled.
        /// The CLI advertises this as the `interrupt_cancel_queued_v1` capability on its init
        /// message, which ADR 0010's suggested version carries.
        cancel_queued: bool,
    },
    /// Stops one of the background tasks the agent spawned, named by the id the CLI reported it
    /// started under.
    StopTask {
        task_id: String,
    },
}

impl ControlRequest {
    /// What the CLI calls this request, so a failure can say which one went unanswered.
    pub(super) const fn subtype(&self) -> &'static str {
        match self {
            Self::ListModels => "list_models",
            Self::GetContextUsage => "get_context_usage",
            Self::GetBinaryVersion => "get_binary_version",
            Self::Initialize => "initialize",
            Self::ReloadSkills => "reload_skills",
            Self::SetPermissionMode { .. } => "set_permission_mode",
            Self::Interrupt { .. } => "interrupt",
            Self::StopTask { .. } => "stop_task",
        }
    }
}

/// A Prompt on its way into the running loop, in the envelope stream-json input takes user
/// messages in. The CLI owns the conversation's identity, so the envelope's `session_id` rides
/// along empty and `parent_tool_use_id` marks the message as the user's own rather than a
/// subagent's.
#[derive(Serialize)]
pub(super) struct UserMessageEnvelope<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    message: UserMessage<'a>,
    parent_tool_use_id: Option<&'static str>,
    session_id: &'static str,
}

impl<'a> UserMessageEnvelope<'a> {
    pub(super) fn text(prompt: &'a str) -> Self {
        Self {
            kind: "user",
            message: UserMessage {
                role: "user",
                content: [UserContentBlock {
                    kind: "text",
                    text: prompt,
                }],
            },
            parent_tool_use_id: None,
            session_id: "",
        }
    }
}

#[derive(Serialize)]
struct UserMessage<'a> {
    role: &'static str,
    content: [UserContentBlock<'a>; 1],
}

#[derive(Serialize)]
struct UserContentBlock<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
}

/// A partial-message chunk of the Turn's streamed output: one Anthropic streaming event, owned by
/// the conversation itself or by the subagent whose spawning tool use `parent_tool_use_id` names.
#[derive(Deserialize)]
pub(super) struct StreamEventMessage {
    pub(super) event: StreamEvent,
    #[serde(default)]
    pub(super) parent_tool_use_id: Option<String>,
}

/// One streaming event, decoded only as far as the projection reads. `kind` stays a free string so
/// events this build does not know are ridden out rather than failed (ADR 0010).
#[derive(Deserialize)]
pub(super) struct StreamEvent {
    #[serde(rename = "type")]
    pub(super) kind: String,
    #[serde(default)]
    pub(super) index: Option<u64>,
    #[serde(default)]
    pub(super) content_block: Option<ContentBlock>,
    #[serde(default)]
    pub(super) delta: Option<ContentDelta>,
}

/// The content block a `content_block_start` opens: the text or thinking it opens with, and for a
/// `tool_use` block the tool's identity and whatever of its input rode along rather than streaming
/// in `input_json_delta` increments.
#[derive(Deserialize)]
pub(super) struct ContentBlock {
    #[serde(rename = "type", default)]
    pub(super) kind: String,
    #[serde(default)]
    pub(super) text: Option<String>,
    #[serde(default)]
    pub(super) thinking: Option<String>,
    #[serde(default)]
    pub(super) id: Option<String>,
    #[serde(default)]
    pub(super) name: Option<String>,
    #[serde(default)]
    pub(super) input: Option<Value>,
}

/// The increment a `content_block_delta` carries. A `message_delta`'s delta object carries no
/// `type` at all — verified against the live CLI — so an absent kind decodes rather than fails.
#[derive(Deserialize)]
pub(super) struct ContentDelta {
    #[serde(rename = "type", default)]
    pub(super) kind: String,
    #[serde(default)]
    pub(super) text: Option<String>,
    #[serde(default)]
    pub(super) thinking: Option<String>,
    #[serde(default)]
    pub(super) partial_json: Option<String>,
}

/// A full-message snapshot of one assistant conversation message. The loop's own conversation
/// streams in `stream_event` chunks and its snapshots restate them, but a subagent's conversation
/// never streams — its snapshots, attributed by `parent_tool_use_id`, are the wire's only account
/// of its work.
#[derive(Deserialize)]
pub(super) struct AssistantMessageSnapshot {
    pub(super) message: AssistantMessageBody,
    #[serde(default)]
    pub(super) parent_tool_use_id: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct AssistantMessageBody {
    #[serde(default)]
    pub(super) model: Option<String>,
    #[serde(default)]
    pub(super) content: Vec<ContentBlock>,
}

/// A conversation message the loop echoes back with the `user` role: tool results on their way
/// into the next model call. Decoded only as far as the tool results the projection presents.
#[derive(Deserialize)]
pub(super) struct EchoedUserMessage {
    pub(super) message: EchoedUserBody,
}

#[derive(Deserialize)]
pub(super) struct EchoedUserBody {
    #[serde(default)]
    pub(super) content: EchoedUserContent,
}

/// An echoed user message's content: the block list tool results arrive in, or any other shape —
/// plain Prompt text among them — that carries nothing the projection presents.
#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum EchoedUserContent {
    Blocks(Vec<EchoedUserBlock>),
    Other(serde::de::IgnoredAny),
}

impl Default for EchoedUserContent {
    fn default() -> Self {
        Self::Other(serde::de::IgnoredAny)
    }
}

/// One block of an echoed user message. A `tool_result` block reports the outcome of the tool use
/// `tool_use_id` names; `content` is free-form — a bare string or a list of typed blocks — so it
/// stays undecoded here.
#[derive(Deserialize)]
pub(super) struct EchoedUserBlock {
    #[serde(rename = "type", default)]
    pub(super) kind: String,
    #[serde(default)]
    pub(super) tool_use_id: Option<String>,
    #[serde(default)]
    pub(super) content: Value,
    #[serde(default)]
    pub(super) is_error: bool,
}

/// The terminal message the CLI's loop ends with, whether that loop was a Turn or an Errand's
/// one-shot print: `success` reports a finished loop (which may still carry `is_error`), and every
/// other subtype is a failure whose `errors` say what went wrong.
#[derive(Deserialize)]
pub(super) struct ResultMessage {
    #[serde(default)]
    pub(super) uuid: Option<String>,
    pub(super) subtype: String,
    #[serde(default)]
    pub(super) is_error: bool,
    #[serde(default)]
    pub(super) result: Option<Value>,
    #[serde(default)]
    pub(super) errors: Vec<String>,
    /// Why the loop stopped. An interrupted Turn is the reason this is read at all: the CLI reports
    /// one as an unerrored `success` carrying no answer, so an abort is legible nowhere else.
    #[serde(default)]
    pub(super) terminal_reason: Option<String>,
    /// The answer as the JSON schema the launch asked for shaped it, which is what an Errand came
    /// for. Absent from every result the CLI was given no schema to honor, and from one it could
    /// not honor the schema it was given.
    #[serde(default)]
    pub(super) structured_output: Option<Value>,
    /// The terminal loop result's disjoint Anthropic usage. Each field stays
    /// optional so a CLI that omits one does not turn that omission into zero.
    #[serde(default)]
    pub(super) usage: Option<ResultUsage>,
    #[serde(default)]
    pub(super) total_cost_usd: Option<f64>,
}

#[derive(Deserialize)]
pub(super) struct ResultUsage {
    #[serde(default)]
    pub(super) input_tokens: Option<f64>,
    #[serde(default)]
    pub(super) cache_read_input_tokens: Option<f64>,
    #[serde(default)]
    pub(super) cache_creation_input_tokens: Option<f64>,
    #[serde(default)]
    pub(super) output_tokens: Option<f64>,
}

/// A `system` message: the CLI's own bookkeeping alongside the conversation. Only the task
/// lifecycle is decoded: the background work the agent spawns is what an interrupt has to stop
/// before it stops the loop, a task running an agent is a Subagent, whose start, description
/// changes, and settle the projection presents, and a background task whose settling wakes the
/// loop is a Watch, which keeps its Session Monitoring until it settles.
#[derive(Deserialize)]
pub(super) struct SystemMessage {
    pub(super) subtype: String,
    /// The task's own identity, which every lifecycle message names. For a task running an agent
    /// it is also the agent's: a resume starts the same task again, and a SendMessage addresses
    /// the agent by it.
    #[serde(default)]
    pub(super) task_id: Option<String>,
    /// The tool use that delegated the task, on `task_started`. For a spawn it is the identity
    /// every chunk the subagent streams carries as `parent_tool_use_id`, which is what makes that
    /// conversation the Subagent's. A resumed agent's task starts again naming the SendMessage tool
    /// use instead, while its chunks keep riding under the original spawn's id (verified against
    /// the live 2.1.280 CLI).
    #[serde(default)]
    pub(super) tool_use_id: Option<String>,
    #[serde(default)]
    pub(super) task_type: Option<String>,
    #[serde(default)]
    pub(super) subagent_type: Option<String>,
    /// What the task was asked to do, on `task_started` — repeated unchanged when a resume starts
    /// it again; on `task_progress`, the subagent's latest tool activity instead.
    #[serde(default)]
    pub(super) description: Option<String>,
    /// The text the agent was handed, on `task_started`: a spawn's prompt, and the raw message
    /// when a late SendMessage restarts a finished agent (verified against the live 2.1.280 CLI).
    #[serde(default)]
    pub(super) prompt: Option<String>,
    /// How the task ended, on `task_notification` — `completed`, `failed`, or `stopped`.
    #[serde(default)]
    pub(super) status: Option<String>,
    /// How the task ended in the CLI's own words, on `task_notification` — the same account it
    /// delivers to the agent the settling wakes. Display text, never parsed.
    #[serde(default)]
    pub(super) summary: Option<String>,
    /// What changed about a running task, on `task_updated`.
    #[serde(default)]
    pub(super) patch: Option<TaskPatch>,
}

/// The revision a `task_updated` carries. Only the description is decoded: every way a task ends
/// arrives as its own `task_notification`, so status transitions carry nothing the projection
/// presents.
#[derive(Deserialize)]
pub(super) struct TaskPatch {
    #[serde(default)]
    pub(super) description: Option<String>,
}

/// The CLI's answer to one control request, correlated back by `request_id`.
#[derive(Deserialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub(super) enum ControlResponse {
    Success {
        request_id: String,
        #[serde(default)]
        response: Option<Value>,
    },
    Error {
        request_id: String,
        error: String,
    },
}

/// What `get_binary_version` answers with. The build time rides along on the wire and is not
/// decoded, because the floor is a version.
#[derive(Deserialize)]
pub(super) struct NativeBinaryVersion {
    pub(super) version: String,
}

/// What `initialize` answers with, decoded as far as Suru reads it: the account the CLI would make
/// requests under and the slash commands a user can invoke. A CLI holding no credentials answers
/// with an account naming none rather than by omitting it, but an absent account decodes as one
/// naming none all the same.
#[derive(Deserialize)]
pub(super) struct NativeInitialize {
    #[serde(default)]
    pub(super) account: NativeAccount,
    #[serde(default)]
    pub(super) commands: Vec<NativeSkill>,
}

/// What `reload_skills` answers with: the model-visible Skills Claude resolved from its native
/// configuration, after applying scopes, overrides, plugins, and visibility rules. Skills reserved
/// for explicit user invocation remain available through `initialize.commands` instead.
#[derive(Deserialize)]
pub(super) struct NativeSkillList {
    pub(super) skills: Vec<NativeSkill>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NativeSkill {
    pub(super) name: String,
    pub(super) description: String,
    #[serde(default)]
    pub(super) argument_hint: String,
}

/// The account the CLI reports at the init handshake. Every field is optional on the wire and each
/// says something different about where the credentials come from, so the probe reads the shape as
/// a whole rather than any one field.
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NativeAccount {
    /// The signed-in user, present only for an Anthropic login.
    #[serde(default)]
    pub(super) email: Option<String>,
    /// Where an API key in use came from, such as `ANTHROPIC_API_KEY`.
    #[serde(default)]
    pub(super) api_key_source: Option<String>,
    /// Where a bearer token in use came from; `none` when there is no token.
    #[serde(default)]
    pub(super) token_source: Option<String>,
    /// Which API backend the CLI is configured against: `firstParty` for an Anthropic login, and a
    /// third-party cloud — Bedrock, Vertex, an enterprise gateway — for credentials held entirely
    /// outside the CLI.
    #[serde(default)]
    pub(super) api_provider: Option<String>,
}

/// What `list_models` answers with: the rows the CLI's own model picker offers.
#[derive(Deserialize)]
pub(super) struct NativeModelList {
    pub(super) models: Vec<NativeModel>,
}

/// One row of the CLI's model picker. `value` is what a spawn's model flag accepts — an alias such
/// as `sonnet` or `opus[1m]` — so it is the Model ID Suru presents; the row's canonical
/// `resolvedModel` is not decoded because Suru presents rows verbatim rather than collapsing
/// aliases.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct NativeModel {
    pub(super) value: String,
    pub(super) display_name: String,
    pub(super) description: String,
    #[serde(default)]
    pub(super) supported_effort_levels: Vec<String>,
}
