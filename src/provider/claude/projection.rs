//! Projection of a Claude Session's conversation messages into attributed Provider events.
//!
//! The CLI streams a Turn as partial-message chunks — Anthropic streaming events riding in
//! `stream_event` envelopes — and ends each stretch of its loop with one `result` message. The
//! wire carries several conversations at once: the loop's own, and one for every subagent the
//! agent spawns through the Task tool, attributed to the spawning tool use's id as
//! `parent_tool_use_id`. Only the loop's own conversation streams; a subagent's arrives solely as
//! the full `assistant` snapshots that restate the loop's chunks after the fact, so each
//! conversation is presented from whichever account is all it gets. Both project the same way —
//! text blocks become the agent Message, thinking blocks become Reasoning Activity split at their
//! headings, the Bash tool's executions become Command Activity settled by the tool results the
//! loop echoes back — and each event leaves here attributed to the conversation that produced it,
//! so orchestration lands a subagent's work in the Subagent's own child Session rather than the
//! parent's Transcript.
//!
//! The task lifecycle the CLI reports beside the conversations is where Subagents begin and end:
//! `task_started` for an agent task opens the Subagent — known by its task id — in the
//! conversation whose tool use spawned it, `task_updated` revises what it is doing, and
//! `task_notification` settles its stretch of work. A background shell or monitor is instead a
//! Watch (ADR 0030): its start and its notification bracket the time its Session may read
//! Monitoring, and every Watch still live when the CLI process ends settles as lost. A settled
//! agent the loop resumes through
//! SendMessage starts the same task again, naming the SendMessage tool use: that is a resume of the
//! Subagent rather than a new one, and since the resumed conversation still rides under the
//! original spawn's id, the resumed work lands in the Subagent's own Session, in the Turn the
//! resume begins there (ADR 0031). What the delegating tool handed the agent — the Agent tool's
//! `prompt`, SendMessage's `message` — is the Delegation that opens that Turn. The Resume State
//! records the spawn each agent's conversation rides under, so an agent spawned before a restart
//! resumes the same way once the conversation carries on; one resumed with no record claims the
//! first conversation nothing else has, provided it is the only such agent working.
//! A `result` Settles the Turn as completed, interrupted, or failed — except where a steer's own
//! result is still to come, since the CLI answers every message queued into a running loop with a
//! result while Suru keeps them all inside the Turn the steer joined. The result speaks only for
//! the loop's own conversation: a subagent's streams live past it, which is what lets a Subagent
//! outlive the Turn (ADR 0015), and the stretch the loop later runs to deliver its outcome ends
//! with a result of its own. A fresh owning message after that boundary explicitly begins a
//! native Continuation, including when a background Bash command woke the loop with no Subagent
//! involved; its result and interrupt belong to that Continuation. Every block kind this slice
//! does not present is passed over rather than failed, because the wire grows freely (ADR 0010).

use std::{
    collections::{BTreeMap, BTreeSet, HashSet, VecDeque},
    sync::Arc,
};

use futures_util::stream;
use serde_json::Value;
use tokio::sync::mpsc;

use super::super::shell_wrapper::strip_launcher_wrapper;
use super::{
    claude_error,
    session::ClaudeResumeState,
    thinking::{ThinkingEvent, ThinkingSplitter},
    transport::ConversationItem,
    turn_in_flight::TurnInFlight,
    wire::{
        AssistantMessageSnapshot, ContentBlock, EchoedUserContent, EchoedUserMessage,
        ResultMessage, StreamEventMessage, SystemMessage,
    },
};
use crate::protocol::{Cost, Usage};
use crate::provider::{
    AttributedProviderEvent, ProviderActivityId, ProviderCommandStatus, ProviderError,
    ProviderEvent, ProviderEventAttribution, ProviderEventStream, ProviderSubagentId,
    ProviderSubagentStatus, ProviderWatchId, ProviderWatchOutcome, ReportedTurnMetering,
};

/// The tool whose executions are Command Activity. Claude sends the command itself as the tool's
/// `command` input, so stripping applies only if recognizable launcher plumbing ever appears.
const COMMAND_TOOL: &str = "Bash";

/// The tool that spawns a subagent. Its tool-use id is what the CLI names as a task's
/// `tool_use_id` and what the subagent's every chunk rides under as `parent_tool_use_id`.
const TASK_TOOL: &str = "Task";
const AGENT_TOOL: &str = "Agent";

/// The tool that resumes a settled background agent. The CLI starts the agent's task again naming
/// this tool use as its `tool_use_id`, so the resume opens in the conversation that ran it, like a
/// spawn, and the tool's input says what the resume asks.
const SEND_MESSAGE_TOOL: &str = "SendMessage";

/// What one task the CLI reports is to Suru. Every task joins the roster an interrupt stops, but
/// only a Subagent has a conversation of its own, and only a Watch may wake the loop once it has
/// ended its Turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TaskKind {
    /// A task running an agent, opened as a Subagent.
    Subagent,
    /// Background work whose settling the CLI delivers to the agent, waking its loop into a
    /// Continuation. Its Command Activity, where it has one, already stands in the Transcript.
    Watch,
    /// Work that neither runs an agent Suru presents nor wakes this loop.
    Unwatched,
}

impl TaskKind {
    /// The one table classifying the CLI's `task_type`s (ADR 0030). A background shell and a
    /// Monitor tool both run as `local_bash`, and a monitor over MCP or a WebSocket is a Watch
    /// too: each one's settling is delivered to the agent. `local_workflow`, `remote_agent`,
    /// `in_process_teammate`, `dream`, plan-mode tasks, and any type this build has not heard of
    /// are none of Suru's to wait on, since the wire grows freely (ADR 0010) and a task wrongly
    /// read as a Watch would leave a Session Monitoring for nothing.
    fn of(task_type: Option<&str>) -> Self {
        match task_type {
            Some("local_agent") => Self::Subagent,
            Some("local_bash" | "monitor_mcp" | "monitor_ws") => Self::Watch,
            _ => Self::Unwatched,
        }
    }
}

/// How a task's `task_notification` reports it finishing well; anything else — failed, stopped —
/// settles the Subagent as failed.
const TASK_COMPLETED_STATUS: &str = "completed";

/// How a task's `task_notification` reports it stopped rather than finishing or failing.
const TASK_STOPPED_STATUS: &str = "stopped";

pub(super) fn provider_events(
    messages: mpsc::UnboundedReceiver<Result<ConversationItem, ProviderError>>,
    projection: ClaudeProjection,
    questionnaires: Arc<super::questionnaire::ClaudeQuestionnaires>,
    approvals: Arc<super::approval::ClaudeApprovals>,
    execution_directory: std::path::PathBuf,
    context: Arc<super::context::ContextQueries>,
    reports: mpsc::UnboundedReceiver<AttributedProviderEvent>,
) -> ProviderEventStream {
    Box::pin(stream::unfold(
        EventReceiver {
            context,
            reports,
            questionnaires,
            approvals,
            execution_directory,
            messages,
            projection,
            pending: VecDeque::new(),
        },
        next_provider_event,
    ))
}

struct EventReceiver {
    context: Arc<super::context::ContextQueries>,
    reports: mpsc::UnboundedReceiver<AttributedProviderEvent>,
    questionnaires: Arc<super::questionnaire::ClaudeQuestionnaires>,
    approvals: Arc<super::approval::ClaudeApprovals>,
    execution_directory: std::path::PathBuf,
    messages: mpsc::UnboundedReceiver<Result<ConversationItem, ProviderError>>,
    projection: ClaudeProjection,
    pending: VecDeque<Result<AttributedProviderEvent, ProviderError>>,
}

