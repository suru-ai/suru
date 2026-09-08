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
//! `task_started` for an agent task opens the Subagent under the spawning tool use's identity,
//! `task_progress` and `task_updated` revise what it is doing, and `task_notification` settles it.
//! A `result` Settles the Turn as completed, interrupted, or failed — except where a steer's own
//! result is still to come, since the CLI answers every message queued into a running loop with a
//! result while Suru keeps them all inside the Turn the steer joined. The result speaks only for
//! the loop's own conversation: a subagent's streams live past it, which is what lets a Subagent
//! outlive the Turn (ADR 0015), and the stretch the loop later runs to deliver its outcome ends
//! with a result of its own. Every block kind this slice does not present is passed over rather
//! than failed, because the wire grows freely (ADR 0010).

use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
};

use futures_util::stream;
use serde_json::Value;
use tokio::sync::mpsc;

use super::super::shell_wrapper::strip_launcher_wrapper;
use super::{
    claude_error,
    thinking::{ThinkingEvent, ThinkingSplitter},
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
    ProviderSubagentStatus, ReportedTurnMetering,
};

/// The tool whose executions are Command Activity. Claude sends the command itself as the tool's
/// `command` input, so stripping applies only if recognizable launcher plumbing ever appears.
const COMMAND_TOOL: &str = "Bash";

/// The tool that spawns a subagent. Its tool-use id is what the CLI names as a task's
/// `tool_use_id` and what the subagent's every chunk rides under as `parent_tool_use_id`.
const TASK_TOOL: &str = "Task";

/// The task type the CLI reports for a task running an agent — a Subagent. Every other type
/// (`local_bash` above all) is background work with no conversation of its own, already in the
/// Transcript as the Command Activity that spawned it.
const SUBAGENT_TASK_TYPE: &str = "local_agent";

/// How a task's `task_notification` reports it finishing well; anything else — failed, stopped —
/// settles the Subagent as failed.
const TASK_COMPLETED_STATUS: &str = "completed";

pub(super) fn provider_events(
    messages: mpsc::UnboundedReceiver<Result<Value, ProviderError>>,
    turn: Arc<TurnInFlight>,
    questionnaires: Arc<super::questionnaire::ClaudeQuestionnaires>,
    context: Arc<super::context::ContextQueries>,
    reports: mpsc::UnboundedReceiver<AttributedProviderEvent>,
) -> ProviderEventStream {
    Box::pin(stream::unfold(
        EventReceiver {
            context,
            reports,
            questionnaires,
            messages,
            projection: ClaudeProjection::new(turn),
            pending: VecDeque::new(),
        },
        next_provider_event,
    ))
}

struct EventReceiver {
    context: Arc<super::context::ContextQueries>,
    reports: mpsc::UnboundedReceiver<AttributedProviderEvent>,
    questionnaires: Arc<super::questionnaire::ClaudeQuestionnaires>,
    messages: mpsc::UnboundedReceiver<Result<Value, ProviderError>>,
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
                return Some((Err(error), events));
            }
            Ok(message) => {
                events.context.observe(&message);
                match events
                    .questionnaires
                    .receive(
                        &message,
                        events.projection.questionnaire_attribution(&message),
                    )
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
                                    if event.attribution == ProviderEventAttribution::OwningSession
                                    {
                                        events.context.request();
                                        events
                                            .projection
                                            .question_tools
                                            .retain(|_, owner| owner.is_some());
                                    }
                                }
                                ProviderEvent::SubagentCompleted { subagent_id, .. } => {
                                    events.questionnaires.settle(
                                        &ProviderEventAttribution::Subagent(subagent_id.clone()),
                                    )
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

/// The attribution one conversation's events land under: the owning Session for the loop's own,
/// and the Subagent the spawning tool use names for everything streamed on its behalf.
fn attributed(owner: &ConversationKey, event: ProviderEvent) -> AttributedProviderEvent {
    AttributedProviderEvent {
        attribution: match owner {
            None => ProviderEventAttribution::OwningSession,
            Some(subagent) => {
                ProviderEventAttribution::Subagent(ProviderSubagentId::new(subagent.clone()))
            }
        },
        event,
    }
}

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
}

/// One task the CLI is running an agent for: the identity its Subagent's events are attributed
/// by, and the description its row currently reads, kept so progress ticks repeating it
/// unchanged publish nothing.
struct SubagentTask {
    subagent: String,
    description: String,
}

