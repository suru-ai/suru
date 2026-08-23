//! Projection of a Claude Session's conversation messages into Provider events.
//!
//! The CLI streams a Turn as partial-message chunks — Anthropic streaming events riding in
//! `stream_event` envelopes — and ends it with one `result` message. Text blocks the conversation
//! itself owns become the agent Message; chunks owned by a subagent, and every block kind this
//! slice does not present, are passed over rather than failed, because the wire grows freely
//! (ADR 0010). A `result` Settles the Turn as completed or failed.

use std::collections::VecDeque;

use futures_util::stream;
use serde_json::Value;
use tokio::sync::mpsc;

use super::{
    CLAUDE_FAILURE_FALLBACK, claude_error,
    wire::{ResultMessage, StreamEventMessage},
};
use crate::provider::{ProviderError, ProviderEvent, ProviderEventStream, concise_remote_message};

pub(super) fn provider_events(
    messages: mpsc::UnboundedReceiver<Result<Value, ProviderError>>,
) -> ProviderEventStream {
    Box::pin(stream::unfold(
        EventReceiver {
            messages,
            projection: ClaudeProjection::default(),
            pending: VecDeque::new(),
        },
        next_provider_event,
    ))
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

/// What the projection remembers between conversation messages: the streaming text block that is
/// the agent Message currently open, by the block index the CLI streams it under.
#[derive(Default)]
struct ClaudeProjection {
    open_text_block: Option<u64>,
}

impl ClaudeProjection {
    fn project(&mut self, message: Value) -> Result<Vec<ProviderEvent>, ProviderError> {
        match message.get("type").and_then(Value::as_str) {
            Some("stream_event") => self.project_stream_event(message),
            Some("result") => self.project_result(message),
            // The init message, full-message snapshots of what already streamed, tool results
            // echoed as user messages — nothing this slice presents.
            _ => Ok(Vec::new()),
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
        // A subagent's narration is its own conversation, not this one's Transcript.
        if message.parent_tool_use_id.is_some() {
            return Ok(Vec::new());
        }
        let event = message.event;
        let mut projected = Vec::new();
        match event.kind.as_str() {
            "content_block_start" => {
                let Some(block) = event.content_block else {
                    return Ok(Vec::new());
                };
                if block.kind != "text" {
                    return Ok(Vec::new());
                }
                // Blocks stream strictly one at a time, so a start while one is open means its
                // stop was lost; settle the open Message rather than interleaving two.
                if self.open_text_block.take().is_some() {
                    projected.push(ProviderEvent::AgentMessageCompleted);
                }
                self.open_text_block = event.index;
                projected.push(ProviderEvent::AgentMessageStarted);
                if let Some(text) = block.text.filter(|text| !text.is_empty()) {
                    projected.push(ProviderEvent::AgentMessageDelta { content: text });
                }
            }
            "content_block_delta" => {
                if self.open_text_block != event.index || self.open_text_block.is_none() {
                    return Ok(Vec::new());
                }
                let Some(delta) = event.delta else {
                    return Ok(Vec::new());
                };
                if delta.kind != "text_delta" {
                    return Ok(Vec::new());
                }
                projected.push(ProviderEvent::AgentMessageDelta {
                    content: delta.text.unwrap_or_default(),
                });
            }
            "content_block_stop"
                if event.index.is_some() && self.open_text_block == event.index =>
            {
                self.open_text_block = None;
                projected.push(ProviderEvent::AgentMessageCompleted);
            }
            // Thinking and tool blocks land in later slices; message boundaries carry nothing
            // the Transcript presents.
            _ => {}
        }
        Ok(projected)
    }

    fn project_result(&mut self, message: Value) -> Result<Vec<ProviderEvent>, ProviderError> {
        let result: ResultMessage = serde_json::from_value(message).map_err(|error| {
            claude_error(format!(
                "Claude Code CLI sent a malformed result message: {error}"
            ))
        })?;
        let mut projected = Vec::new();
        // A result while a text block is still open is the CLI failing mid-stream; what did
        // stream stays in the Transcript as a settled Message.
        if self.open_text_block.take().is_some() {
            projected.push(ProviderEvent::AgentMessageCompleted);
        }
        if result.subtype == "success" && !result.is_error {
            projected.push(ProviderEvent::TurnCompleted);
        } else {
            projected.push(ProviderEvent::TurnFailed {
                message: result_failure_message(&result),
            });
        }
        Ok(projected)
    }
}

/// The user-readable account of a failed result: the first user-facing error the CLI reported —
/// `[ede_diagnostic]` entries are CLI-internal telemetry the CLI hides from its own UI — then the
/// result text an errored `success` carries, then the bare subtype when the CLI said nothing more.
fn result_failure_message(result: &ResultMessage) -> String {
    let reported = result
        .errors
        .iter()
        .find(|error| !error.starts_with("[ede_diagnostic]"))
        .map(String::as_str)
        .or_else(|| result.result.as_ref().and_then(Value::as_str))
        .filter(|reported| !reported.trim().is_empty());
    match reported {
        Some(reported) => concise_remote_message(
            &format!("Claude Turn failed: {reported}"),
            CLAUDE_FAILURE_FALLBACK,
        ),
        None => format!(
            "Claude Turn failed: the Claude Code CLI reported `{}`",
            result.subtype
        ),
    }
}