async fn next_provider_event(
    mut events: EventReceiver,
) -> Option<(
    Result<AttributedProviderEvent, ProviderError>,
    EventReceiver,
)> {
    loop {
        if let Some(event) = events.pending.pop_front() {
            return Some((event, events));
        }
        let message = tokio::select! {
            message = events.messages.recv() => message?,
            Some(report) = events.reports.recv() => {
                if let Some(report) = events.context.route_report(report) {
                    return Some((Ok(report), events));
                }
                continue;
            },
        };
        match message {
            Err(error) => {
                events.questionnaires.clear();
                events.approvals.clear();
                return Some((Err(error), events));
            }
            Ok(ConversationItem::ProcessEnded) => {
                let settled = events.projection.project_process_ended();
                events.pending.extend(settled.into_iter().map(Ok));
            }
            Ok(ConversationItem::Message(message)) => {
                events.context.observe(&message);
                let attribution = events.projection.intervention_attribution(&message);
                match events
                    .questionnaires
                    .receive(&message, attribution.clone())
                    .await
                {
                    Ok(Some(projected)) => {
                        events
                            .context
                            .observe_output(&projected, events.projection.turn.is_running());
                        events.pending.extend(projected.into_iter().map(Ok));
                        continue;
                    }
                    Err(error) => {
                        events.pending.push_back(Err(error));
                        continue;
                    }
                    Ok(None) => {}
                }
                match events
                    .approvals
                    .receive(&message, attribution, &events.execution_directory)
                    .await
                {
                    Ok(Some(projected)) => {
                        events
                            .context
                            .observe_output(&projected, events.projection.turn.is_running());
                        events.pending.extend(projected.into_iter().map(Ok));
                        continue;
                    }
                    Err(error) => {
                        events.pending.push_back(Err(error));
                        continue;
                    }
                    Ok(None) => {}
                }
                let prompt_running = events.projection.turn.is_running();
                match events.projection.project(message) {
                    Ok(projected) => {
                        events.context.observe_output(&projected, prompt_running);
                        for event in &projected {
                            match &event.event {
                                ProviderEvent::TurnCompleted
                                | ProviderEvent::TurnInterrupted
                                | ProviderEvent::TurnFailed { .. } => {
                                    events.questionnaires.settle(&event.attribution);
                                    events.approvals.settle(&event.attribution);
                                    if event.attribution == ProviderEventAttribution::OwningSession
                                    {
                                        events.context.request();
                                        events
                                            .projection
                                            .intervention_tools
                                            .retain(|_, owner| owner.is_some());
                                    }
                                }
                                ProviderEvent::SubagentCompleted { subagent_id, .. } => {
                                    events.questionnaires.settle(
                                        &ProviderEventAttribution::Subagent(subagent_id.clone()),
                                    );
                                    events.approvals.settle(&ProviderEventAttribution::Subagent(
                                        subagent_id.clone(),
                                    ));
                                }
                                _ => {}
                            }
                        }
                        events.pending.extend(projected.into_iter().map(Ok));
                    }
                    Err(error) => events.pending.push_back(Err(error)),
                }
            }
        }
    }
}

/// Which conversation on the wire produced a message: the loop's own, or the subagent's whose
/// spawning tool use `parent_tool_use_id` names.
type ConversationKey = Option<String>;

/// The loop's own conversation, whose events land in the owning Session.
const OWNING_CONVERSATION: ConversationKey = None;

/// A `tool_use` block between its start and stop: the input streams in `input_json_delta`
/// increments beside whatever the start already carried.
struct OpenToolUse {
    id: String,
    name: String,
    streamed_input: String,
    opening_input: Option<Value>,
}

/// The thinking block a conversation has open, and the Reasoning block its split is currently
/// filling.
struct OpenThinking {
    index: u64,
    activity: ProviderActivityId,
    splitter: ThinkingSplitter,
}

/// What the projection remembers about one conversation between its chunks: the streaming text
/// block that is its agent Message currently open, the thinking block feeding its Reasoning
/// Activity, and the tool-use blocks still streaming their input, keyed by the block index each
/// numbers within its own conversation.
#[derive(Default)]
struct ConversationInFlight {
    open_text_block: Option<u64>,
    open_thinking: Option<OpenThinking>,
    open_tools: BTreeMap<u64, OpenToolUse>,
    /// Whether any chunk has streamed for this conversation. A conversation that streams is
    /// presented from its chunks, so its full-message snapshots restate what already projected
    /// and are passed over; one that never streams is presented from the snapshots alone.
    streamed: bool,
}

/// A command running until a tool result settles it, remembering the conversation that ran it —
/// which is where its output and settle land.
struct RunningCommand {
    owner: ConversationKey,
    activity: ProviderActivityId,
    /// The command as its Activity reads it, which describes a background task it starts that
    /// the CLI gives no description of its own.
    command: String,
}

/// A tool use that delegates to an agent, remembered from its block until the task it delegates
/// starts: the conversation that ran it, whose Turn the Delegation's row stands in, and which kind
/// of Delegation it is. Its input is read once the block closes, since it streams.
struct DelegationTool {
    delegator: ConversationKey,
    kind: DelegationKind,
}

enum DelegationKind {
    /// The Agent or Task tool, spawning a new agent. The task's start describes it, and the tool's
    /// `prompt` is the Delegation's text.
    Spawn { prompt: Option<String> },
    /// SendMessage, resuming an agent that settled. Its input describes the resume — the loop's
    /// `summary` of the message, or else the message's own first line — and its `message` is the
    /// Delegation's text.
    Resume {
        description: Option<String>,
        message: Option<String>,
    },
}

impl DelegationKind {
    /// Reads what the tool's completed input says of the Delegation. Input in no shape this reads
    /// leaves the Delegation to be described by the task's start instead.
    fn read_input(&mut self, input: &Value) {
        match self {
            Self::Spawn { prompt } => *prompt = input_text(input, "prompt"),
            Self::Resume {
                description,
                message,
            } => {
                *description = send_message_description(input);
                *message = input_text(input, "message");
            }
        }
    }

    /// The Delegation's text, as the tool's input gave it.
    fn text(&mut self) -> Option<String> {
        match self {
            Self::Spawn { prompt } => prompt.take(),
            Self::Resume { message, .. } => message.take(),
        }
    }
}

/// One agent task the CLI has run as a Subagent. Its task id is the Subagent's identity, and it
/// is remembered for as long as the wire lasts, because a settled agent may be resumed.
struct AgentTask {
    /// The `parent_tool_use_id` its conversation rides under: the spawning tool use's id, which a
    /// resume does not change. `None` where no chunk can be attributed to it.
    conversation: Option<String>,
    /// While the agent works, the description the row of its current stretch reads, kept so
    /// updates repeating it unchanged publish nothing; `None` once that stretch has settled.
    working: Option<String>,
}

/// What the projection remembers between conversation messages, across every conversation the
/// wire carries at once.
pub(super) struct ClaudeProjection {
    intervention_tools: BTreeMap<String, ConversationKey>,
    conversations: BTreeMap<ConversationKey, ConversationInFlight>,
    /// The commands whose tool results are still to be echoed back, by tool-use id — the CLI's
    /// ids are unique across conversations, so one table serves them all.
    running_commands: BTreeMap<String, RunningCommand>,
    /// The delegating tool uses that have streamed, by tool-use id. A `task_started` naming one of
    /// them is a Delegation out of the conversation that ran it — which is how a subagent's own
    /// spawns recurse one level down, and how a sibling's resume lands in the sibling's Turn.
    delegation_tools: BTreeMap<String, DelegationTool>,
    /// Every agent task this wire has started, working or settled, by task id — the identity the
    /// rest of the lifecycle and a resume name it by.
    agent_tasks: BTreeMap<String, AgentTask>,
    /// The Watches running in the current CLI process, by task id, against the conversation
    /// whose agent a Watch's settling wakes. They settle on their own notification, or all at
    /// once as lost when the process ends.
    watches: BTreeMap<String, ConversationKey>,
    /// The subagent conversations, from the `parent_tool_use_id` each rides under to the task id
    /// of the agent it belongs to — whose Subagent its events are, in whichever stretch.
    conversation_agents: BTreeMap<String, String>,
    /// The agents working in a resume whose conversation this wire does not know, by task id —
    /// agents spawned before a restart the Resume State never recorded — until one claims the
    /// conversation its chunks turn out to ride under.
    unplaced_resumes: BTreeSet<String>,
    /// What the Session must remember to continue after a restart, kept current as agents spawn so
    /// orchestration can store each revision.
    resume: ClaudeResumeState,
    /// Latest assistant-snapshot Model evidence by conversation, since the stretch it began in.
    /// A snapshot can race the task lifecycle, so evidence waits here until the stretch's row
    /// exists, and a settle clears it so a resume reports only its own.
    subagent_models: BTreeMap<String, crate::protocol::ModelId>,
    reporting_lifetime: String,
    latest_reported_cost: Option<Cost>,
    seen_results: HashSet<String>,
    reasoning_blocks: u64,
    turn_metering: Option<ReportedTurnMetering>,
    /// What the Session reads back out of the conversation: whether the Turn it started is still
    /// running, and the background work it must stop before interrupting.
    turn: Arc<TurnInFlight>,
}