/// What the projection remembers between conversation messages, across every conversation the
/// wire carries at once.
struct ClaudeProjection {
    question_tools: BTreeMap<String, ConversationKey>,
    conversations: BTreeMap<ConversationKey, ConversationInFlight>,
    /// The commands whose tool results are still to be echoed back, by tool-use id — the CLI's
    /// ids are unique across conversations, so one table serves them all.
    running_commands: BTreeMap<String, RunningCommand>,
    /// The Task tool uses that have streamed, by tool-use id, each remembering the conversation
    /// that ran it. A `task_started` naming one of them is a spawn out of that conversation —
    /// which is how a subagent's own spawns recurse one level down.
    spawn_tools: BTreeMap<String, ConversationKey>,
    /// The agent tasks running as Subagents, by the task id the rest of the lifecycle names.
    subagent_tasks: BTreeMap<String, SubagentTask>,
    reasoning_blocks: u64,
    turn_metering: Option<ReportedTurnMetering>,
    /// What the Session reads back out of the conversation: whether the Turn it started is still
    /// running, and the background work it must stop before interrupting.
    turn: Arc<TurnInFlight>,
}

impl ClaudeProjection {
    /// can_use_tool identifies the tool call, while its preceding native
    /// assistant block names the spawning tool use through parent_tool_use_id.
    /// agent_id alone is not interchangeable with that spawning tool identity.
    fn questionnaire_attribution(&mut self, message: &Value) -> Option<ProviderEventAttribution> {
        let request = &message["request"];
        if let Some(tool) = request["tool_use_id"].as_str()
            && let Some(owner) = self.question_tools.remove(tool)
        {
            return Some(match owner {
                None => ProviderEventAttribution::OwningSession,
                Some(owner) => ProviderEventAttribution::Subagent(ProviderSubagentId::new(owner)),
            });
        }
        request["agent_id"]
            .is_null()
            .then_some(ProviderEventAttribution::OwningSession)
    }

