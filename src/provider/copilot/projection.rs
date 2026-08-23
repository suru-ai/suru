//! Projection of Copilot's Session event stream onto Suru's Provider events.
//!
//! Copilot reports one long timeline per Session rather than one stream per Turn, so
//! [`CopilotCorrelation`] is the running state the projection needs: whether a Suru Turn is in
//! flight, and which agent Message, Reasoning blocks, and Tool executions inside it are still
//! running. Events that arrive outside a Turn Suru is running are dropped, events that contradict
//! the recorded state fail the Session, and everything else becomes the Provider events a Session
//! consumes.
//!
//! Only the agent Message is strict about that: a Message the Transcript shows the reader as the
//! answer must be the answer. Reasoning and Tool work report on how the answer was reached, so a
//! Copilot report the projection cannot make sense of costs the reader that report rather than the
//! Turn it belongs to.
//!
//! A Suru Turn spans Copilot's whole agentic loop: it opens when the Prompt is delivered and
//! settles on the session-level idle signal, not on the per-model-call `assistant.turn_end`. Idle
//! is the Turn's only settle point, because Copilot emits it mechanically whenever the loop stops —
//! including when it stopped on an error. An error therefore records what the Turn will settle as
//! rather than settling it: a Turn that settled early would leave its own trailing idle to be read
//! against whichever Turn had opened by the time it was projected.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex as StdMutex},
};

use futures_util::stream;
use github_copilot_sdk::{
    EventSubscription, SessionEvent,
    session_events::{
        AssistantMessageData, AssistantMessageDeltaData, AssistantMessageStartData,
        AssistantReasoningData, AssistantReasoningDeltaData, SessionErrorData, SessionEventType,
        SessionIdleData, ToolExecutionCompleteData, ToolExecutionPartialResultData,
        ToolExecutionStartData,
    },
    subscription::RecvErrorKind,
};
use tokio::sync::mpsc;

use super::{
    COPILOT_FAILURE_FALLBACK, copilot_error, tools::command_text, transport::CopilotConnection,
};
use crate::provider::{
    ProviderActivityId, ProviderCommandStatus, ProviderError, ProviderEvent, ProviderEventStream,
    concise_remote_message,
    harness::SharedHarnessHandle,
    reasoning::{ReasoningSegment, ReasoningSummarySplitter},
};

/// Everything the projection must remember between events for one Copilot Session.
pub(super) struct CopilotCorrelation {
    turn: Option<ActiveTurn>,
}

/// The Suru Turn whose events are currently being projected.
struct ActiveTurn {
    message: Option<ActiveMessage>,
    /// The Reasoning blocks the Turn has open, in the order Copilot opened them. Copilot's loop
    /// reasons one block through at a time, so this is a short list rather than a
    /// map, and settling the Turn walks it in the order a reader met it.
    reasoning: Vec<ActiveReasoning>,
    /// The Commands the Turn is still running, by the Tool call identity Copilot gave each. Copilot
    /// runs several Tools at once, so a Turn holds as many as it started.
    commands: HashMap<String, ActiveCommand>,
    /// What Copilot reported going wrong inside this Turn, which is what it settles as once the
    /// loop goes idle. The first report wins, because the failures after it are its consequences.
    failure: Option<String>,
}

/// The agent Message Copilot is still streaming, and the text it has carried so far — against
/// which the completed Message's repeat of it is reconciled.
struct ActiveMessage {
    message_id: String,
    streamed: String,
}

/// A Command Copilot is still running, and the output it has streamed so far — against which the
/// completed execution's repeat of it is reconciled.
#[derive(Default)]
struct ActiveCommand {
    streamed_output: String,
}

/// A Reasoning block Copilot still has open: the text it has streamed so far — against
/// which the completed block's repeat of it is reconciled — and the split holding its title back
/// until the head of the block resolves into one.
#[derive(Default)]
struct ActiveReasoning {
    reasoning_id: String,
    streamed: String,
    splitter: ReasoningSummarySplitter,
}

impl CopilotCorrelation {
    pub(super) fn new() -> Self {
        Self { turn: None }
    }

    /// Opens the Turn a Prompt is about to be delivered into. Copilot hosts one agentic loop per
    /// Session, so a second Turn cannot begin while one is running.
    pub(super) fn begin_turn(&mut self) -> Result<(), ProviderError> {
        if self.turn.is_some() {
            return Err(copilot_error(
                "Copilot started a Turn while another Turn was active",
            ));
        }
        self.turn = Some(ActiveTurn {
            message: None,
            reasoning: Vec::new(),
            commands: HashMap::new(),
            failure: None,
        });
        Ok(())
    }

    /// Gives up the Turn opened by [`Self::begin_turn`] when the Prompt never reached Copilot.
    pub(super) fn abandon_turn(&mut self) {
        self.turn = None;
    }