impl ClaudeProjection {
    /// can_use_tool identifies the tool call, while its preceding native
    /// assistant block names the spawning tool use through parent_tool_use_id,
    /// which resolves to the Subagent the conversation belongs to. agent_id
    /// alone is not interchangeable with that spawning tool identity.
    fn intervention_attribution(&self, message: &Value) -> Option<ProviderEventAttribution> {
        let request = &message["request"];
        if let Some(tool) = request["tool_use_id"].as_str()
            && let Some(owner) = self.intervention_tools.get(tool)
        {
            return Some(self.attribution(owner));
        }
        request["agent_id"]
            .is_null()
            .then_some(ProviderEventAttribution::OwningSession)
    }

    /// A projection for a Session whose conversation `resume` restores: every agent it records
    /// spawning is a settled Subagent a resume may name, riding under the conversation recorded
    /// for it.
    pub(super) fn new(turn: Arc<TurnInFlight>, resume: ClaudeResumeState) -> Self {
        let agent_tasks = resume
            .agents
            .iter()
            .map(|(task_id, conversation)| {
                (
                    task_id.clone(),
                    AgentTask {
                        conversation: Some(conversation.clone()),
                        working: None,
                    },
                )
            })
            .collect();
        let conversation_agents = resume
            .agents
            .iter()
            .map(|(task_id, conversation)| (conversation.clone(), task_id.clone()))
            .collect();
        Self {
            intervention_tools: BTreeMap::new(),
            conversations: BTreeMap::new(),
            running_commands: BTreeMap::new(),
            delegation_tools: BTreeMap::new(),
            agent_tasks,
            watches: BTreeMap::new(),
            conversation_agents,
            unplaced_resumes: BTreeSet::new(),
            resume,
            subagent_models: BTreeMap::new(),
            reporting_lifetime: uuid::Uuid::new_v4().to_string(),
            latest_reported_cost: None,
            seen_results: HashSet::new(),
            reasoning_blocks: 0,
            turn_metering: None,
            turn,
        }
    }

    /// The attribution one conversation's events land under: the owning Session for the loop's
    /// own, and for a subagent's the Subagent its agent task is. A conversation whose task has not
    /// started yet names no Subagent orchestration holds, so what it carries lands nowhere.
    fn attribution(&self, owner: &ConversationKey) -> ProviderEventAttribution {
        match owner {
            None => ProviderEventAttribution::OwningSession,
            Some(conversation) => ProviderEventAttribution::Subagent(ProviderSubagentId::new(
                self.conversation_agents
                    .get(conversation)
                    .unwrap_or(conversation)
                    .clone(),
            )),
        }
    }

    fn attributed(&self, owner: &ConversationKey, event: ProviderEvent) -> AttributedProviderEvent {
        AttributedProviderEvent {
            attribution: self.attribution(owner),
            event,
        }
    }

    fn project(&mut self, message: Value) -> Result<Vec<AttributedProviderEvent>, ProviderError> {
        match message.get("type").and_then(Value::as_str) {
            Some("stream_event") => self.project_stream_event(message),
            Some("assistant") => Ok(self.project_assistant_snapshot(message)),
            Some("user") => Ok(self.project_tool_results(message)),
            Some("result") => self.project_result(message),
            Some("system") => Ok(self.project_task_lifecycle(message)),
            // Everything else the CLI says about itself — nothing this projection presents.
            _ => Ok(Vec::new()),
        }
    }

    /// The task lifecycle the CLI reports beside the conversations. Every task joins the roster
    /// of background work an interrupt stops before it stops the loop — kept from the tasks' own
    /// start and settle rather than from the roster snapshot the CLI also sends, because that
    /// snapshot covers only work already in the background, and a subagent still running in the
    /// foreground of the Turn is exactly what an interrupt alone would leave behind. A task
    /// running an agent is more: a Subagent, opened in the conversation that spawned it, resumed
    /// from whichever conversation sent it more, and revised and settled under its task id.
    fn project_task_lifecycle(&mut self, message: Value) -> Vec<AttributedProviderEvent> {
        let Ok(message) = serde_json::from_value::<SystemMessage>(message) else {
            return Vec::new();
        };
        match message.subtype.as_str() {
            "task_started" => self.project_task_started(message),
            // A progress tick's description is the subagent's latest tool activity ("Running
            // cargo test"), not what it was asked to do, so it revises nothing.
            "task_progress" => Vec::new(),
            "task_updated" => self.project_task_description(
                message.task_id,
                message.patch.and_then(|patch| patch.description),
            ),
            // However a task ends — finished, failed, or stopped — the CLI notifies, so the
            // notification alone is enough to settle it.
            "task_notification" => self.project_task_settled(message),
            _ => Vec::new(),
        }
    }

    fn project_task_started(&mut self, message: SystemMessage) -> Vec<AttributedProviderEvent> {
        let Some(task_id) = message.task_id else {
            return Vec::new();
        };
        self.turn.task_started(task_id.clone());
        match TaskKind::of(message.task_type.as_deref()) {
            TaskKind::Subagent => {}
            TaskKind::Watch => {
                return self.project_watch_started(
                    task_id,
                    message.description,
                    message.tool_use_id.as_deref(),
                );
            }
            TaskKind::Unwatched => return Vec::new(),
        }
        // The roster is the running process's, so an agent task joins it on every start — even
        // one whose row is still working, which is what an agent the previous process was running
        // looks like when the process that replaced it resumes the same task.
        self.turn.subagent_task_started(task_id.clone());
        if self
            .agent_tasks
            .get(&task_id)
            .is_some_and(|task| task.working.is_some())
        {
            return Vec::new();
        }
        // The tool use the start names is the Delegation, and the conversation that ran it is the
        // delegating one. A start naming no tool this projection saw delegates from the loop's own
        // conversation, which always has a Turn to land in (ADR 0015).
        let delegation = message
            .tool_use_id
            .as_ref()
            .and_then(|tool| self.delegation_tools.remove(tool));
        let (delegator, mut kind) = delegation.map_or(
            (OWNING_CONVERSATION, DelegationKind::Spawn { prompt: None }),
            |tool| (tool.delegator, tool.kind),
        );
        // What the agent was handed is the delegating tool's own input, or where that never
        // streamed, the text the start carries.
        let delegation = kind.text().or(message.prompt);
        let subagent_id = ProviderSubagentId::new(task_id.clone());
        let name = message
            .subagent_type
            .unwrap_or_else(|| TASK_TOOL.to_owned());
        let mut recorded = false;
        let (conversation, event) = match (self.agent_tasks.get_mut(&task_id), kind) {
            // A task this wire started before, or one the Resume State recorded spawning before a
            // restart, is a settled agent resumed: the same Subagent, continuing the same
            // conversation. The start repeats the spawn's description, so the resume reads what the
            // SendMessage asked instead.
            (Some(task), kind) => {
                let description = resume_description(kind)
                    .or(message.description)
                    .unwrap_or_default();
                task.working = Some(description.clone());
                if task.conversation.is_none() {
                    self.unplaced_resumes.insert(task_id.clone());
                }
                (
                    task.conversation.clone(),
                    ProviderEvent::SubagentResumed {
                        subagent_id,
                        name,
                        description,
                        delegation,
                    },
                )
            }
            // A resume of an agent this wire never saw start, and no Resume State recorded, is
            // still a resume: orchestration finds the Session the agent's identity was stored
            // with, or records it as a new Subagent where none was. Its conversation rides under a
            // spawn this wire never saw either, so it is claimed from the chunks once they come.
            (None, kind @ DelegationKind::Resume { .. }) => {
                let description = resume_description(kind)
                    .or(message.description)
                    .unwrap_or_default();
                self.agent_tasks.insert(
                    task_id.clone(),
                    AgentTask {
                        conversation: None,
                        working: Some(description.clone()),
                    },
                );
                self.unplaced_resumes.insert(task_id.clone());
                (
                    None,
                    ProviderEvent::SubagentResumed {
                        subagent_id,
                        name,
                        description,
                        delegation,
                    },
                )
            }
            (None, DelegationKind::Spawn { .. }) => {
                // A spawn's tool use is the identity the subagent's every chunk rides under, and
                // the Resume State records it so a resume after a restart still routes them. A
                // start that names no tool use leaves nothing to attribute chunks by, though the
                // Subagent's row and settle still reach the Transcript.
                let conversation = message.tool_use_id;
                if let Some(conversation) = &conversation {
                    self.conversation_agents
                        .insert(conversation.clone(), task_id.clone());
                    self.resume
                        .agents
                        .insert(task_id.clone(), conversation.clone());
                    recorded = true;
                }
                let description = message.description.unwrap_or_default();
                self.agent_tasks.insert(
                    task_id.clone(),
                    AgentTask {
                        conversation: conversation.clone(),
                        working: Some(description.clone()),
                    },
                );
                (
                    conversation,
                    ProviderEvent::SubagentStarted {
                        subagent_id,
                        name,
                        description,
                        delegation,
                    },
                )
            }
        };
        // The start alone is attributed to the delegating conversation, because it is what decides
        // which Session the row stands in. The rest of the lifecycle addresses the row by the
        // Subagent's own identity and rides the owning conversation, so a nested Subagent's settle
        // still lands after its spawner's own — order the wire does not promise.
        let mut projected = vec![self.attributed(&delegator, event)];
        if let Some(model) = conversation
            .and_then(|conversation| self.subagent_models.get(&conversation))
            .cloned()
        {
            projected.push(
                ProviderEvent::SubagentModelChanged {
                    subagent_id: ProviderSubagentId::new(task_id),
                    model,
                }
                .into(),
            );
        }
        if recorded {
            projected.push(self.resume_state_changed());
        }
        projected
    }

