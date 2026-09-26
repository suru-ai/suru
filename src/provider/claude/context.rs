//! Optional native context snapshots. A request captures its Turn, Model and ordering before it
//! runs; completion never takes the Session's child lock or delays conversation projection.

use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::{sync::mpsc, time::Duration};

use super::{transport::StreamJsonTransport, wire::ControlRequest};
use crate::{
    protocol::{ContextFill, TurnId},
    provider::{
        AttributedProviderEvent, ContextFillReport, ProviderEvent, ProviderEventAttribution,
    },
};

#[derive(Default)]
struct QueryState {
    transport: Option<StreamJsonTransport>,
    turn_id: Option<TurnId>,
    selected_model: String,
    native_model: Option<String>,
    sequence: u64,
    continuation: bool,
    ready: bool,
    delivered_sequence: u64,
}

pub(super) struct ContextQueries {
    state: Mutex<QueryState>,
    timeout: Duration,
    reports: mpsc::UnboundedSender<AttributedProviderEvent>,
}

impl ContextQueries {
    pub(super) fn new(
        timeout: Duration,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<AttributedProviderEvent>) {
        let (reports, receiver) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                state: Mutex::new(QueryState::default()),
                timeout,
                reports,
            }),
            receiver,
        )
    }

    pub(super) fn connect(&self, transport: StreamJsonTransport) {
        let mut state = self
            .state
            .lock()
            .expect("Claude context lock is not poisoned");
        state.transport = Some(transport);
        state.native_model = None;
    }

    pub(super) fn disconnect(&self) {
        self.state
            .lock()
            .expect("Claude context lock is not poisoned")
            .transport = None;
    }

    pub(super) fn begin_turn(&self, turn_id: TurnId, model: &str) {
        let mut state = self
            .state
            .lock()
            .expect("Claude context lock is not poisoned");
        state.turn_id = Some(turn_id);
        state.continuation = false;
        state.ready = false;
        state.selected_model = model.to_owned();
    }

    pub(super) fn ready(&self) {
        self.state
            .lock()
            .expect("Claude context lock is not poisoned")
            .ready = true;
    }

    pub(super) fn abandon(&self) {
        let mut state = self
            .state
            .lock()
            .expect("Claude context lock is not poisoned");
        state.ready = false;
        state.continuation = false;
    }

    pub(super) fn observe_output(&self, events: &[AttributedProviderEvent], prompt_running: bool) {
        if !prompt_running
            && events.iter().any(|event| {
                event.attribution == ProviderEventAttribution::OwningSession
                    // These are the same non-output events orchestration handles separately
                    // while idle. Everything else can open an owed Continuation, including a
                    // Usage-only result, a Questionnaire, or another Subagent spawn.
                    && !matches!(
                        event.event,
                        ProviderEvent::ContextFill { .. }
                            | ProviderEvent::TurnCompleted
                            | ProviderEvent::TurnInterrupted
                            | ProviderEvent::TurnFailed { .. }
                            | ProviderEvent::AgentSelectionChanged { .. }
                            | ProviderEvent::AgentSelectionRejected { .. }
                            | ProviderEvent::SubagentUpdated { .. }
                            | ProviderEvent::SubagentCompleted { .. }
                            | ProviderEvent::SubagentWoken { .. }
                            | ProviderEvent::SubagentSteered { .. }
                            | ProviderEvent::WatchStarted { .. }
                            | ProviderEvent::WatchSettled { .. }
                            | ProviderEvent::ResumeStateChanged { .. }
                    )
            })
        {
            let mut state = self
                .state
                .lock()
                .expect("Claude context lock is not poisoned");
            if state.ready {
                state.continuation = true;
            }
        }
    }

    pub(super) fn route_report(
        &self,
        mut event: AttributedProviderEvent,
    ) -> Option<AttributedProviderEvent> {
        let mut state = self
            .state
            .lock()
            .expect("Claude context lock is not poisoned");
        if let ProviderEvent::ContextFill { report } = &mut event.event {
            // Server ordering is per Turn; keep request ordering across implicit Continuations too.
            if report.sequence <= state.delivered_sequence {
                return None;
            }
            state.delivered_sequence = report.sequence;
            // Late owning output creates a Continuation in orchestration without a start_turn
            // callback. Only the ordered projection can bind a captured request to that Turn:
            // retain the explicit ID unless its Prompt generation is still current. Starting
            // another Prompt clears this permission before any fallible Provider work.
            if state.ready && state.continuation && report.turn_id == state.turn_id {
                report.turn_id = None;
            }
        }
        Some(event)
    }

    pub(super) fn observe(&self, message: &Value) {
        // The native request has no child selector. A child's compaction cannot measure that
        // child's context, and must not masquerade as a new measurement of the parent.
        if message
            .get("parent_tool_use_id")
            .is_some_and(|owner| !owner.is_null())
            || message.get("type").and_then(Value::as_str) != Some("system")
        {
            return;
        }
        match message.get("subtype").and_then(Value::as_str) {
            Some("init") => {
                if let Some(model) = message
                    .get("model")
                    .and_then(Value::as_str)
                    .filter(|m| !m.is_empty())
                {
                    self.state
                        .lock()
                        .expect("Claude context lock is not poisoned")
                        .native_model = Some(model.to_owned());
                }
            }
            Some("compact_boundary") => self.request(),
            _ => {}
        }
    }

    pub(super) fn request(&self) {
        let mut state = self
            .state
            .lock()
            .expect("Claude context lock is not poisoned");
        if !state.ready {
            return;
        }
        let (Some(transport), Some(turn_id)) = (state.transport.clone(), state.turn_id) else {
            return;
        };
        // init resolves aliases such as `default` to the Model this process actually runs.
        let model = state
            .native_model
            .clone()
            .unwrap_or_else(|| state.selected_model.clone());
        state.sequence += 1;
        let sequence = state.sequence;
        let timeout = self.timeout;
        let reports = self.reports.clone();
        tokio::spawn(async move {
            let Ok(response) = transport
                .control_request(&ControlRequest::GetContextUsage, timeout)
                .await
            else {
                return;
            };
            let Some(fill) = context_fill(&response, &model) else {
                return;
            };
            let _ = reports.send(AttributedProviderEvent {
                attribution: ProviderEventAttribution::OwningSession,
                event: ProviderEvent::ContextFill {
                    report: ContextFillReport {
                        turn_id: Some(turn_id),
                        sequence,
                        fill,
                    },
                },
            });
        });
    }
}

fn context_fill(response: &Value, expected_model: &str) -> Option<ContextFill> {
    let model = response.get("model")?.as_str()?;
    // The CLI may append its context tier to the concrete model identity in /context while
    // reporting the bare concrete identity in init. Capacity still comes exclusively from rawMaxTokens.
    if model.trim_end_matches("[1m]") != expected_model.trim_end_matches("[1m]") {
        return None;
    }
    Some(ContextFill {
        occupied_tokens: response.get("totalTokens")?.as_u64()?,
        capacity_tokens: response
            .get("rawMaxTokens")
            .and_then(Value::as_u64)
            .filter(|capacity| *capacity > 0),
    })
}
