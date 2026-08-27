//! Projection of a Claude Session's conversation messages into Provider events.
//!
//! The CLI streams a Turn as partial-message chunks — Anthropic streaming events riding in
//! `stream_event` envelopes — and ends it with one `result` message. Text blocks the conversation
//! itself owns become the agent Message; thinking blocks become Reasoning Activity, split so each
//! heading begins a new block; the Bash tool's executions become Command Activity, settled by the
//! tool results the loop echoes back as user messages. Chunks owned by a subagent contribute only
//! their tool uses — their narration is the subagent's own conversation, not this Transcript.
//! Every block kind this slice does not present is passed over rather than failed, because the
//! wire grows freely (ADR 0010). A `result` Settles the Turn as completed, interrupted, or failed —
//! except where a steer's own result is still to come, since the CLI answers every message queued
//! into a running loop with a result while Suru keeps them all inside the Turn the steer joined.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
};

use futures_util::{StreamExt as _, stream};
use serde_json::Value;
use tokio::sync::mpsc;

use super::super::shell_wrapper::strip_launcher_wrapper;
use super::{
    claude_error,
    thinking::{ThinkingEvent, ThinkingSplitter},
    turn_in_flight::TurnInFlight,
    wire::{
        ContentBlock, EchoedUserContent, EchoedUserMessage, ResultMessage, StreamEvent,
        StreamEventMessage, SystemMessage,
    },
};
use crate::provider::{
    AttributedProviderEvent, ProviderActivityId, ProviderCommandStatus, ProviderError,
    ProviderEvent, ProviderEventStream,
};

/// The tool whose executions are Command Activity. Claude sends the command itself as the tool's
/// `command` input, so stripping applies only if recognizable launcher plumbing ever appears.
const COMMAND_TOOL: &str = "Bash";

pub(super) fn provider_events(
    messages: mpsc::UnboundedReceiver<Result<Value, ProviderError>>,
    turn: Arc<TurnInFlight>,
) -> ProviderEventStream {
    Box::pin(
        stream::unfold(
            EventReceiver {
                messages,
                projection: ClaudeProjection::new(turn),
                pending: VecDeque::new(),
            },
            next_provider_event,
        )
        .map(|event| event.map(AttributedProviderEvent::from)),
    )
}

struct EventReceiver {
    messages: mpsc::UnboundedReceiver<Result<Value, ProviderError>>,
    projection: ClaudeProjection,
    pending: VecDeque<Result<ProviderEvent, ProviderError>>,
}

async fn next_provider_event(
    mut events: EventReceiver,
) -> Option<(Result<ProviderEvent, ProviderError>, EventReceiver)> {
    loop {
        if let Some(event) = events.pending.pop_front() {
            return Some((event, events));
        }
        let message = events.messages.recv().await?;
        match message {
            Err(error) => return Some((Err(error), events)),
            Ok(message) => match events.projection.project(message) {
                Ok(projected) => events.pending.extend(projected.into_iter().map(Ok)),
                Err(error) => events.pending.push_back(Err(error)),
            },
        }
    }
}

/// A streaming `tool_use` block, keyed while open by the conversation that owns it and the block
/// index it streams under — subagents stream concurrently, each numbering its own blocks.
type ToolBlockKey = (Option<String>, u64);

/// A `tool_use` block between its start and stop: the input streams in `input_json_delta`
/// increments beside whatever the start already carried.
struct OpenToolUse {
    id: String,
    name: String,
    streamed_input: String,
    opening_input: Option<Value>,
}

/// The thinking block the conversation itself has open, and the Reasoning block its split is
/// currently filling.
struct OpenThinking {
    index: u64,
    activity: ProviderActivityId,
    splitter: ThinkingSplitter,
}

/// What the projection remembers between conversation messages: the streaming text block that is
/// the agent Message currently open, the thinking block feeding Reasoning Activity, the tool-use
/// blocks still streaming their input, and the commands running until a tool result settles them.
struct ClaudeProjection {
    open_text_block: Option<u64>,
    open_thinking: Option<OpenThinking>,
    open_tools: BTreeMap<ToolBlockKey, OpenToolUse>,
    running_commands: BTreeMap<String, ProviderActivityId>,
    reasoning_blocks: u64,
    /// What the Session reads back out of the conversation: whether the Turn it started is still
    /// running, and the background work it must stop before interrupting.
    turn: Arc<TurnInFlight>,
}

impl ClaudeProjection {
    fn new(turn: Arc<TurnInFlight>) -> Self {
        Self {
            open_text_block: None,
            open_thinking: None,
            open_tools: BTreeMap::new(),
            running_commands: BTreeMap::new(),
            reasoning_blocks: 0,
            turn,
        }
    }