    /// A background task that may wake the loop once its Turn has ended: a Watch, described by
    /// what the task's start says it does, or else by the command that started it. A start
    /// repeating a Watch already live announces nothing new.
    ///
    /// Every Watch is attributed to the loop's own conversation: which subagent, if any, left a
    /// task running is not yet read off the wire, so its settling is taken to wake the owning
    /// Session's agent. The owner is kept with the Watch so its settle lands where its start did.
    fn project_watch_started(
        &mut self,
        task_id: String,
        description: Option<String>,
        tool_use_id: Option<&str>,
    ) -> Vec<AttributedProviderEvent> {
        if self.watches.contains_key(&task_id) {
            return Vec::new();
        }
        let description = description
            .filter(|description| !description.trim().is_empty())
            .or_else(|| {
                tool_use_id
                    .and_then(|tool| self.running_commands.get(tool))
                    .map(|command| command.command.clone())
            })
            .unwrap_or_else(|| task_id.clone());
        let owner = OWNING_CONVERSATION;
        let event = ProviderEvent::WatchStarted {
            watch_id: ProviderWatchId::new(task_id.clone()),
            description,
        };
        let started = self.attributed(&owner, event);
        self.watches.insert(task_id, owner);
        vec![started]
    }

    /// A Watch's own notification that it settled, which the CLI delivers to the agent whose
    /// loop it wakes; the notification's summary is how the Watch settled in the CLI's words.
    fn project_watch_settled(
        &mut self,
        task_id: &str,
        message: &SystemMessage,
    ) -> Option<AttributedProviderEvent> {
        let owner = self.watches.remove(task_id)?;
        let outcome = match message.status.as_deref() {
            None | Some(TASK_COMPLETED_STATUS) => ProviderWatchOutcome::Completed,
            Some(TASK_STOPPED_STATUS) => ProviderWatchOutcome::Stopped,
            Some(_) => ProviderWatchOutcome::Failed,
        };
        let event = ProviderEvent::WatchSettled {
            watch_id: ProviderWatchId::new(task_id),
            outcome,
            summary: message
                .summary
                .clone()
                .filter(|summary| !summary.trim().is_empty()),
            woke_agent: true,
        };
        Some(self.attributed(&owner, event))
    }

    /// The CLI process this projection was reading has ended — stopped by the Session, or replaced
    /// by one spawned under another Agent Selection — and everything it wrote before it did has
    /// projected already. Every task on its roster died with it, so each live Watch settles as
    /// lost and wakes nothing, and the roster starts empty again for whatever the next process
    /// reports: output the old process left queued can no longer put its tasks back.
    fn project_process_ended(&mut self) -> Vec<AttributedProviderEvent> {
        self.turn.tasks_died_with_process();
        std::mem::take(&mut self.watches)
            .into_iter()
            .map(|(task_id, owner)| {
                self.attributed(
                    &owner,
                    ProviderEvent::WatchSettled {
                        watch_id: ProviderWatchId::new(task_id),
                        outcome: ProviderWatchOutcome::Lost,
                        summary: None,
                        woke_agent: false,
                    },
                )
            })
            .collect()
    }

    /// The Resume State as it stands now, for orchestration to store in place of what the
    /// Session's startup reported.
    fn resume_state_changed(&self) -> AttributedProviderEvent {
        ProviderEvent::ResumeStateChanged {
            resume_state: self.resume.to_provider(),
        }
        .into()
    }

    /// Claims a subagent conversation nothing on this wire has attributed for the one agent resumed
    /// without a conversation of its own — an agent spawned before a restart that the Resume State
    /// never recorded, whose conversation rides under a spawn this wire never saw. Only a lone such
    /// agent can claim it: with two working, nothing on the wire tells whose a chunk is, so neither
    /// claims and their chunks land nowhere, as an unknown conversation's always have. A claimed
    /// conversation is recorded like a spawned one, so the next restart need not claim it again.
    fn claim_conversation(&mut self, owner: &ConversationKey) -> Option<AttributedProviderEvent> {
        let conversation = owner.as_ref()?;
        if self.unplaced_resumes.len() != 1
            || self.conversation_agents.contains_key(conversation)
            || self.delegation_tools.contains_key(conversation)
        {
            return None;
        }
        let task_id = self.unplaced_resumes.pop_first()?;
        self.conversation_agents
            .insert(conversation.clone(), task_id.clone());
        if let Some(task) = self.agent_tasks.get_mut(&task_id) {
            task.conversation = Some(conversation.clone());
        }
        self.resume.agents.insert(task_id, conversation.clone());
        Some(self.resume_state_changed())
    }

    /// A revised description for a running Subagent. Tasks that are not Subagents, tasks never
    /// started, and updates repeating the description unchanged all publish nothing.
    fn project_task_description(
        &mut self,
        task_id: Option<String>,
        description: Option<String>,
    ) -> Vec<AttributedProviderEvent> {
        let Some((task_id, description)) = task_id.zip(description) else {
            return Vec::new();
        };
        let Some(working) = self
            .agent_tasks
            .get_mut(&task_id)
            .and_then(|task| task.working.as_mut())
        else {
            return Vec::new();
        };
        if *working == description {
            return Vec::new();
        }
        working.clone_from(&description);
        vec![
            ProviderEvent::SubagentUpdated {
                subagent_id: ProviderSubagentId::new(task_id),
                description,
            }
            .into(),
        ]
    }

    fn project_task_settled(&mut self, message: SystemMessage) -> Vec<AttributedProviderEvent> {
        let Some(task_id) = message.task_id.clone() else {
            return Vec::new();
        };
        self.turn.task_settled(&task_id);
        if let Some(settled) = self.project_watch_settled(&task_id, &message) {
            return vec![settled];
        }
        let Some(task) = self
            .agent_tasks
            .get_mut(&task_id)
            .filter(|task| task.working.is_some())
        else {
            return Vec::new();
        };
        task.working = None;
        self.unplaced_resumes.remove(&task_id);
        // The settle is the last of this stretch's events: whatever its streams leave open, the
        // settle closes in the child Session, and nothing more of the conversation's lands until a
        // resume begins the next stretch.
        if let Some(conversation) = task.conversation.clone() {
            self.conversations.remove(&Some(conversation.clone()));
            self.intervention_tools
                .retain(|_, owner| owner.as_deref() != Some(conversation.as_str()));
            self.running_commands
                .retain(|_, command| command.owner.as_deref() != Some(conversation.as_str()));
            self.subagent_models.remove(&conversation);
        }
        let status = if message
            .status
            .as_deref()
            .is_none_or(|status| status == TASK_COMPLETED_STATUS)
        {
            ProviderSubagentStatus::Completed
        } else {
            ProviderSubagentStatus::Failed
        };
        vec![
            ProviderEvent::SubagentCompleted {
                subagent_id: ProviderSubagentId::new(task_id),
                status,
            }
            .into(),
        ]
    }