    /// Whether a Turn is running, which is what makes a Prompt delivered now a steer rather than
    /// the start of another Turn, and what there is for an interrupt to stop.
    pub(super) fn is_turn_running(&self) -> bool {
        self.turn.is_some()
    }
}

/// Streams the Provider events projected from one Copilot Session's timeline, failing the stream
/// when the shared harness process hosting it dies.
pub(super) fn provider_events(
    subscription: EventSubscription,
    harness: Arc<SharedHarnessHandle<CopilotConnection>>,
    correlation: Arc<StdMutex<CopilotCorrelation>>,
) -> ProviderEventStream {
    // The SDK drops the oldest events on a subscriber that falls behind, and a dropped delta is
    // Transcript content Suru cannot get back, so the timeline is drained as fast as it arrives
    // and queued here rather than at the pace the Session's consumer reads.
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    tokio::spawn(drain_session_timeline(subscription, events_tx));
    Box::pin(stream::unfold(
        CopilotEvents {
            events: events_rx,
            harness,
            correlation,
            pending: VecDeque::new(),
            ended: false,
        },
        next_provider_event,
    ))
}

/// Moves Copilot's timeline off the SDK's bounded subscription as it arrives.
async fn drain_session_timeline(
    mut subscription: EventSubscription,
    events: mpsc::UnboundedSender<Result<SessionEvent, ProviderError>>,
) {
    loop {
        match subscription.recv().await {
            Ok(event) => {
                if events.send(Ok(event)).is_err() {
                    return;
                }
            }
            Err(error) => {
                if let RecvErrorKind::Lagged(lagged) = error.kind() {
                    let _ = events.send(Err(copilot_error(format!(
                        "Copilot produced events faster than Suru could record them: {} were lost",
                        lagged.skipped()
                    ))));
                }
                return;
            }
        }
    }
}

struct CopilotEvents {
    events: mpsc::UnboundedReceiver<Result<SessionEvent, ProviderError>>,
    harness: Arc<SharedHarnessHandle<CopilotConnection>>,
    correlation: Arc<StdMutex<CopilotCorrelation>>,
    pending: VecDeque<Result<ProviderEvent, ProviderError>>,
    ended: bool,
}

async fn next_provider_event(
    mut events: CopilotEvents,
) -> Option<(Result<ProviderEvent, ProviderError>, CopilotEvents)> {
    loop {
        if let Some(event) = events.pending.pop_front() {
            return Some((event, events));
        }
        if events.ended {
            return None;
        }
        let harness = events.harness.clone();
        // A process that dies takes every Session it hosted with it, so its exit races the
        // timeline rather than leaving the Session waiting on a pipe nobody is left to write to.
        let received = tokio::select! {
            biased;
            crashed = harness.crashed() => {
                events.ended = true;
                return Some((Err(crashed), events));
            }
            received = events.events.recv() => received,
        };
        match received {
            None => {
                events.ended = true;
                return None;
            }
            Some(Err(error)) => {
                events.ended = true;
                return Some((Err(error), events));
            }
            Some(Ok(event)) => {
                let projected = {
                    let mut correlation = events
                        .correlation
                        .lock()
                        .expect("Copilot correlation lock is not poisoned");
                    project_session_event(&mut correlation, event)
                };
                match projected {
                    Ok(projected) => events.pending.extend(projected.into_iter().map(Ok)),
                    Err(error) => events.pending.push_back(Err(error)),
                }
            }
        }
    }
}

fn project_session_event(
    correlation: &mut CopilotCorrelation,
    event: SessionEvent,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    match event.parsed_type() {
        SessionEventType::AssistantMessageStart => {
            let started: AssistantMessageStartData = decode(&event)?;
            project_message_started(correlation, started.message_id)
        }
        SessionEventType::AssistantMessageDelta => {
            let delta: AssistantMessageDeltaData = decode(&event)?;
            project_message_delta(correlation, &delta.message_id, delta.delta_content)
        }
        SessionEventType::AssistantMessage => {
            let message: AssistantMessageData = decode(&event)?;
            project_message_completed(correlation, &message.message_id, &message.content)
        }
        SessionEventType::ToolExecutionStart => Ok(reported(&event)
            .map_or_else(Vec::new, |started: ToolExecutionStartData| {
                project_command_started(correlation, &started)
            })),
        SessionEventType::ToolExecutionPartialResult => Ok(reported(&event).map_or_else(
            Vec::new,
            |output: ToolExecutionPartialResultData| {
                project_command_output(correlation, &output.tool_call_id, output.partial_output)
            },
        )),
        SessionEventType::ToolExecutionComplete => Ok(reported(&event).map_or_else(
            Vec::new,
            |completed: ToolExecutionCompleteData| {
                project_command_completed(correlation, &completed)
            },
        )),
        SessionEventType::AssistantReasoningDelta => Ok(reported(&event).map_or_else(
            Vec::new,
            |delta: AssistantReasoningDeltaData| {
                project_reasoning_delta(correlation, &delta.reasoning_id, &delta.delta_content)
            },
        )),
        SessionEventType::AssistantReasoning => Ok(reported(&event).map_or_else(
            Vec::new,
            |reasoning: AssistantReasoningData| {
                project_reasoning_completed(
                    correlation,
                    &reasoning.reasoning_id,
                    &reasoning.content,
                )
            },
        )),
        // A transient error is one Copilot's own loop recovers from by retrying, so it is not the
        // Turn's outcome and stays out of the Transcript.
        SessionEventType::SessionError if event.is_transient_error() => Ok(Vec::new()),
        SessionEventType::SessionError => {
            let failure: SessionErrorData = decode(&event)?;
            project_session_error(correlation, &failure)
        }
        SessionEventType::SessionIdle => {
            let idle: SessionIdleData = decode(&event)?;
            project_session_idle(correlation, idle.aborted.unwrap_or(false))
        }
        _ => Ok(Vec::new()),
    }
}