    fn project(&mut self, message: Value) -> Result<Vec<ProviderEvent>, ProviderError> {
        match message.get("type").and_then(Value::as_str) {
            Some("stream_event") => self.project_stream_event(message),
            Some("user") => Ok(self.project_tool_results(message)),
            Some("result") => self.project_result(message),
            Some("system") => {
                self.project_task_lifecycle(message);
                Ok(Vec::new())
            }
            // Full-message snapshots of what already streamed, and everything else the CLI says
            // about itself — nothing this projection presents.
            _ => Ok(Vec::new()),
        }
    }

    /// The task lifecycle the CLI reports beside the conversation. None of it is Transcript
    /// material: it is the roster of background work an interrupt stops before it stops the loop,
    /// kept from the tasks' own start and settle rather than from the roster snapshot the CLI also
    /// sends, because that snapshot covers only work already in the background — a subagent still
    /// running in the foreground of the Turn is exactly what an interrupt alone would leave behind.
    fn project_task_lifecycle(&mut self, message: Value) {
        let Ok(message) = serde_json::from_value::<SystemMessage>(message) else {
            return;
        };
        let Some(task_id) = message.task_id else {
            return;
        };
        match message.subtype.as_str() {
            "task_started" => self.turn.task_started(task_id),
            // However a task ends — finished, failed, or stopped — the CLI notifies, so the
            // notification alone is enough to take it off the roster.
            "task_notification" => self.turn.task_settled(&task_id),
            _ => {}
        }
    }

    fn project_stream_event(
        &mut self,
        message: Value,
    ) -> Result<Vec<ProviderEvent>, ProviderError> {
        let message: StreamEventMessage = serde_json::from_value(message).map_err(|error| {
            claude_error(format!(
                "Claude Code CLI sent a malformed stream event: {error}"
            ))
        })?;
        // A subagent's narration is its own conversation, not this one's Transcript — but its
        // tool uses are work the Turn did, so those flow on into Activity.
        if let Some(parent) = message.parent_tool_use_id {
            return Ok(self.project_subagent_event(parent, message.event));
        }
        let event = message.event;
        let mut projected = Vec::new();
        match event.kind.as_str() {
            "content_block_start" => {
                let Some(block) = event.content_block else {
                    return Ok(Vec::new());
                };
                // Blocks stream strictly one at a time, so a start while a text or thinking
                // block is open means its stop was lost; settle what is open rather than
                // interleaving two.
                if self.open_text_block.take().is_some() {
                    projected.push(ProviderEvent::AgentMessageCompleted);
                }
                self.settle_open_thinking(&mut projected);
                match block.kind.as_str() {
                    "text" => {
                        self.open_text_block = event.index;
                        projected.push(ProviderEvent::AgentMessageStarted);
                        if let Some(text) = block.text.filter(|text| !text.is_empty()) {
                            projected.push(ProviderEvent::AgentMessageDelta { content: text });
                        }
                    }
                    "thinking" => {
                        if let Some(index) = event.index {
                            self.open_thinking(index, block.thinking, &mut projected);
                        }
                    }
                    "tool_use" => self.open_tool_use(None, event.index, block),
                    _ => {}
                }
            }
            "content_block_delta" => {
                let Some(index) = event.index else {
                    return Ok(Vec::new());
                };
                let Some(delta) = event.delta else {
                    return Ok(Vec::new());
                };
                if self.open_text_block == Some(index) && delta.kind == "text_delta" {
                    projected.push(ProviderEvent::AgentMessageDelta {
                        content: delta.text.unwrap_or_default(),
                    });
                } else if let Some(thinking) = self
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
                    && let Some(tool) = self.open_tools.get_mut(&(None, index))
                {
                    tool.streamed_input
                        .push_str(&delta.partial_json.unwrap_or_default());
                }
            }
            "content_block_stop" if event.index.is_some() => {
                if self.open_text_block == event.index {
                    self.open_text_block = None;
                    projected.push(ProviderEvent::AgentMessageCompleted);
                } else if self
                    .open_thinking
                    .as_ref()
                    .is_some_and(|open| Some(open.index) == event.index)
                {
                    self.settle_open_thinking(&mut projected);
                } else {
                    self.close_tool_use((None, event.index.unwrap_or_default()), &mut projected);
                }
            }
            // Message boundaries carry nothing the Transcript presents.
            _ => {}
        }
        Ok(projected)
    }