    fn project_stream_event(
        &mut self,
        message: Value,
    ) -> Result<Vec<AttributedProviderEvent>, ProviderError> {
        let message: StreamEventMessage = serde_json::from_value(message).map_err(|error| {
            claude_error(format!(
                "Claude Code CLI sent a malformed stream event: {error}"
            ))
        })?;
        let owner: ConversationKey = message.parent_tool_use_id;
        let claimed = self.claim_conversation(&owner);
        let event = message.event;
        // The conversation steps out of the table while its chunk projects, so the projection's
        // shared state — the commands and spawns other conversations feed too — stays reachable.
        let mut conversation = self.conversations.remove(&owner).unwrap_or_default();
        conversation.streamed = true;
        let mut projected = Vec::new();
        match event.kind.as_str() {
            // Only a new owning message can begin a native loop; child messages and trailing
            // block stops cannot revive a settled Turn.
            "message_start" if owner.is_none() => {
                if let Some(selection) = self.turn.begin_continuation() {
                    projected.push(ProviderEvent::ContinuationStarted { selection });
                }
            }
            "content_block_start" => {
                if let Some(block) = event.content_block {
                    // Blocks stream strictly one at a time within a conversation, so a start
                    // while its text or thinking block is open means a stop was lost; settle
                    // what is open rather than interleaving two.
                    if conversation.open_text_block.take().is_some() {
                        projected.push(ProviderEvent::AgentMessageCompleted);
                    }
                    self.settle_open_thinking(&mut conversation, &mut projected);
                    match block.kind.as_str() {
                        "text" => {
                            conversation.open_text_block = event.index;
                            projected.push(ProviderEvent::AgentMessageStarted);
                            if let Some(text) = block.text.filter(|text| !text.is_empty()) {
                                projected.push(ProviderEvent::AgentMessageDelta { content: text });
                            }
                        }
                        "thinking" => {
                            if let Some(index) = event.index {
                                self.open_thinking(
                                    &mut conversation,
                                    index,
                                    block.thinking,
                                    &mut projected,
                                );
                            }
                        }
                        "tool_use" => {
                            self.open_tool_use(&owner, &mut conversation, event.index, block)
                        }
                        _ => {}
                    }
                }
            }
            "content_block_delta" => {
                if let Some((index, delta)) = event.index.zip(event.delta) {
                    if conversation.open_text_block == Some(index) && delta.kind == "text_delta" {
                        projected.push(ProviderEvent::AgentMessageDelta {
                            content: delta.text.unwrap_or_default(),
                        });
                    } else if let Some(thinking) = conversation
                        .open_thinking
                        .as_mut()
                        .filter(|open| open.index == index && delta.kind == "thinking_delta")
                    {
                        let split = thinking.splitter.push(&delta.thinking.unwrap_or_default());
                        Self::project_thinking_split(
                            &mut self.reasoning_blocks,
                            thinking,
                            split,
                            &mut projected,
                        );
                    } else if delta.kind == "input_json_delta"
                        && let Some(tool) = conversation.open_tools.get_mut(&index)
                    {
                        tool.streamed_input
                            .push_str(&delta.partial_json.unwrap_or_default());
                    }
                }
            }
            "content_block_stop" => {
                if let Some(index) = event.index {
                    if conversation.open_text_block == Some(index) {
                        conversation.open_text_block = None;
                        projected.push(ProviderEvent::AgentMessageCompleted);
                    } else if conversation
                        .open_thinking
                        .as_ref()
                        .is_some_and(|open| open.index == index)
                    {
                        self.settle_open_thinking(&mut conversation, &mut projected);
                    } else {
                        self.close_tool_use(&owner, &mut conversation, index, &mut projected);
                    }
                }
            }
            // Message boundaries carry nothing the Transcript presents.
            _ => {}
        }
        self.conversations.insert(owner.clone(), conversation);
        let attribution = self.attribution(&owner);
        Ok(claimed
            .into_iter()
            .chain(projected.into_iter().map(|event| AttributedProviderEvent {
                attribution: attribution.clone(),
                event,
            }))
            .collect())
    }

    /// A full-message snapshot of an assistant message. A conversation that streamed is already
    /// in the Transcript chunk by chunk, so its snapshots are passed over. A subagent's
    /// conversation never streams — verified against 2.1.237, which attributes no `stream_event`
    /// to a parent tool use — so its snapshots are all the wire carries of it, and each block
    /// projects as a settled whole: text as the agent Message, thinking as Reasoning split at its
    /// headings, and tool uses through the same open/close pair the streaming path takes, which
    /// is what records a Task block as a spawn and a Bash block as a Command awaiting its result.
    fn project_assistant_snapshot(&mut self, message: Value) -> Vec<AttributedProviderEvent> {
        let Ok(message) = serde_json::from_value::<AssistantMessageSnapshot>(message) else {
            return Vec::new();
        };
        let owner: ConversationKey = message.parent_tool_use_id;
        let claimed = self.claim_conversation(&owner);
        let observed_model = owner.as_ref().and_then(|conversation| {
            message
                .message
                .model
                .filter(|model| !model.is_empty())
                .map(|model| (conversation.clone(), crate::protocol::ModelId::new(model)))
        });
        if let Some((conversation, model)) = observed_model.as_ref() {
            self.subagent_models
                .insert(conversation.clone(), model.clone());
        }
        let held = self.conversations.remove(&owner);
        let known = held.is_some();
        let mut conversation = held.unwrap_or_default();
        let mut projected = Vec::new();
        if !conversation.streamed {
            for (index, block) in message.message.content.into_iter().enumerate() {
                let index = index as u64;
                match block.kind.as_str() {
                    "text" => {
                        if let Some(text) = block.text.filter(|text| !text.is_empty()) {
                            projected.push(ProviderEvent::AgentMessageStarted);
                            projected.push(ProviderEvent::AgentMessageDelta { content: text });
                            projected.push(ProviderEvent::AgentMessageCompleted);
                        }
                    }
                    "thinking" => {
                        if block
                            .thinking
                            .as_deref()
                            .is_some_and(|text| !text.is_empty())
                        {
                            self.open_thinking(
                                &mut conversation,
                                index,
                                block.thinking,
                                &mut projected,
                            );
                            self.settle_open_thinking(&mut conversation, &mut projected);
                        }
                    }
                    "tool_use" => {
                        self.open_tool_use(&owner, &mut conversation, Some(index), block);
                        self.close_tool_use(&owner, &mut conversation, index, &mut projected);
                    }
                    _ => {}
                }
            }
        }
        // A snapshot's blocks arrive settled, so they leave nothing open behind: a conversation
        // the table did not already hold has nothing to remember, and reinserting one would
        // recreate entries for subagents whose settle already cleared them.
        if known {
            self.conversations.insert(owner.clone(), conversation);
        }
        // Evidence reaches the row of the stretch the agent is working in; while none is, it waits
        // for the start that opens one.
        let model_evidence = observed_model.and_then(|(conversation, model)| {
            let task_id = self.conversation_agents.get(&conversation)?;
            self.agent_tasks.get(task_id)?.working.as_ref()?;
            Some(
                ProviderEvent::SubagentModelChanged {
                    subagent_id: ProviderSubagentId::new(task_id.clone()),
                    model,
                }
                .into(),
            )
        });
        let mut attributed_events = claimed
            .into_iter()
            .chain(model_evidence)
            .collect::<Vec<_>>();
        let attribution = self.attribution(&owner);
        attributed_events.extend(projected.into_iter().map(|event| AttributedProviderEvent {
            attribution: attribution.clone(),
            event,
        }));
        attributed_events
    }