/// Reads an event that reports on the work rather than carrying the answer, giving up on one Suru
/// cannot read. The Turn keeps going without it: the reader loses that report, which is what an
/// unreadable report costs, rather than the Turn it belongs to. The Log says one went, without the
/// conversation content it was carrying.
fn reported<T: serde::de::DeserializeOwned>(event: &SessionEvent) -> Option<T> {
    let decoded = event.typed_data();
    if decoded.is_none() {
        tracing::warn!(
            event = %event.event_type,
            "dropped a Copilot event Suru could not read"
        );
    }
    decoded
}

fn decode<T: serde::de::DeserializeOwned>(event: &SessionEvent) -> Result<T, ProviderError> {
    event.typed_data().ok_or_else(|| {
        copilot_error(format!(
            "Copilot sent an invalid `{}` event",
            event.event_type
        ))
    })
}

fn project_message_started(
    correlation: &mut CopilotCorrelation,
    message_id: String,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    let Some(turn) = correlation.turn.as_mut() else {
        return Ok(Vec::new());
    };
    if turn.message.is_some() {
        return Err(copilot_error(
            "Copilot started a second agent Message before completing the first",
        ));
    }
    turn.message = Some(ActiveMessage {
        message_id,
        streamed: String::new(),
    });
    Ok(vec![ProviderEvent::AgentMessageStarted])
}

fn project_message_delta(
    correlation: &mut CopilotCorrelation,
    message_id: &str,
    delta: String,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    let Some(turn) = correlation.turn.as_mut() else {
        return Ok(Vec::new());
    };
    let Some(message) = turn.message.as_mut() else {
        return Err(copilot_error(
            "Copilot sent agent Message content before starting the Message",
        ));
    };
    if message.message_id != message_id {
        return Ok(Vec::new());
    }
    message.streamed.push_str(&delta);
    Ok(vec![ProviderEvent::AgentMessageDelta { content: delta }])
}

/// Completes the streaming Message, or stands in for one Copilot never streamed: a Model call that
/// only asked for tools reports an empty Message, and one short enough to arrive whole reports it
/// without a start or a single delta.
fn project_message_completed(
    correlation: &mut CopilotCorrelation,
    message_id: &str,
    content: &str,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    let Some(turn) = correlation.turn.as_mut() else {
        return Ok(Vec::new());
    };
    let Some(message) = turn.message.as_ref() else {
        if content.is_empty() {
            return Ok(Vec::new());
        }
        return Ok(vec![
            ProviderEvent::AgentMessageStarted,
            ProviderEvent::AgentMessageDelta {
                content: content.to_owned(),
            },
            ProviderEvent::AgentMessageCompleted,
        ]);
    };
    if message.message_id != message_id {
        return Ok(Vec::new());
    }
    let Some(remaining) = content.strip_prefix(&message.streamed) else {
        return Err(copilot_error(
            "Copilot completed an agent Message with content that did not match its stream",
        ));
    };
    let mut projected = Vec::with_capacity(if remaining.is_empty() { 1 } else { 2 });
    if !remaining.is_empty() {
        projected.push(ProviderEvent::AgentMessageDelta {
            content: remaining.to_owned(),
        });
    }
    projected.push(ProviderEvent::AgentMessageCompleted);
    turn.message = None;
    Ok(projected)
}

/// Names the Activity one Tool execution projects onto. Copilot draws Tool call and Reasoning
/// identities from namespaces of their own, which the Provider seam gives one identity space, so
/// what tells them apart there is the kind they came from.
fn command_activity_id(tool_call_id: &str) -> ProviderActivityId {
    ProviderActivityId::new(format!("command:{tool_call_id}"))
}

