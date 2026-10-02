//! Optional native context snapshots. A request captures its Turn, Model and ordering before it
//! runs; completion never takes the Session's child lock or delays conversation projection.

use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::{sync::mpsc, time::Duration};

use super::{
    transport::{ControlFailure, StreamJsonTransport},
    wire::ControlRequest,
};
use crate::{
    protocol::{ContextBreakdown, ContextFill, ContextItem, ContextPart, ContextSource, TurnId},
    provider::{
        AttributedProviderEvent, ContextFillReport, ProviderError, ProviderEvent,
        ProviderEventAttribution,
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
    /// The first sequence a request issued since the loop's own conversation last reported a
    /// compaction boundary carries: a reading from it on measures the context that compaction
    /// left, and one before it the context as it was.
    compacted_from: u64,
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
                            | ProviderEvent::SubagentDelegated { .. }
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
            Some("compact_boundary") => {
                {
                    let mut state = self
                        .state
                        .lock()
                        .expect("Claude context lock is not poisoned");
                    state.compacted_from = state.sequence + 1;
                }
                self.request();
            }
            _ => {}
        }
    }

    /// Whether `reading` was requested once the loop's own conversation reported its latest
    /// compaction boundary, and so measures the context that compaction left.
    pub(super) fn measures_compacted_context(&self, reading: &AttributedProviderEvent) -> bool {
        let ProviderEvent::ContextFill { report } = &reading.event else {
            return false;
        };
        report.sequence
            >= self
                .state
                .lock()
                .expect("Claude context lock is not poisoned")
                .compacted_from
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

    /// Asks the running CLI what occupies its context now, whatever Turn it is in. Unlike a Context
    /// Fill reading, the answer is the caller's alone: nothing is reported or ordered against
    /// Turns.
    pub(super) async fn breakdown(&self) -> Result<ContextBreakdown, ProviderError> {
        let transport = self
            .state
            .lock()
            .expect("Claude context lock is not poisoned")
            .transport
            .clone()
            .ok_or_else(|| ProviderError::new("Claude is not running for this Session"))?;
        let response = transport
            .control_request(&ControlRequest::GetContextUsage, self.timeout)
            .await
            .map_err(ControlFailure::into_error)?;
        context_breakdown(&response)
            .ok_or_else(|| ProviderError::new("Claude described its context in an unknown shape"))
    }
}

/// Reads `get_context_usage`'s categories: the `used` ones occupy the context and sum to its
/// total, a `buffer` is held back from the Agent, and the rest — free space, and Tool definitions
/// deferred until the Agent asks for them — occupy nothing.
fn context_breakdown(response: &Value) -> Option<ContextBreakdown> {
    let fill = raw_context_fill(response)?;
    let mut reserved_tokens = None;
    let mut parts = Vec::new();
    for category in response.get("categories")?.as_array()? {
        let (Some(name), Some(tokens)) = (
            category.get("name").and_then(Value::as_str),
            category.get("tokens").and_then(Value::as_u64),
        ) else {
            continue;
        };
        match category.get("kind").and_then(Value::as_str) {
            Some("used") => parts.push(context_part(response, name, tokens)),
            Some("buffer") => *reserved_tokens.get_or_insert(0) += tokens,
            _ => {}
        }
    }
    Some(ContextBreakdown {
        fill,
        reserved_tokens,
        parts,
    })
}

fn context_part(response: &Value, name: &str, tokens: u64) -> ContextPart {
    let (source, items) = match name {
        "System prompt" => (ContextSource::SystemPrompt, Vec::new()),
        "System tools" => (ContextSource::SystemTools, Vec::new()),
        "MCP tools" => (
            ContextSource::McpTools,
            named_items(response.get("mcpTools"), "name"),
        ),
        "Memory files" => (
            ContextSource::Instructions,
            named_items(response.get("memoryFiles"), "path"),
        ),
        "Skills" => (
            ContextSource::Skills,
            named_items(
                response
                    .get("skills")
                    .and_then(|skills| skills.get("skillFrontmatter")),
                "name",
            ),
        ),
        "Custom agents" => (
            ContextSource::Agents,
            named_items(response.get("agents"), "agentType"),
        ),
        "Messages" => (
            ContextSource::Messages,
            message_items(response.get("messageBreakdown")),
        ),
        other => (
            ContextSource::Other {
                label: other.to_owned(),
            },
            Vec::new(),
        ),
    };
    ContextPart {
        source,
        tokens,
        items,
    }
}

/// The entries of a native list, each named by its `label` field; an entry lacking either a name
/// or a token count is left out.
fn named_items(list: Option<&Value>, label: &str) -> Vec<ContextItem> {
    list.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            Some(ContextItem {
                label: entry.get(label)?.as_str()?.to_owned(),
                tokens: entry.get("tokens")?.as_u64()?,
            })
        })
        .collect()
}

/// The conversation by kind of message, leaving out the kinds holding nothing.
fn message_items(breakdown: Option<&Value>) -> Vec<ContextItem> {
    const KINDS: [(&str, &str); 7] = [
        ("toolCallTokens", "Tool calls"),
        ("toolResultTokens", "Tool results"),
        ("attachmentTokens", "Attachments"),
        ("assistantMessageTokens", "Agent messages"),
        ("userMessageTokens", "User messages"),
        ("redirectedContextTokens", "Redirected context"),
        ("unattributedTokens", "Unattributed"),
    ];
    let Some(breakdown) = breakdown else {
        return Vec::new();
    };
    KINDS
        .into_iter()
        .filter_map(|(field, label)| {
            let tokens = breakdown
                .get(field)?
                .as_u64()
                .filter(|tokens| *tokens > 0)?;
            Some(ContextItem {
                label: label.to_owned(),
                tokens,
            })
        })
        .collect()
}

fn context_fill(response: &Value, expected_model: &str) -> Option<ContextFill> {
    let model = response.get("model")?.as_str()?;
    // The CLI may append its context tier to the concrete model identity in /context while
    // reporting the bare concrete identity in init. Capacity still comes exclusively from rawMaxTokens.
    if model.trim_end_matches("[1m]") != expected_model.trim_end_matches("[1m]") {
        return None;
    }
    raw_context_fill(response)
}

/// `get_context_usage`'s occupancy against the raw Model window, never the effective `maxTokens`
/// a compaction buffer reduces.
fn raw_context_fill(response: &Value) -> Option<ContextFill> {
    Some(ContextFill {
        occupied_tokens: response.get("totalTokens")?.as_u64()?,
        capacity_tokens: response
            .get("rawMaxTokens")
            .and_then(Value::as_u64)
            .filter(|capacity| *capacity > 0),
    })
}