    /// Starts tracking a `tool_use` block whose input is about to stream. An Agent or Task tool use
    /// is remembered as a spawn and a SendMessage tool use as a resume, so the task the CLI starts
    /// for either opens its row in the conversation that ran the tool.
    fn open_tool_use(
        &mut self,
        owner: &ConversationKey,
        conversation: &mut ConversationInFlight,
        index: Option<u64>,
        block: ContentBlock,
    ) {
        let (Some(index), Some(id), Some(name)) = (index, block.id, block.name) else {
            return;
        };
        self.intervention_tools.insert(id.clone(), owner.clone());
        let kind = match name.as_str() {
            TASK_TOOL | AGENT_TOOL => Some(DelegationKind::Spawn { prompt: None }),
            SEND_MESSAGE_TOOL => Some(DelegationKind::Resume {
                description: None,
                message: None,
            }),
            _ => None,
        };
        if let Some(kind) = kind {
            self.delegation_tools.insert(
                id.clone(),
                DelegationTool {
                    delegator: owner.clone(),
                    kind,
                },
            );
        }
        conversation.open_tools.insert(
            index,
            OpenToolUse {
                id,
                name,
                streamed_input: String::new(),
                opening_input: block.input,
            },
        );
    }

    /// Closes a `tool_use` block: a completed Bash tool use becomes a running Command Activity in
    /// the conversation that ran it, recording the bare command, and a completed delegating tool
    /// use leaves what it asks for the spawn or resume it starts. Any other tool, and input in no
    /// shape this projection reads, is passed over.
    fn close_tool_use(
        &mut self,
        owner: &ConversationKey,
        conversation: &mut ConversationInFlight,
        index: u64,
        projected: &mut Vec<ProviderEvent>,
    ) {
        let Some(tool) = conversation.open_tools.remove(&index) else {
            return;
        };
        let delegation = self.delegation_tools.get_mut(&tool.id);
        if tool.name != COMMAND_TOOL && delegation.is_none() {
            return;
        }
        let streamed = serde_json::from_str::<Value>(&tool.streamed_input).ok();
        let input = streamed.or(tool.opening_input).unwrap_or(Value::Null);
        if let Some(delegation) = delegation {
            delegation.kind.read_input(&input);
            return;
        }
        let Some(command) = input.get("command").and_then(Value::as_str) else {
            return;
        };
        let activity_id = ProviderActivityId::new(format!("command:{}", tool.id));
        let command = strip_launcher_wrapper(command.to_owned());
        projected.push(ProviderEvent::CommandStarted {
            activity_id: activity_id.clone(),
            command: command.clone(),
            cwd: None,
        });
        self.running_commands.insert(
            tool.id,
            RunningCommand {
                owner: owner.clone(),
                activity: activity_id,
                command,
            },
        );
    }

    /// The tool results a `user` message echoes back, settling the commands they report on in
    /// whichever conversation ran each. A user message in any other shape is not the projection's
    /// to present.
    fn project_tool_results(&mut self, message: Value) -> Vec<AttributedProviderEvent> {
        let Ok(message) = serde_json::from_value::<EchoedUserMessage>(message) else {
            return Vec::new();
        };
        let EchoedUserContent::Blocks(blocks) = message.message.content else {
            return Vec::new();
        };
        let mut projected = Vec::new();
        for block in blocks {
            if block.kind != "tool_result" {
                continue;
            }
            let Some(command) = block
                .tool_use_id
                .and_then(|id| self.running_commands.remove(&id))
            else {
                continue;
            };
            let output = tool_result_text(&block.content);
            if !output.is_empty() {
                projected.push(self.attributed(
                    &command.owner,
                    ProviderEvent::CommandOutputDelta {
                        activity_id: command.activity.clone(),
                        content: output,
                    },
                ));
            }
            projected.push(self.attributed(
                &command.owner,
                ProviderEvent::CommandCompleted {
                    activity_id: command.activity,
                    status: if block.is_error {
                        ProviderCommandStatus::Failed
                    } else {
                        ProviderCommandStatus::Completed
                    },
                    exit_status: None,
                },
            ));
        }
        projected
    }

    /// Opens a conversation's thinking block and the first Reasoning block of its split.
    fn open_thinking(
        &mut self,
        conversation: &mut ConversationInFlight,
        index: u64,
        opening: Option<String>,
        projected: &mut Vec<ProviderEvent>,
    ) {
        let activity = next_reasoning_activity(&mut self.reasoning_blocks);
        projected.push(ProviderEvent::ReasoningStarted {
            activity_id: activity.clone(),
        });
        let mut thinking = OpenThinking {
            index,
            activity,
            splitter: ThinkingSplitter::default(),
        };
        if let Some(opening) = opening.filter(|opening| !opening.is_empty()) {
            let split = thinking.splitter.push(&opening);
            Self::project_thinking_split(
                &mut self.reasoning_blocks,
                &mut thinking,
                split,
                projected,
            );
        }
        conversation.open_thinking = Some(thinking);
    }

    /// Settles a conversation's open thinking block, releasing whatever its split still withholds.
    fn settle_open_thinking(
        &mut self,
        conversation: &mut ConversationInFlight,
        projected: &mut Vec<ProviderEvent>,
    ) {
        let Some(mut thinking) = conversation.open_thinking.take() else {
            return;
        };
        let split = thinking.splitter.finish();
        Self::project_thinking_split(&mut self.reasoning_blocks, &mut thinking, split, projected);
        projected.push(ProviderEvent::ReasoningCompleted {
            activity_id: thinking.activity,
        });
    }

    /// Lowers a thinking split's resolutions onto the Reasoning block it is filling: a heading
    /// break settles the block and starts the next one under the heading's title.
    fn project_thinking_split(
        reasoning_blocks: &mut u64,
        thinking: &mut OpenThinking,
        split: Vec<ThinkingEvent>,
        projected: &mut Vec<ProviderEvent>,
    ) {
        for event in split {
            match event {
                ThinkingEvent::Title(title) => {
                    projected.push(ProviderEvent::ReasoningTitleChanged {
                        activity_id: thinking.activity.clone(),
                        title,
                    })
                }
                ThinkingEvent::Content(content) => projected.push(ProviderEvent::ReasoningDelta {
                    activity_id: thinking.activity.clone(),
                    content,
                }),
                ThinkingEvent::Break { title } => {
                    projected.push(ProviderEvent::ReasoningCompleted {
                        activity_id: thinking.activity.clone(),
                    });
                    thinking.activity = next_reasoning_activity(reasoning_blocks);
                    projected.push(ProviderEvent::ReasoningStarted {
                        activity_id: thinking.activity.clone(),
                    });
                    projected.push(ProviderEvent::ReasoningTitleChanged {
                        activity_id: thinking.activity.clone(),
                        title,
                    });
                }
            }
        }
    }