/// Opens the Command a Tool execution is recorded as. Copilot reports no working directory of its
/// own for one: every Tool runs in the Session's Workspace, which the Session already carries.
fn project_command_started(
    correlation: &mut CopilotCorrelation,
    started: &ToolExecutionStartData,
) -> Vec<ProviderEvent> {
    let Some(turn) = correlation.turn.as_mut() else {
        return Vec::new();
    };
    if turn
        .commands
        .insert(started.tool_call_id.clone(), ActiveCommand::default())
        .is_some()
    {
        return Vec::new();
    }
    vec![ProviderEvent::CommandStarted {
        activity_id: command_activity_id(&started.tool_call_id),
        command: command_text(started),
        cwd: None,
    }]
}

/// Streams the next of a running Command's output into the Transcript.
fn project_command_output(
    correlation: &mut CopilotCorrelation,
    tool_call_id: &str,
    output: String,
) -> Vec<ProviderEvent> {
    let Some(turn) = correlation.turn.as_mut() else {
        return Vec::new();
    };
    let Some(command) = turn.commands.get_mut(tool_call_id) else {
        return Vec::new();
    };
    command.streamed_output.push_str(&output);
    vec![ProviderEvent::CommandOutputDelta {
        activity_id: command_activity_id(tool_call_id),
        content: output,
    }]
}

/// Settles the Command on what the Tool execution came to, carrying whatever of its output the
/// stream had not already reached.
fn project_command_completed(
    correlation: &mut CopilotCorrelation,
    completed: &ToolExecutionCompleteData,
) -> Vec<ProviderEvent> {
    let Some(turn) = correlation.turn.as_mut() else {
        return Vec::new();
    };
    let Some(command) = turn.commands.remove(&completed.tool_call_id) else {
        return Vec::new();
    };
    let mut projected = Vec::with_capacity(2);
    if let Some(trailing) = trailing_output(completed, &command.streamed_output) {
        projected.push(ProviderEvent::CommandOutputDelta {
            activity_id: command_activity_id(&completed.tool_call_id),
            content: trailing,
        });
    }
    projected.push(ProviderEvent::CommandCompleted {
        activity_id: command_activity_id(&completed.tool_call_id),
        status: if completed.success {
            ProviderCommandStatus::Completed
        } else {
            ProviderCommandStatus::Failed
        },
        // Copilot reports whether the Tool succeeded rather than what the command exited with, and
        // a shell Tool writes its exit code into the output it hands the Model.
        exit_status: None,
    });
    projected
}

/// What the completed execution adds to the output already streamed: the rest of a result the
/// stream had not reached, or the reason a failed one gives instead of a result.
///
/// A result that is not what streamed extends adds nothing. Copilot cuts the result it hands the
/// Model down for token efficiency, so the stream is the fuller record, and replacing it would show
/// the reader the same output twice.
fn trailing_output(completed: &ToolExecutionCompleteData, streamed: &str) -> Option<String> {
    let reported = match completed.result.as_ref() {
        Some(result) => result
            .detailed_content
            .as_deref()
            .unwrap_or(result.content.as_str()),
        None => completed
            .error
            .as_ref()
            .map_or("", |error| error.message.as_str()),
    };
    if let Some(trailing) = reported.strip_prefix(streamed) {
        return (!trailing.is_empty()).then(|| trailing.to_owned());
    }
    // A failure reports why rather than more output, so it does not continue the stream: it is
    // added below what the command had produced rather than onto the end of its last line.
    if completed.success || reported.is_empty() {
        return None;
    }
    Some(if streamed.is_empty() || streamed.ends_with('\n') {
        reported.to_owned()
    } else {
        format!("\n{reported}")
    })
}

/// Names the Activity one Reasoning block projects onto, in the identity space
/// [`command_activity_id`] explains.
fn reasoning_activity_id(reasoning_id: &str) -> ProviderActivityId {
    ProviderActivityId::new(format!("reasoning:{reasoning_id}"))
}

/// Streams the next of a Reasoning block into the Transcript, opening the block on the first of it
/// Copilot sends: Copilot announces a block by reasoning into it rather than with an event of its
/// own.
fn project_reasoning_delta(
    correlation: &mut CopilotCorrelation,
    reasoning_id: &str,
    delta: &str,
) -> Vec<ProviderEvent> {
    let Some(turn) = correlation.turn.as_mut() else {
        return Vec::new();
    };
    let mut projected = Vec::new();
    if !turn
        .reasoning
        .iter()
        .any(|block| block.reasoning_id == reasoning_id)
    {
        projected.push(ProviderEvent::ReasoningStarted {
            activity_id: reasoning_activity_id(reasoning_id),
        });
        turn.reasoning.push(ActiveReasoning {
            reasoning_id: reasoning_id.to_owned(),
            ..ActiveReasoning::default()
        });
    }
    let block = turn
        .reasoning
        .iter_mut()
        .find(|block| block.reasoning_id == reasoning_id)
        .expect("the block is open, having been opened just now when it was not");
    block.streamed.push_str(delta);
    let segment = block.splitter.push(delta);
    projected.extend(reasoning_segment_events(reasoning_id, segment));
    projected
}

