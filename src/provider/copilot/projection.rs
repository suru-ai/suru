//! Projection of Copilot's Session event stream onto Suru's Provider events.
//!
//! Copilot reports one long timeline per Session rather than one stream per Turn, so
//! [`CopilotCorrelation`] is the running state the projection needs: whether a Suru Turn is in
//! flight and which agent Message inside it is still streaming. Events that arrive outside a Turn
//! Suru is running are dropped, events that contradict the recorded state fail the Session, and
//! everything else becomes the Provider events a Session consumes.
//!
//! A Suru Turn spans Copilot's whole agentic loop: it opens when the Prompt is delivered and
//! settles on the session-level idle signal, not on the per-model-call `assistant.turn_end`. Idle
//! is the Turn's only settle point, because Copilot emits it mechanically whenever the loop stops —
//! including when it stopped on an error. An error therefore records what the Turn will settle as
//! rather than settling it: a Turn that settled early would leave its own trailing idle to be read
//! against whichever Turn had opened by the time it was projected.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex as StdMutex},
};

use futures_util::stream;
use github_copilot_sdk::{
    EventSubscription, SessionEvent,
    session_events::{
        AssistantMessageData, AssistantMessageDeltaData, AssistantMessageStartData,
        SessionErrorData, SessionEventType, SessionIdleData,
    },
    subscription::RecvErrorKind,
};
use tokio::sync::mpsc;

use super::{COPILOT_FAILURE_FALLBACK, copilot_error, transport::CopilotConnection};
use crate::provider::{
    ProviderError, ProviderEvent, ProviderEventStream, concise_remote_message,
    harness::SharedHarnessHandle,
};

/// Everything the projection must remember between events for one Copilot Session.
pub(super) struct CopilotCorrelation {
    turn: Option<ActiveTurn>,
}

/// The Suru Turn whose events are currently being projected.
struct ActiveTurn {
    message: Option<ActiveMessage>,
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
            failure: None,
        });
        Ok(())
    }

    /// Gives up the Turn opened by [`Self::begin_turn`] when the Prompt never reached Copilot.
    pub(super) fn abandon_turn(&mut self) {
        self.turn = None;
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
    let Some(turn) = correlation.turn.take() else {
        return Ok(Vec::new());
    };
    let mut projected = Vec::with_capacity(2);
    // A loop that stops mid-Message leaves it settled rather than streaming forever.
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