    fn project_result(
        &mut self,
        message: Value,
    ) -> Result<Vec<AttributedProviderEvent>, ProviderError> {
        let result: ResultMessage = serde_json::from_value(message).map_err(|error| {
            claude_error(format!(
                "Claude Code CLI sent a malformed result message: {error}"
            ))
        })?;
        if result
            .uuid
            .as_ref()
            .is_some_and(|uuid| !self.seen_results.insert(uuid.clone()))
        {
            return Ok(Vec::new());
        }
        let mut projected = Vec::new();
        // A result is a boundary of the loop's own conversation alone: its subagents stream on
        // past it (ADR 0015). A result while the loop's blocks are still streaming is the CLI
        // failing mid-stream; what did stream stays in the Transcript, settled.
        let mut owning = self
            .conversations
            .remove(&OWNING_CONVERSATION)
            .unwrap_or_default();
        self.settle_open_thinking(&mut owning, &mut projected);
        if owning.open_text_block.take().is_some() {
            projected.push(ProviderEvent::AgentMessageCompleted);
        }
        // A command of the loop's own whose tool result never came back has no outcome to match,
        // so it settles as failed rather than holding the Turn open. A subagent's commands are
        // owed nothing by this result and keep running.
        let unanswered = self
            .running_commands
            .iter()
            .filter(|(_, command)| command.owner.is_none())
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in unanswered {
            let command = self
                .running_commands
                .remove(&id)
                .expect("an unanswered command was just listed from the table");
            projected.push(ProviderEvent::CommandCompleted {
                activity_id: command.activity,
                status: ProviderCommandStatus::Failed,
                exit_status: None,
            });
        }
        // The result ends a stretch, not the wire's account of the loop: that the loop's own
        // conversation streams is what keeps its snapshots passed over, so the flag outlives the
        // blocks the settle just closed.
        self.conversations.insert(
            OWNING_CONVERSATION,
            ConversationInFlight {
                streamed: owning.streamed,
                ..ConversationInFlight::default()
            },
        );
        let reported_cost = result
            .total_cost_usd
            .and_then(Cost::from_usd)
            .filter(|cost| {
                self.latest_reported_cost
                    .is_none_or(|latest| cost.nano_usd() >= latest.nano_usd())
            });
        if let Some(cost) = reported_cost {
            self.latest_reported_cost = Some(cost);
        }
        if result.usage.is_some() || reported_cost.is_some() || self.turn_metering.is_some() {
            let usage = result
                .usage
                .as_ref()
                .map_or_else(Usage::default, |usage| Usage {
                    fresh_input_tokens: reported_token_count(usage.input_tokens),
                    cache_read_tokens: reported_token_count(usage.cache_read_input_tokens),
                    cache_write_tokens: reported_token_count(usage.cache_creation_input_tokens),
                    output_tokens: reported_token_count(usage.output_tokens),
                    reasoning_tokens: None,
                    native_meter: None,
                    model_context_window: None,
                });
            if let Some(metering) = self.turn_metering.as_mut() {
                metering.add_usage_with_cumulative_cost(usage, reported_cost);
            } else {
                self.turn_metering = Some(ReportedTurnMetering::new(usage, reported_cost));
            }
            let metering = self
                .turn_metering
                .as_ref()
                .expect("Claude Turn metering was just initialized");
            projected.push(metering.subtree_event(&self.reporting_lifetime));
        }
        let turn_settled;
        if was_interrupted(&result) {
            self.turn.abandon_turn();
            projected.push(ProviderEvent::TurnInterrupted);
            turn_settled = true;
        } else if result.subtype == "success" && !result.is_error {
            // A steered Turn is answered stretch by stretch: the CLI ends every user message
            // queued into the loop with a result of its own, and only the last one Settles the
            // Turn that holds them all. A result no Turn waited on at all ends a stretch the
            // loop ran on its own — waking to deliver a Subagent's outcome — and completing it
            // is what settles the Continuation that output began, or nothing where none is open.
            if self.turn.result_settles_turn() || !self.turn.is_running() {
                projected.push(ProviderEvent::TurnCompleted);
                turn_settled = true;
            } else {
                turn_settled = false;
            }
        } else {
            self.turn.abandon_turn();
            projected.push(ProviderEvent::TurnFailed {
                message: super::result_failure_message("Turn", &result),
            });
            turn_settled = true;
        }
        if turn_settled {
            self.turn_metering = None;
        }
        Ok(projected.into_iter().map(Into::into).collect())
    }
}

/// What a resume Delegation asks, as its row reads it. Only a SendMessage carries a description of
/// its own; a start naming anything else is described by the task itself.
fn resume_description(kind: DelegationKind) -> Option<String> {
    match kind {
        DelegationKind::Resume { description, .. } => description,
        DelegationKind::Spawn { .. } => None,
    }
}

/// One text field of a delegating tool's input, where it holds text with something to read. A
/// field in any other shape — SendMessage's structured protocol messages among them — holds no
/// Delegation text.
fn input_text(input: &Value, field: &str) -> Option<String> {
    input
        .get(field)
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
}

/// How a SendMessage's input describes the resume it starts: the `summary` the loop gave of its
/// message, or where it gave none, the message's own first line. A message in some shape other
/// than text describes nothing.
fn send_message_description(input: &Value) -> Option<String> {
    let text = |field: &str| input.get(field).and_then(Value::as_str);
    text("summary")
        .map(str::trim)
        .filter(|summary| !summary.is_empty())
        .or_else(|| {
            text("message")?
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
        })
        .map(str::to_owned)
}

/// Reads an integer token count without letting a malformed negative,
/// fractional, non-finite, or unrepresentable Provider number become a
/// fabricated zero.
fn reported_token_count(value: Option<f64>) -> Option<u64> {
    value
        .filter(|value| {
            value.is_finite() && *value >= 0.0 && value.fract() == 0.0 && *value <= u64::MAX as f64
        })
        .map(|value| value as u64)
}

/// Whether a result is a Turn the user stopped. The CLI reports an interrupted loop as an unerrored
/// `success` carrying no answer, so the abort is legible only in `terminal_reason` — verified
/// against 2.1.237, whose interrupted result reads `aborted_streaming`. Which of the two abort
/// reasons the CLI gives says only where its loop was when the interrupt landed: streaming an
/// answer, or waiting on a tool it had already called.
fn was_interrupted(result: &ResultMessage) -> bool {
    matches!(
        result.terminal_reason.as_deref(),
        Some("aborted_streaming" | "aborted_tools")
    )
}

/// The next Reasoning block's identity. Blocks are numbered across the Session — one namespace
/// for every conversation, since each block lands in a Session of its own anyway — apart from the
/// tool-use ids commands are named by.
fn next_reasoning_activity(reasoning_blocks: &mut u64) -> ProviderActivityId {
    let activity = ProviderActivityId::new(format!("reasoning:{reasoning_blocks}"));
    *reasoning_blocks += 1;
    activity
}