/// Settles the block Copilot has reasoned through, or stands in for one it never streamed: a Model
/// that reasons without streaming reports the block whole and only once.
fn project_reasoning_completed(
    correlation: &mut CopilotCorrelation,
    reasoning_id: &str,
    content: &str,
) -> Vec<ProviderEvent> {
    let Some(turn) = correlation.turn.as_mut() else {
        return Vec::new();
    };
    let mut block = turn
        .reasoning
        .iter()
        .position(|block| block.reasoning_id == reasoning_id)
        .map_or_else(ActiveReasoning::default, |open| turn.reasoning.remove(open));
    let mut projected = Vec::new();
    if block.streamed.is_empty() {
        projected.push(ProviderEvent::ReasoningStarted {
            activity_id: reasoning_activity_id(reasoning_id),
        });
    }
    // The completed block repeats what streamed and carries whatever the stream had not reached.
    // When the two disagree the stream already showed the reader a coherent block, so repeating the
    // completed one on top of it would double the text rather than correct it.
    let remaining = content
        .strip_prefix(block.streamed.as_str())
        .unwrap_or_default();
    if !remaining.is_empty() {
        projected.extend(reasoning_segment_events(
            reasoning_id,
            block.splitter.push(remaining),
        ));
    }
    projected.extend(settle_reasoning(reasoning_id, &mut block));
    projected
}

/// Settles the Activity a Reasoning block streamed into, releasing whatever its split still
/// withholds, so no block is left open for a Turn to settle around.
fn settle_reasoning(reasoning_id: &str, block: &mut ActiveReasoning) -> Vec<ProviderEvent> {
    let mut projected = reasoning_segment_events(reasoning_id, block.splitter.finish());
    projected.push(ProviderEvent::ReasoningCompleted {
        activity_id: reasoning_activity_id(reasoning_id),
    });
    projected
}

/// Settles every block the stopped loop left open. The Session store settles a Turn's open
/// Activities from its own snapshot, but only the split here knows the title it is still
/// withholding, which would otherwise go with the block.
fn settle_open_reasoning(turn: &mut ActiveTurn) -> Vec<ProviderEvent> {
    let mut projected = Vec::new();
    for mut block in std::mem::take(&mut turn.reasoning) {
        let reasoning_id = std::mem::take(&mut block.reasoning_id);
        projected.extend(settle_reasoning(&reasoning_id, &mut block));
    }
    projected
}

/// Lowers one split step of a Reasoning block onto the Provider events that carry it, dropping the
/// step that resolved nothing because the split is still withholding the head.
fn reasoning_segment_events(reasoning_id: &str, segment: ReasoningSegment) -> Vec<ProviderEvent> {
    let mut projected = Vec::new();
    if let Some(title) = segment.title {
        projected.push(ProviderEvent::ReasoningTitleChanged {
            activity_id: reasoning_activity_id(reasoning_id),
            title,
        });
    }
    if !segment.content.is_empty() {
        projected.push(ProviderEvent::ReasoningDelta {
            activity_id: reasoning_activity_id(reasoning_id),
            content: segment.content,
        });
    }
    projected
}

/// Records what the Turn will settle as when Copilot reports an error — authentication, quota,
/// rate limit, and the rest — naming the category Copilot typed it as, so a remote failure reads
/// without opening the Log. The loop stopping is what settles the Turn on it.
fn project_session_error(
    correlation: &mut CopilotCorrelation,
    failure: &SessionErrorData,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    let Some(turn) = correlation.turn.as_mut() else {
        return Ok(Vec::new());
    };
    let kind = failure.error_type.replace(['_', '-'], " ");
    turn.failure.get_or_insert_with(|| {
        concise_remote_message(
            &format!("Copilot {kind} error: {}", failure.message),
            COPILOT_FAILURE_FALLBACK,
        )
    });
    Ok(Vec::new())
}