    fn new(turn: Arc<TurnInFlight>) -> Self {
        Self {
            question_tools: BTreeMap::new(),
            conversations: BTreeMap::new(),
            running_commands: BTreeMap::new(),
            spawn_tools: BTreeMap::new(),
            subagent_tasks: BTreeMap::new(),
            reasoning_blocks: 0,
            turn_metering: None,
            turn,
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
    /// running an agent is more: a Subagent, opened in the conversation that spawned it and
    /// revised and settled under its own identity.
    fn project_task_lifecycle(&mut self, message: Value) -> Vec<AttributedProviderEvent> {
        let Ok(message) = serde_json::from_value::<SystemMessage>(message) else {
            return Vec::new();
        };
        match message.subtype.as_str() {
            "task_started" => self.project_task_started(message),
            "task_progress" => self.project_task_description(message.task_id, message.description),
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
        if message.task_type.as_deref() != Some(SUBAGENT_TASK_TYPE)
            || self.subagent_tasks.contains_key(&task_id)
        {
            return Vec::new();
        }
        // The spawning tool use's id is the identity the subagent's every chunk rides under. A
        // start that names none leaves the task id standing in, so the Subagent's row and settle
        // still reach the Transcript even though no chunk can ever be attributed to it.
        let subagent = message.tool_use_id.unwrap_or_else(|| task_id.clone());
        self.turn
            .subagent_task_started(task_id.clone(), subagent.clone());
        let spawner = self
            .spawn_tools
            .remove(&subagent)
            .unwrap_or(OWNING_CONVERSATION);
        let description = message.description.unwrap_or_default();
        let name = message
            .subagent_type
            .unwrap_or_else(|| TASK_TOOL.to_owned());
        self.subagent_tasks.insert(
            task_id,
            SubagentTask {
                subagent: subagent.clone(),
                description: description.clone(),
            },
        );
        // The spawn alone is attributed to the spawning conversation, because it is what decides
        // which Session the child hangs under. The rest of the lifecycle addresses the row by
        // the Subagent's own identity and rides the owning conversation, so a nested Subagent's
        // settle still lands after its spawner's own — order the wire does not promise.
        vec![attributed(
            &spawner,
            ProviderEvent::SubagentStarted {
                subagent_id: ProviderSubagentId::new(subagent),
                name,
                description,
            },
        )]
    }

    /// A revised description for a running Subagent. Tasks that are not Subagents, tasks never
    /// started, and ticks repeating the description unchanged all publish nothing.
    fn project_task_description(
        &mut self,
        task_id: Option<String>,
        description: Option<String>,
    ) -> Vec<AttributedProviderEvent> {
        let Some((task_id, description)) = task_id.zip(description) else {
            return Vec::new();
        };
        let Some(task) = self.subagent_tasks.get_mut(&task_id) else {
            return Vec::new();
        };
        if task.description == description {
            return Vec::new();
        }
        task.description = description.clone();
        vec![attributed(
            &OWNING_CONVERSATION,
            ProviderEvent::SubagentUpdated {
                subagent_id: ProviderSubagentId::new(task.subagent.clone()),
                description,
            },
        )]
    }

    fn project_task_settled(&mut self, message: SystemMessage) -> Vec<AttributedProviderEvent> {
        let Some(task_id) = message.task_id else {
            return Vec::new();
        };
        self.turn.task_settled(&task_id);
        let Some(task) = self.subagent_tasks.remove(&task_id) else {
            return Vec::new();
        };
        // The settle is the last of the Subagent's events: whatever its streams leave open, the
        // settle closes in the child Session, and nothing more of the conversation's can land.
        self.conversations.remove(&Some(task.subagent.clone()));
        self.question_tools
            .retain(|_, owner| owner.as_deref() != Some(task.subagent.as_str()));
        self.running_commands
            .retain(|_, command| command.owner.as_deref() != Some(task.subagent.as_str()));
        let status = if message
            .status
            .as_deref()
            .is_none_or(|status| status == TASK_COMPLETED_STATUS)
        {
            ProviderSubagentStatus::Completed
        } else {
            ProviderSubagentStatus::Failed
        };
        vec![attributed(
            &OWNING_CONVERSATION,
            ProviderEvent::SubagentCompleted {
                subagent_id: ProviderSubagentId::new(task.subagent),
                status,
            },
        )]
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
        let owner = message.parent_tool_use_id;
        let event = message.event;
        // The conversation steps out of the table while its chunk projects, so the projection's
        // shared state — the commands and spawns other conversations feed too — stays reachable.
        let mut conversation = self.conversations.remove(&owner).unwrap_or_default();
        conversation.streamed = true;
        let mut projected = Vec::new();
        match event.kind.as_str() {
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
        Ok(projected
            .into_iter()
            .map(|event| attributed(&owner, event))
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
        projected
            .into_iter()
            .map(|event| attributed(&owner, event))
            .collect()
    }

    /// Starts tracking a `tool_use` block whose input is about to stream. A Task tool use is
    /// remembered as a spawn, so the task the CLI starts for it opens its Subagent in the
    /// conversation that ran the tool.
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
        if name == "AskUserQuestion" {
            self.question_tools.insert(id.clone(), owner.clone());
        }
        if name == TASK_TOOL {
            self.spawn_tools.insert(id.clone(), owner.clone());
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
    /// the conversation that ran it, recording the bare command. Any other tool, and input in no
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
        if tool.name != COMMAND_TOOL {
            return;
        }
        let streamed = serde_json::from_str::<Value>(&tool.streamed_input).ok();
        let input = streamed.or(tool.opening_input).unwrap_or(Value::Null);
        let Some(command) = input.get("command").and_then(Value::as_str) else {
            return;
        };
        let activity_id = ProviderActivityId::new(format!("command:{}", tool.id));
        projected.push(ProviderEvent::CommandStarted {
            activity_id: activity_id.clone(),
            command: strip_launcher_wrapper(command.to_owned()),
            cwd: None,
        });
        self.running_commands.insert(
            tool.id,
            RunningCommand {
                owner: owner.clone(),
                activity: activity_id,
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
                projected.push(attributed(
                    &command.owner,
                    ProviderEvent::CommandOutputDelta {
                        activity_id: command.activity.clone(),
                        content: output,
                    },
                ));
            }
            projected.push(attributed(
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
        if let Some(usage) = result.usage.as_ref() {
            let usage = Usage {
                fresh_input_tokens: reported_token_count(usage.input_tokens),
                cache_read_tokens: reported_token_count(usage.cache_read_input_tokens),
                cache_write_tokens: reported_token_count(usage.cache_creation_input_tokens),
                output_tokens: reported_token_count(usage.output_tokens),
                reasoning_tokens: None,
                native_meter: None,
                model_context_window: None,
            };
            let reported_cost = result.total_cost_usd.and_then(Cost::from_usd);
            if let Some(metering) = self.turn_metering.as_mut() {
                metering.add(usage, reported_cost);
            } else {
                self.turn_metering = Some(ReportedTurnMetering::new(usage, reported_cost));
            }
            let metering = self
                .turn_metering
                .as_ref()
                .expect("Claude Turn metering was just initialized");
            projected.push(metering.event());
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
        Ok(projected
            .into_iter()
            .map(|event| attributed(&OWNING_CONVERSATION, event))
            .collect())
    }
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