/// The text of a tool result, in either shape the wire carries one: a bare string, or a list of
/// blocks whose text entries are the output.
fn tool_result_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{ClaudeProjection, ClaudeResumeState, TurnInFlight, send_message_description};
    use crate::provider::{
        AttributedProviderEvent, ProviderEvent, ProviderEventAttribution, ProviderSubagentId,
        ProviderWatchId, ProviderWatchOutcome,
    };

    /// The loop's own conversation running SendMessage `tool` to the agent `task`, and the CLI
    /// starting the agent's task again for it.
    fn send_message_resuming(tool: &str, task: &str) -> [Value; 3] {
        [
            json!({
                "type": "stream_event",
                "event": {
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "tool_use", "id": tool, "name": "SendMessage", "input": {}},
                },
                "parent_tool_use_id": null,
            }),
            json!({
                "type": "stream_event",
                "event": {"type": "content_block_stop", "index": 0},
                "parent_tool_use_id": null,
            }),
            json!({
                "type": "system",
                "subtype": "task_started",
                "task_id": task,
                "tool_use_id": tool,
                "description": "The spawn's description",
                "task_type": "local_agent",
                "subagent_type": "general-purpose",
            }),
        ]
    }

    fn said_under(conversation: &str, text: &str) -> Value {
        json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [{"type": "text", "text": text}]},
            "parent_tool_use_id": conversation,
        })
    }

    fn project(
        projection: &mut ClaudeProjection,
        messages: &[Value],
    ) -> Vec<AttributedProviderEvent> {
        messages
            .iter()
            .flat_map(|message| {
                projection
                    .project(message.clone())
                    .expect("the message projects")
            })
            .collect()
    }

    /// Where the agent Message `text` lands: under the Subagent the attribution names.
    fn attribution_of(events: &[AttributedProviderEvent], text: &str) -> ProviderEventAttribution {
        events
            .iter()
            .find(|event| {
                matches!(&event.event, ProviderEvent::AgentMessageDelta { content } if content == text)
            })
            .unwrap_or_else(|| panic!("{text} projects as an agent Message: {events:?}"))
            .attribution
            .clone()
    }

    fn subagent(task: &str) -> ProviderEventAttribution {
        ProviderEventAttribution::Subagent(ProviderSubagentId::new(task))
    }

    fn task_started(task: &str, task_type: &str, description: Option<&str>) -> Value {
        json!({
            "type": "system",
            "subtype": "task_started",
            "task_id": task,
            "task_type": task_type,
            "description": description,
        })
    }

    fn task_notification(task: &str, status: &str, summary: Option<&str>) -> Value {
        json!({
            "type": "system",
            "subtype": "task_notification",
            "task_id": task,
            "status": status,
            "summary": summary,
        })
    }

    fn fresh_projection() -> ClaudeProjection {
        ClaudeProjection::new(TurnInFlight::new(), ClaudeResumeState::default())
    }

    fn owning(event: ProviderEvent) -> AttributedProviderEvent {
        event.into()
    }

    #[test]
    fn a_background_shell_or_monitor_starts_a_watch_and_nothing_else_does() {
        for (task_type, watched) in [
            ("local_bash", true),
            ("monitor_mcp", true),
            ("monitor_ws", true),
            ("local_workflow", false),
            ("remote_agent", false),
            ("in_process_teammate", false),
            ("dream", false),
            ("a_task_type_this_build_has_never_heard_of", false),
        ] {
            let mut projection = fresh_projection();
            let events = project(
                &mut projection,
                &[task_started("task-1", task_type, Some("Background work"))],
            );
            let expected = if watched {
                vec![owning(ProviderEvent::WatchStarted {
                    watch_id: ProviderWatchId::new("task-1"),
                    description: "Background work".to_owned(),
                })]
            } else {
                Vec::new()
            };
            assert_eq!(events, expected, "a `{task_type}` task");
            assert_eq!(
                projection.turn.live_tasks(),
                ["task-1"],
                "a `{task_type}` task still joins the roster an interrupt stops"
            );
        }
    }

    #[test]
    fn a_watch_the_cli_gives_no_description_is_described_by_the_command_that_started_it() {
        let mut projection = fresh_projection();
        let events = project(
            &mut projection,
            &[
                json!({
                    "type": "stream_event",
                    "event": {
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": {
                            "type": "tool_use",
                            "id": "toolu_1",
                            "name": "Bash",
                            "input": {"command": "cargo test", "run_in_background": true},
                        },
                    },
                    "parent_tool_use_id": null,
                }),
                json!({
                    "type": "stream_event",
                    "event": {"type": "content_block_stop", "index": 0},
                    "parent_tool_use_id": null,
                }),
                json!({
                    "type": "system",
                    "subtype": "task_started",
                    "task_id": "task-1",
                    "tool_use_id": "toolu_1",
                    "task_type": "local_bash",
                }),
                task_started("task-2", "local_bash", None),
            ],
        );
        let described = events
            .iter()
            .filter_map(|event| match &event.event {
                ProviderEvent::WatchStarted { description, .. } => Some(description.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            described,
            ["cargo test", "task-2"],
            "a Watch with nothing else to go on is named by its task"
        );
    }

    #[test]
    fn a_watchs_notification_settles_it_with_the_outcome_and_summary_the_cli_gives() {
        for (status, outcome) in [
            ("completed", ProviderWatchOutcome::Completed),
            ("failed", ProviderWatchOutcome::Failed),
            ("stopped", ProviderWatchOutcome::Stopped),
        ] {
            let mut projection = fresh_projection();
            let events = project(
                &mut projection,
                &[
                    task_started("task-1", "local_bash", Some("cargo test")),
                    task_notification("task-1", status, Some("cargo test finished")),
                    task_notification("task-1", status, Some("a repeated notification")),
                ],
            );
            assert_eq!(
                events[1..],
                [owning(ProviderEvent::WatchSettled {
                    watch_id: ProviderWatchId::new("task-1"),
                    outcome,
                    summary: Some("cargo test finished".to_owned()),
                    woke_agent: true,
                })],
                "a `{status}` notification settles the Watch once, waking the loop it belongs to"
            );
            assert!(projection.turn.live_tasks().is_empty());
        }
    }

    #[test]
    fn the_watches_a_process_was_running_settle_as_lost_when_it_ends() {
        let mut projection = fresh_projection();
        project(
            &mut projection,
            &[
                task_started("task-1", "local_bash", Some("cargo test")),
                task_started("task-2", "monitor_mcp", Some("Watch the queue")),
                task_notification("task-2", "completed", None),
            ],
        );

        let lost = projection.project_process_ended();

        assert_eq!(
            lost,
            [owning(ProviderEvent::WatchSettled {
                watch_id: ProviderWatchId::new("task-1"),
                outcome: ProviderWatchOutcome::Lost,
                summary: None,
                woke_agent: false,
            })],
            "only the Watch still live is lost, and its loss wakes nothing"
        );
        assert!(
            projection.turn.live_tasks().is_empty(),
            "the roster starts empty again for the next process"
        );
        assert!(
            projection.project_process_ended().is_empty(),
            "a Watch is lost once"
        );
    }

    #[test]
    fn a_spawn_records_the_conversation_its_agent_rides_under_in_the_resume_state() {
        let mut projection = ClaudeProjection::new(
            TurnInFlight::new(),
            ClaudeResumeState {
                session_id: "provider-session".to_owned(),
                ..Default::default()
            },
        );

        let events = project(
            &mut projection,
            &[json!({
                "type": "system",
                "subtype": "task_started",
                "task_id": "a2046dbbe8ecd4a5c",
                "tool_use_id": "agent_1",
                "description": "Say hello",
                "task_type": "local_agent",
            })],
        );

        let revised = events
            .iter()
            .find_map(|event| match &event.event {
                ProviderEvent::ResumeStateChanged { resume_state } => Some(resume_state.payload()),
                _ => None,
            })
            .expect("the spawn revises the Resume State");
        assert_eq!(
            *revised,
            json!({
                "session_id": "provider-session",
                "agents": {"a2046dbbe8ecd4a5c": "agent_1"},
            })
        );
    }

    #[test]
    fn an_agent_the_resume_state_recorded_resumes_with_its_conversation_routed() {
        let mut projection = ClaudeProjection::new(
            TurnInFlight::new(),
            ClaudeResumeState {
                session_id: "provider-session".to_owned(),
                agents: [("a2046dbbe8ecd4a5c".to_owned(), "agent_1".to_owned())].into(),
            },
        );

        let mut messages = send_message_resuming("send_1", "a2046dbbe8ecd4a5c").to_vec();
        messages.push(said_under("agent_1", "GOODBYE"));
        let events = project(&mut projection, &messages);

        assert!(
            events.iter().any(|event| matches!(
                &event.event,
                ProviderEvent::SubagentResumed { subagent_id, .. }
                    if subagent_id.as_str() == "a2046dbbe8ecd4a5c"
            )),
            "the start resumes the recorded agent: {events:?}"
        );
        assert_eq!(
            attribution_of(&events, "GOODBYE"),
            subagent("a2046dbbe8ecd4a5c")
        );
    }

    #[test]
    fn a_lone_agent_resumed_with_no_record_claims_the_conversation_nothing_else_has() {
        let mut projection =
            ClaudeProjection::new(TurnInFlight::new(), ClaudeResumeState::default());

        let mut messages = send_message_resuming("send_1", "a0ld").to_vec();
        messages.push(said_under("agent_before", "Picking up."));
        let events = project(&mut projection, &messages);

        assert_eq!(attribution_of(&events, "Picking up."), subagent("a0ld"));
        assert!(
            events.iter().any(|event| matches!(
                &event.event,
                ProviderEvent::ResumeStateChanged { resume_state }
                    if resume_state.payload()["agents"]["a0ld"] == "agent_before"
            )),
            "the claim is recorded so a later restart need not claim it again: {events:?}"
        );
    }

    #[test]
    fn two_agents_resumed_with_no_record_claim_no_conversation() {
        let mut projection =
            ClaudeProjection::new(TurnInFlight::new(), ClaudeResumeState::default());

        let mut messages = send_message_resuming("send_1", "a0ld").to_vec();
        messages.extend(send_message_resuming("send_2", "b0ld"));
        messages.push(said_under("agent_before", "Whose is this?"));
        let events = project(&mut projection, &messages);

        assert!(
            ![subagent("a0ld"), subagent("b0ld")]
                .contains(&attribution_of(&events, "Whose is this?")),
            "nothing tells whose the conversation is, so neither agent claims it: {events:?}"
        );
    }

    #[test]
    fn a_send_message_is_described_by_its_summary() {
        assert_eq!(
            send_message_description(&json!({
                "to": "a2046dbbe8ecd4a5c",
                "message": "Now say GOODBYE.\nNothing else.",
                "summary": "  Say goodbye ",
            })),
            Some("Say goodbye".to_owned())
        );
    }

    #[test]
    fn a_send_message_without_a_summary_is_described_by_its_messages_first_line() {
        assert_eq!(
            send_message_description(&json!({
                "to": "a2046dbbe8ecd4a5c",
                "message": "\n  Tighten the second paragraph.\nThen stop.",
                "summary": "",
            })),
            Some("Tighten the second paragraph.".to_owned())
        );
    }

    #[test]
    fn a_send_message_carrying_no_text_describes_nothing() {
        assert_eq!(
            send_message_description(&json!({
                "to": "a2046dbbe8ecd4a5c",
                "message": {"type": "shutdown_request"},
            })),
            None
        );
        assert_eq!(send_message_description(&serde_json::Value::Null), None);
    }
}