/// Settles the Turn on the signal that Copilot's agentic loop has stopped: the whole loop is the
/// Turn, so its idle is the Turn's outcome — whatever the loop met on the way there, and an idle
/// the abort produced is an interruption.
fn project_session_idle(
    correlation: &mut CopilotCorrelation,
    aborted: bool,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    let Some(mut turn) = correlation.turn.take() else {
        return Ok(Vec::new());
    };
    // A loop that stops mid-Message or mid-block leaves both settled rather than running forever.
    let mut projected = settle_open_reasoning(&mut turn);
    if turn.message.is_some() {
        projected.push(ProviderEvent::AgentMessageCompleted);
    }
    projected.push(match (turn.failure, aborted) {
        (Some(message), _) => ProviderEvent::TurnFailed { message },
        (None, true) => ProviderEvent::TurnInterrupted,
        (None, false) => ProviderEvent::TurnCompleted,
    });
    Ok(projected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(event_type: &str, data: serde_json::Value) -> SessionEvent {
        SessionEvent {
            id: "fixture-event".to_owned(),
            timestamp: "2026-01-01T00:00:00Z".to_owned(),
            parent_id: None,
            ephemeral: None,
            agent_id: None,
            debug_cli_received_at_ms: None,
            debug_ws_forwarded_at_ms: None,
            event_type: event_type.to_owned(),
            data,
        }
    }

    fn project(
        correlation: &mut CopilotCorrelation,
        event_type: &str,
        data: serde_json::Value,
    ) -> Vec<ProviderEvent> {
        project_session_event(correlation, event(event_type, data))
            .unwrap_or_else(|error| panic!("`{event_type}` projects cleanly, got: {error}"))
    }

    fn in_turn() -> CopilotCorrelation {
        let mut correlation = CopilotCorrelation::new();
        correlation.begin_turn().expect("open the Turn");
        correlation
    }

    #[test]
    fn a_timeline_outside_a_turn_projects_nothing() {
        let mut correlation = CopilotCorrelation::new();
        assert!(
            project(
                &mut correlation,
                "assistant.message_start",
                json!({ "messageId": "m1" }),
            )
            .is_empty()
        );
        assert!(project(&mut correlation, "session.idle", json!({})).is_empty());
    }

    #[test]
    fn a_completed_message_carries_the_content_the_stream_had_not_reached() {
        let mut correlation = in_turn();
        project(
            &mut correlation,
            "assistant.message_start",
            json!({ "messageId": "m1" }),
        );
        project(
            &mut correlation,
            "assistant.message_delta",
            json!({ "messageId": "m1", "deltaContent": "Hello" }),
        );
        assert_eq!(
            project(
                &mut correlation,
                "assistant.message",
                json!({ "messageId": "m1", "content": "Hello from Copilot" }),
            ),
            [
                ProviderEvent::AgentMessageDelta {
                    content: " from Copilot".to_owned()
                },
                ProviderEvent::AgentMessageCompleted,
            ]
        );
    }

    #[test]
    fn a_message_that_never_streamed_still_reaches_the_transcript_whole() {
        let mut correlation = in_turn();
        assert_eq!(
            project(
                &mut correlation,
                "assistant.message",
                json!({ "messageId": "m1", "content": "Whole" }),
            ),
            [
                ProviderEvent::AgentMessageStarted,
                ProviderEvent::AgentMessageDelta {
                    content: "Whole".to_owned()
                },
                ProviderEvent::AgentMessageCompleted,
            ]
        );
    }

    #[test]
    fn a_message_that_only_asked_for_tools_adds_nothing_to_the_transcript() {
        let mut correlation = in_turn();
        assert!(
            project(
                &mut correlation,
                "assistant.message",
                json!({ "messageId": "m1", "content": "" }),
            )
            .is_empty()
        );
    }

    #[test]
    fn content_belonging_to_another_message_is_dropped() {
        let mut correlation = in_turn();
        project(
            &mut correlation,
            "assistant.message_start",
            json!({ "messageId": "m1" }),
        );
        assert!(
            project(
                &mut correlation,
                "assistant.message_delta",
                json!({ "messageId": "other", "deltaContent": "wrong Message content" }),
            )
            .is_empty()
        );
    }

    #[test]
    fn a_completion_that_contradicts_the_stream_fails_the_session() {
        let mut correlation = in_turn();
        project(
            &mut correlation,
            "assistant.message_start",
            json!({ "messageId": "m1" }),
        );
        project(
            &mut correlation,
            "assistant.message_delta",
            json!({ "messageId": "m1", "deltaContent": "Hello" }),
        );
        let failure = project_session_event(
            &mut correlation,
            event(
                "assistant.message",
                json!({ "messageId": "m1", "content": "something else entirely" }),
            ),
        )
        .expect_err("a completion that contradicts the stream fails the Session");
        assert!(
            failure.to_string().contains("did not match its stream"),
            "the failure says what contradicted what, got: {failure}"
        );
    }

    #[test]
    fn reasoning_streams_into_the_transcript_with_the_title_it_leads_with_kept_apart() {
        let mut correlation = in_turn();
        let block = reasoning_activity_id("r1");
        assert_eq!(
            project(
                &mut correlation,
                "assistant.reasoning_delta",
                json!({ "reasoningId": "r1", "deltaContent": "**Reading the seam**\n\nOpening" }),
            ),
            [
                ProviderEvent::ReasoningStarted {
                    activity_id: block.clone()
                },
                ProviderEvent::ReasoningTitleChanged {
                    activity_id: block.clone(),
                    title: "Reading the seam".to_owned()
                },
                ProviderEvent::ReasoningDelta {
                    activity_id: block.clone(),
                    content: "Opening".to_owned()
                },
            ]
        );
        assert_eq!(
            project(
                &mut correlation,
                "assistant.reasoning",
                json!({ "reasoningId": "r1", "content": "**Reading the seam**\n\nOpening the projection." }),
            ),
            [
                ProviderEvent::ReasoningDelta {
                    activity_id: block.clone(),
                    content: " the projection.".to_owned()
                },
                ProviderEvent::ReasoningCompleted { activity_id: block },
            ]
        );
    }

    #[test]
    fn reasoning_a_model_never_streamed_still_reaches_the_transcript_whole() {
        let mut correlation = in_turn();
        let block = reasoning_activity_id("r1");
        assert_eq!(
            project(
                &mut correlation,
                "assistant.reasoning",
                json!({ "reasoningId": "r1", "content": "**Weighing it up**\n\nBoth seams work." }),
            ),
            [
                ProviderEvent::ReasoningStarted {
                    activity_id: block.clone()
                },
                ProviderEvent::ReasoningTitleChanged {
                    activity_id: block.clone(),
                    title: "Weighing it up".to_owned()
                },
                ProviderEvent::ReasoningDelta {
                    activity_id: block.clone(),
                    content: "Both seams work.".to_owned()
                },
                ProviderEvent::ReasoningCompleted { activity_id: block },
            ]
        );
    }

    #[test]
    fn an_idle_settles_the_reasoning_the_turn_never_finished() {
        let mut correlation = in_turn();
        project(
            &mut correlation,
            "assistant.reasoning_delta",
            json!({ "reasoningId": "r1", "deltaContent": "**Reading the seam**" }),
        );

        assert_eq!(
            project(&mut correlation, "session.idle", json!({ "aborted": true })),
            [
                ProviderEvent::ReasoningTitleChanged {
                    activity_id: reasoning_activity_id("r1"),
                    title: "Reading the seam".to_owned()
                },
                ProviderEvent::ReasoningCompleted {
                    activity_id: reasoning_activity_id("r1")
                },
                ProviderEvent::TurnInterrupted,
            ],
            "a block cut short keeps the title its split was still withholding"
        );
    }

    #[test]
    fn a_command_streams_its_output_into_the_transcript_and_settles_on_its_outcome() {
        let mut correlation = in_turn();
        let command = command_activity_id("t1");
        assert_eq!(
            project(
                &mut correlation,
                "tool.execution_start",
                json!({
                    "toolCallId": "t1",
                    "toolName": "bash",
                    "arguments": { "command": "cargo nextest run" },
                }),
            ),
            [ProviderEvent::CommandStarted {
                activity_id: command.clone(),
                command: "cargo nextest run".to_owned(),
                cwd: None,
            }]
        );
        assert_eq!(
            project(
                &mut correlation,
                "tool.execution_partial_result",
                json!({ "toolCallId": "t1", "partialOutput": "running tests\n" }),
            ),
            [ProviderEvent::CommandOutputDelta {
                activity_id: command.clone(),
                content: "running tests\n".to_owned(),
            }]
        );
        assert_eq!(
            project(
                &mut correlation,
                "tool.execution_complete",
                json!({
                    "toolCallId": "t1",
                    "success": true,
                    "result": { "content": "running tests\nall green\n" },
                }),
            ),
            [
                ProviderEvent::CommandOutputDelta {
                    activity_id: command.clone(),
                    content: "all green\n".to_owned(),
                },
                ProviderEvent::CommandCompleted {
                    activity_id: command,
                    status: ProviderCommandStatus::Completed,
                    exit_status: None,
                },
            ]
        );
    }

    #[test]
    fn a_command_that_failed_settles_as_failed_with_what_copilot_said_went_wrong() {
        let mut correlation = in_turn();
        let command = command_activity_id("t1");
        project(
            &mut correlation,
            "tool.execution_start",
            json!({
                "toolCallId": "t1",
                "toolName": "bash",
                "arguments": { "command": "cargo nextest run" },
            }),
        );

        assert_eq!(
            project(
                &mut correlation,
                "tool.execution_complete",
                json!({
                    "toolCallId": "t1",
                    "success": false,
                    "error": { "message": "command timed out" },
                }),
            ),
            [
                ProviderEvent::CommandOutputDelta {
                    activity_id: command.clone(),
                    content: "command timed out".to_owned(),
                },
                ProviderEvent::CommandCompleted {
                    activity_id: command,
                    status: ProviderCommandStatus::Failed,
                    exit_status: None,
                },
            ]
        );
    }

    #[test]
    fn a_command_that_failed_part_way_keeps_the_reason_off_the_output_it_had_produced() {
        let mut correlation = in_turn();
        project(
            &mut correlation,
            "tool.execution_start",
            json!({
                "toolCallId": "t1",
                "toolName": "bash",
                "arguments": { "command": "cargo nextest run" },
            }),
        );
        project(
            &mut correlation,
            "tool.execution_partial_result",
            json!({ "toolCallId": "t1", "partialOutput": "running tests" }),
        );

        let projected = project(
            &mut correlation,
            "tool.execution_complete",
            json!({
                "toolCallId": "t1",
                "success": false,
                "error": { "message": "command timed out" },
            }),
        );
        let [ProviderEvent::CommandOutputDelta { content, .. }, _] = projected.as_slice() else {
            panic!("a failed command carries its reason into its output, got {projected:?}");
        };
        assert_eq!(content, "\ncommand timed out");
    }

    #[test]
    fn an_idle_settles_the_turn_and_whatever_it_left_streaming() {
        let mut correlation = in_turn();
        project(
            &mut correlation,
            "assistant.message_start",
            json!({ "messageId": "m1" }),
        );
        assert_eq!(
            project(&mut correlation, "session.idle", json!({})),
            [
                ProviderEvent::AgentMessageCompleted,
                ProviderEvent::TurnCompleted
            ]
        );
        assert!(
            project(&mut correlation, "session.idle", json!({})).is_empty(),
            "the Turn settles once"
        );
    }

    #[test]
    fn an_aborted_idle_settles_the_turn_as_interrupted() {
        let mut correlation = in_turn();
        assert_eq!(
            project(&mut correlation, "session.idle", json!({ "aborted": true })),
            [ProviderEvent::TurnInterrupted]
        );
    }

    #[test]
    fn a_copilot_error_fails_the_turn_with_the_category_copilot_typed_it_as() {
        let mut correlation = in_turn();
        assert!(
            project(
                &mut correlation,
                "session.error",
                json!({ "errorType": "rate_limit", "message": "too many requests" }),
            )
            .is_empty(),
            "the loop stopping is what settles the Turn on the error"
        );
        let projected = project(&mut correlation, "session.idle", json!({}));
        let [ProviderEvent::TurnFailed { message }] = projected.as_slice() else {
            panic!("a Copilot error fails the Turn, got {projected:?}");
        };
        assert_eq!(message, "Copilot rate limit error: too many requests");
    }

    #[test]
    fn the_first_error_is_what_the_turn_settles_as() {
        let mut correlation = in_turn();
        for message in ["the cause", "a consequence"] {
            project(
                &mut correlation,
                "session.error",
                json!({ "errorType": "quota", "message": message }),
            );
        }
        let projected = project(&mut correlation, "session.idle", json!({}));
        let [ProviderEvent::TurnFailed { message }] = projected.as_slice() else {
            panic!("a Copilot error fails the Turn, got {projected:?}");
        };
        assert_eq!(message, "Copilot quota error: the cause");
    }

    #[test]
    fn a_failed_turns_idle_settles_that_turn_rather_than_the_one_after_it() {
        let mut correlation = in_turn();
        project(
            &mut correlation,
            "session.error",
            json!({ "errorType": "authentication", "message": "sign in again" }),
        );
        project(&mut correlation, "session.idle", json!({}));

        correlation
            .begin_turn()
            .expect("the Session takes the next Prompt");
        assert_eq!(
            project(
                &mut correlation,
                "assistant.message",
                json!({ "messageId": "m2", "content": "Second answer" }),
            ),
            [
                ProviderEvent::AgentMessageStarted,
                ProviderEvent::AgentMessageDelta {
                    content: "Second answer".to_owned()
                },
                ProviderEvent::AgentMessageCompleted,
            ],
            "the Turn after a failed one runs rather than settling on the failed one's idle"
        );
        assert_eq!(
            project(&mut correlation, "session.idle", json!({})),
            [ProviderEvent::TurnCompleted]
        );
    }

    #[test]
    fn a_transient_error_leaves_the_turn_running() {
        let mut correlation = in_turn();
        assert!(
            project(
                &mut correlation,
                "session.error",
                json!({ "errorType": "model_call", "message": "retrying" }),
            )
            .is_empty()
        );
        assert_eq!(
            project(&mut correlation, "session.idle", json!({})),
            [ProviderEvent::TurnCompleted],
            "the Turn Copilot recovered inside still completes"
        );
    }
}