    /// A subagent-owned streaming event: text and thinking are dropped, tool-use blocks flow so
    /// the subagent's work still reaches the Transcript as Activity.
    fn project_subagent_event(&mut self, parent: String, event: StreamEvent) -> Vec<ProviderEvent> {
        let mut projected = Vec::new();
        match event.kind.as_str() {
            "content_block_start" => {
                if let Some(block) = event.content_block.filter(|block| block.kind == "tool_use") {
                    self.open_tool_use(Some(parent), event.index, block);
                }
            }
            "content_block_delta" => {
                if let Some((index, delta)) = event.index.zip(event.delta)
                    && delta.kind == "input_json_delta"
                    && let Some(tool) = self.open_tools.get_mut(&(Some(parent), index))
                {
                    tool.streamed_input
                        .push_str(&delta.partial_json.unwrap_or_default());
                }
            }
            "content_block_stop" => {
                if let Some(index) = event.index {
                    self.close_tool_use((Some(parent), index), &mut projected);
                }
            }
            _ => {}
        }
        projected
    }

    /// Starts tracking a `tool_use` block whose input is about to stream.
    fn open_tool_use(&mut self, parent: Option<String>, index: Option<u64>, block: ContentBlock) {
        let (Some(index), Some(id), Some(name)) = (index, block.id, block.name) else {
            return;
        };
        self.open_tools.insert(
            (parent, index),
            OpenToolUse {
                id,
                name,
                streamed_input: String::new(),
                opening_input: block.input,
            },
        );
    }

    /// Closes a `tool_use` block: a completed Bash tool use becomes a running Command Activity,
    /// recording the bare command. Any other tool, and input in no shape this projection reads,
    /// is passed over.
    fn close_tool_use(&mut self, key: ToolBlockKey, projected: &mut Vec<ProviderEvent>) {
        let Some(tool) = self.open_tools.remove(&key) else {
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
        self.running_commands.insert(tool.id, activity_id);
    }

    /// The tool results a `user` message echoes back, settling the commands they report on. A
    /// user message in any other shape is not the projection's to present.
    fn project_tool_results(&mut self, message: Value) -> Vec<ProviderEvent> {
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
            let Some(activity_id) = block
                .tool_use_id
                .and_then(|id| self.running_commands.remove(&id))
            else {
                continue;
            };
            let output = tool_result_text(&block.content);
            if !output.is_empty() {
                projected.push(ProviderEvent::CommandOutputDelta {
                    activity_id: activity_id.clone(),
                    content: output,
                });
            }
            projected.push(ProviderEvent::CommandCompleted {
                activity_id,
                status: if block.is_error {
                    ProviderCommandStatus::Failed
                } else {
                    ProviderCommandStatus::Completed
                },
                exit_status: None,
            });
        }
        projected
    }

    /// Opens the conversation's thinking block and the first Reasoning block of its split.
    fn open_thinking(
        &mut self,
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
        self.open_thinking = Some(thinking);
    }

    /// Settles the open thinking block, releasing whatever its split still withholds.
    fn settle_open_thinking(&mut self, projected: &mut Vec<ProviderEvent>) {
        let Some(mut thinking) = self.open_thinking.take() else {
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

    fn project_result(&mut self, message: Value) -> Result<Vec<ProviderEvent>, ProviderError> {
        let result: ResultMessage = serde_json::from_value(message).map_err(|error| {
            claude_error(format!(
                "Claude Code CLI sent a malformed result message: {error}"
            ))
        })?;
        let mut projected = Vec::new();
        // A result while blocks are still streaming is the CLI failing mid-stream; what did
        // stream stays in the Transcript, settled.
        self.settle_open_thinking(&mut projected);
        if self.open_text_block.take().is_some() {
            projected.push(ProviderEvent::AgentMessageCompleted);
        }
        // A command whose tool result never came back has no outcome to match, so it settles
        // as failed rather than holding the Turn open.
        self.open_tools.clear();
        for (_, activity_id) in std::mem::take(&mut self.running_commands) {
            projected.push(ProviderEvent::CommandCompleted {
                activity_id,
                status: ProviderCommandStatus::Failed,
                exit_status: None,
            });
        }
        if was_interrupted(&result) {
            self.turn.abandon_turn();
            projected.push(ProviderEvent::TurnInterrupted);
        } else if result.subtype == "success" && !result.is_error {
            // A steered Turn is answered stretch by stretch: the CLI ends every user message
            // queued into the loop with a result of its own, and only the last one Settles the
            // Turn that holds them all.
            if self.turn.result_settles_turn() {
                projected.push(ProviderEvent::TurnCompleted);
            }
        } else {
            self.turn.abandon_turn();
            projected.push(ProviderEvent::TurnFailed {
                message: super::result_failure_message("Turn", &result),
            });
        }
        Ok(projected)
    }
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

/// The next Reasoning block's identity. Blocks are numbered across the Session, in a namespace
/// apart from the tool-use ids commands are named by.
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
