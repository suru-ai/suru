//! Projection of Copilot's Session event stream onto Suru's attributed Provider events.
//!
//! Copilot reports one long timeline per Session rather than one stream per Turn, and the timeline
//! carries several conversations at once: the main agent's, and one for every subagent the runtime
//! delegates to, whose events ride under the envelope's `agentId`. [`CopilotCorrelation`] is the
//! running state the projection needs: whether a Suru Turn is in flight, the agent Message,
//! Reasoning blocks, and Tool executions each conversation still has open, and the Subagents the
//! lifecycle events have opened. Every event leaves here attributed to the conversation that
//! produced it, so orchestration lands a subagent's work in the Subagent's own child Session
//! rather than the parent's Transcript.
//!
//! The `subagent.*` lifecycle is where Subagents are spawned and their first stretch ends:
//! `subagent.started` opens the Subagent under the instance identity its work is attributed with,
//! and `subagent.completed` or `subagent.failed` — addressed by the spawning tool call — settles
//! that stretch. The spawning tool execution itself projects no Command row while the Subagent row
//! answers for the delegation ([`SPAWN_TOOL`]). Main-conversation content outside a Turn Suru is
//! running is dropped unless owed a Continuation; context snapshots can refresh idle Sessions.
//! Events that contradict the recorded state fail the Session, and everything else becomes the
//! Provider events a Session consumes.
//!
//! Only the agent Message is strict about that: a Message the Transcript shows the reader as the
//! answer must be the answer. Reasoning and Tool work report on how the answer was reached, so a
//! Copilot report the projection cannot make sense of costs the reader that report rather than the
//! Turn it belongs to.
//!
//! A Delegation sent through `write_agent` — by the main agent or by a sibling Subagent — reaches
//! the Subagent as a `user.message` under its instance identity, reported when the Subagent
//! consumes it, and attributed to the Agent its `source` names
//! ([`CopilotCorrelation::project_subagent_message`]). Delivered into a working stretch
//! (`delivery: "steering"`) it **steers** that stretch, standing in the Subagent's current Turn at
//! the point it arrives (ADR 0032). The CLIs verified against, 1.0.87 and 1.0.88, never deliver
//! one that way, though: a message sent while the Subagent works is queued until its stretch has
//! ended, and one sent afterwards starts a run at once. Either way Copilot runs the settled agent
//! again on its own context with no second `subagent.started`, which **resumes** it (ADR 0033): a
//! new Turn in the Subagent's Session, opened by the message as its Delegation, whatever the
//! `delivery` says (docs/validation/0397-copilot-subagent-steer.md,
//! docs/validation/0398-copilot-subagent-resume.md). Nothing closes the resumed run either, so it
//! settles where the agent loop exits: the Subagent's `assistant.turn_end` after a model message
//! requesting no tools, with the next message it consumes and the loop's idle as backstops. The
//! `agent_idle` notification Copilot sometimes raises for it settles nothing. The send itself is
//! no work of the sender's, so its `write_agent` execution projects nothing in the sender's
//! Transcript ([`WRITE_AGENT_TOOL`]). Nor does a call to one of the Broker's Tools, which the
//! Broker's own rows answer for ([`is_broker_call`]).
//!
//! A Suru Turn spans one stretch of Copilot's agentic loop: it opens when the Prompt is delivered
//! and settles on the session-level idle signal, not on the per-model-call `assistant.turn_end`.
//! Idle is the Turn's only settle point, because Copilot emits it mechanically whenever the loop
//! stops — including when it stopped on an error. An error therefore records what the Turn will
//! settle as rather than settling it: a Turn that settled early would leave its own trailing idle
//! to be read against whichever Turn had opened by the time it was projected.
//!
//! The idle speaks only for the main conversation — and for any resumed Subagent run still open,
//! which Copilot closes with nothing else: a spawned Subagent's streams live past it, which is
//! what lets a Subagent outlive the Turn (ADR 0015). Main output arriving after the settle, owed
//! to Subagents still working or just settled, begins a Continuation stretch that the loop's next
//! idle settles — unless a Prompt settles the Continuation first, in which case that stretch's
//! idle is owed nothing and is swallowed rather than read against the Prompt's own Turn.
//!
//! A **detached** background shell is the Copilot Watch (ADR 0030): the shell tool run with
//! `mode: "async", detach: true` outlives the loop without deferring its idle, and its completion
//! notification wakes the loop again. Its start is read off the Session's task roster rather than
//! the tool call ([`CopilotCorrelation::project_task_roster`]), and its settle off the
//! `shell_detached_completed` notification ([`CopilotCorrelation::project_system_notification`]).
//! An *attached* background shell is no Watch here: Copilot holds the idle back until it ends,
//! so it keeps the Turn itself open (#379).

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex as StdMutex},
};

use futures_util::stream;
use github_copilot_sdk::{
    EventSubscription, SessionEvent,
    rpc::{TaskShellInfo, TaskShellInfoAttachmentMode, TaskStatus},
    session::Session as NativeSession,
    session_events::{
        AssistantMessageData, AssistantMessageDeltaData, AssistantMessageStartData,
        AssistantReasoningData, AssistantReasoningDeltaData, AssistantUsageData, SessionErrorData,
        SessionEventType, SessionIdleData, SubagentCompletedData, SubagentFailedData,
        SubagentStartedData, SystemNotificationData, ToolExecutionCompleteData,
        ToolExecutionPartialResultData, ToolExecutionStartData, UserMessageData,
        UserMessageDelivery,
    },
    subscription::RecvErrorKind,
};
use tokio::{
    sync::mpsc,
    time::{Duration, timeout},
};

use super::{
    COPILOT_FAILURE_FALLBACK, COPILOT_HARNESS_NAME, copilot_error,
    event_drain::EventDrainCheckpoint, pricing::CopilotPricing, session::until_crash,
    skills::CopilotSkills, tools::command_text, transport::CopilotConnection,
};
use crate::broker::BROKER_SERVER_NAME;
use crate::protocol::{ContextFill, NativeMeter, TurnId, Usage};
use crate::provider::{
    AttributedProviderEvent, ContextFillReport, ProviderActivityId, ProviderCommandStatus,
    ProviderError, ProviderEvent, ProviderEventAttribution, ProviderEventStream,
    ProviderSubagentId, ProviderSubagentStatus, ProviderWatchId, ProviderWatchOutcome,
    ReportedTurnMetering, concise_remote_message, exclusive_count, first_line,
    harness::SharedHarnessHandle,
    reasoning::{ReasoningSegment, ReasoningSummarySplitter},
    reported_count,
};

/// Everything the projection must remember between events for one Copilot Session.
pub(super) struct CopilotCorrelation {
    /// The Suru Turn the main conversation's events currently belong to — the Turn a Prompt
    /// began, or the Continuation stretch late output opened.
    turn: Option<ActiveTurn>,
    /// Loop stretches whose idle is still to come after Suru stopped owning them: a Prompt
    /// settles a Continuation early (ADR 0015), and the stretch it cut off still ends with an
    /// idle of its own. Each owed idle is swallowed rather than read against the Turn that
    /// Prompt began.
    stale_stretches: u32,
    /// Each working Subagent's current stretch and the streams it has open, by the agent instance
    /// identity its events are attributed with.
    subagents: HashMap<String, WorkingSubagent>,
    /// The Subagent each spawning tool call opened: `subagent.completed` and `subagent.failed`
    /// name the spawn's tool call rather than the instance their own envelope names.
    spawns: HashMap<String, String>,
    /// Every spawning tool call whose `subagent.started` has arrived, kept past the Subagent's
    /// settle: the Subagent row answers for the delegation, so the spawn's own execution stays
    /// withheld however late it is reported — before its `subagent.started` or after the settle.
    delegations: HashSet<String>,
    /// Every Subagent instance ever opened on this timeline, kept past its settle: a message
    /// Copilot delivers to a settled Subagent resumes it (ADR 0033), and names the Agent that sent
    /// it by instance identity — a sibling that may have settled before the Subagent it wrote to
    /// received what it sent.
    agents: HashMap<String, KnownSubagent>,
    /// Whether a Subagent has settled since a Turn last began — or a detached shell's completion
    /// woke the loop while no Turn was running. The output a completion provokes arrives only
    /// after the settle, so every settle leaves a Continuation owed to whatever that output turns
    /// out to be; a Turn beginning clears it, because from then on such output has a Turn to
    /// land in.
    late_settle_owes_continuation: bool,
    /// The detached shells live as Watches, by the shell identity Copilot's completion
    /// notification names them by. All are the owning Session's: the task roster says nothing of
    /// which conversation ran the shell tool, and Copilot delivers the completion to the main
    /// loop.
    watches: HashSet<String>,
    /// Every detached shell ever started as a Watch on this timeline, kept past its settle: the
    /// roster keeps listing a shell for a while after it ends, and a read racing the completion
    /// could otherwise start the settled shell's Watch a second time.
    watched_shells: HashSet<String>,
    /// Copilot's most recently published per-Model prices. The connection
    /// updates this on catalog discovery; each usage event reads it once while
    /// the call still belongs to an active Turn.
    pricing: CopilotPricing,
    /// Last Prompt delivered to this native timeline. Retained through idle so context
    /// observations can refresh a settled Session without opening a Continuation.
    context_turn: Option<TurnId>,
    context_sequence: u64,
    /// The latest projected stretch is a Continuation rather than the captured Prompt.
    context_continuation: bool,
    /// Admission has begun but may fail before delivery; old Continuations cannot
    /// rebind observations to this new (possibly Model-invalidated) Turn.
    context_prompt_pending: bool,
}

/// One stretch of Copilot's loop that Suru reads as a Turn, and the main conversation's state
/// within it.
struct ActiveTurn {
    /// Whether this Turn is a Continuation — a stretch the loop ran on its own, delivering what
    /// a Subagent's completion provoked, rather than one a Prompt began. The next delivered
    /// Prompt settles it rather than steering it (ADR 0015).
    continuation: bool,
    streams: ConversationStreams,
    /// What Copilot reported going wrong inside this Turn, which is what it settles as once the
    /// loop goes idle. The first report wins, because the failures after it are its consequences.
    failure: Option<String>,
}

impl ActiveTurn {
    fn new() -> Self {
        Self {
            continuation: false,
            streams: ConversationStreams::default(),
            failure: None,
        }
    }

    fn continuation() -> Self {
        Self {
            continuation: true,
            ..Self::new()
        }
    }
}

/// One stretch of a Subagent's work still going: how it began, which says what settles it, and what
/// the Subagent's conversation still has open in it.
struct WorkingSubagent {
    stretch: Stretch,
    streams: ConversationStreams,
}

impl WorkingSubagent {
    fn new(stretch: Stretch) -> Self {
        Self {
            stretch,
            streams: ConversationStreams::default(),
        }
    }
}

/// How a working Subagent's stretch began.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stretch {
    /// Its spawn, which Copilot settles itself with `subagent.completed` or `subagent.failed`.
    Spawned,
    /// A resume (ADR 0033), which Copilot opens with the message it delivers and closes with
    /// nothing, so it settles where the agent loop exits: at the `assistant.turn_end` that follows
    /// a model message requesting no tools. `answered` records that the latest model message
    /// requested none, which makes the next turn end the exit.
    Resumed { answered: bool },
}

impl Stretch {
    fn is_resumed(self) -> bool {
        matches!(self, Self::Resumed { .. })
    }

    /// Whether the next turn end is where the stretch's loop exits: only a resume's, and only
    /// once its latest model message requested no tools.
    fn exits_at_turn_end(self) -> bool {
        self == Self::Resumed { answered: true }
    }

    /// Records what the latest model message asked of the loop. A spawn keeps no such record:
    /// Copilot settles it itself.
    fn answer(&mut self, no_tools: bool) {
        if let Self::Resumed { answered } = self {
            *answered = no_tools;
        }
    }
}

/// What the projection keeps of a Subagent past its settle, for the resume that may run it again:
/// the name its rows carry, what its spawn was asked to do — the description of a resume whose
/// message carries nothing to read — and the Model it last ran on, which Copilot reports only at
/// spawn and which carries into the resumed Turn.
struct KnownSubagent {
    name: String,
    description: String,
    model: Option<String>,
}

/// What one conversation on the timeline still has open — the main agent's inside its Turn, or a
/// Subagent's for as long as the Subagent runs.
#[derive(Default)]
struct ConversationStreams {
    message: Option<ActiveMessage>,
    /// The Reasoning blocks the conversation has open, in the order Copilot opened them. Copilot's
    /// loop reasons one block through at a time, so this is a short list rather than a
    /// map, and settling the conversation walks it in the order a reader met it.
    reasoning: Vec<ActiveReasoning>,
    /// The Commands the conversation is still running, by the Tool call identity Copilot gave
    /// each. Copilot runs several Tools at once, so a conversation holds as many as it started.
    commands: HashMap<String, ActiveCommand>,
    /// The cumulative reading for this conversation's one Suru Turn. Copilot
    /// emits one usage event per model call and gives it no Turn identity, so
    /// this state lives exactly as long as the surrounding Turn streams do.
    metering: Option<ReportedTurnMetering>,
}

/// The agent Message Copilot is still streaming, and the text it has carried so far — against
/// which the completed Message's repeat of it is reconciled.
struct ActiveMessage {
    message_id: String,
    streamed: String,
}

/// The tool that spawns a Subagent. Its execution is not work of its own: the Subagent row its
/// `subagent.started` opens is the delegation's representation, so the execution's Command row is
/// withheld rather than showing the reader the same delegation twice.
const SPAWN_TOOL: &str = "task";

/// The tool an Agent sends a message to another agent's loop with — the main agent to a Subagent,
/// or one Subagent to a sibling. Its execution is not work of its own either: what it sends is a
/// Delegation, which stands in the Subagent that receives it where that Subagent received it —
/// as a steer, when Copilot delivers it into the working stretch, or opening the Turn it resumes
/// the settled Subagent into — and never in the sender's Transcript, whose only trace of it is a
/// resume's row, so the execution projects nothing, whatever it came to.
const WRITE_AGENT_TOOL: &str = "write_agent";

/// Whether `started` is a call to one of the Broker's Tools, which Copilot reports as an execution
/// on the MCP server Suru handed the Session the Broker as. Its execution is not work of its own
/// either: the Broker adds whatever row stands for what the call did, so the execution projects
/// nothing, whatever it came to — as a [`WRITE_AGENT_TOOL`] send projects nothing.
fn is_broker_call(started: &ToolExecutionStartData) -> bool {
    started.mcp_server_name.as_deref() == Some(BROKER_SERVER_NAME)
}

/// A Command Copilot is still running, and the output it has streamed so far — against which the
/// completed execution's repeat of it is reconciled.
#[derive(Default)]
struct ActiveCommand {
    streamed_output: String,
    /// The Command header a [`SPAWN_TOOL`] execution would have opened with, held back while the
    /// Subagent row answers for the delegation. A spawn that never opens its Subagent has no row
    /// answering for it, so the withheld Command surfaces when the execution settles or the
    /// conversation stops — a failed delegation stays visible.
    withheld_spawn: Option<String>,
    /// What a [`SPAWN_TOOL`] execution hands the Subagent it spawns — the tool's `prompt`
    /// argument — kept for the `subagent.started` that opens it, which names the spawning tool
    /// call but never carries the prompt itself.
    spawn_prompt: Option<String>,
    /// Whether the execution is a [`WRITE_AGENT_TOOL`] send or a Broker call, which project
    /// nothing at all.
    absorbed: bool,
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
    #[cfg(test)]
    pub(super) fn new() -> Self {
        Self::with_pricing(CopilotPricing::default())
    }

    pub(super) fn with_pricing(pricing: CopilotPricing) -> Self {
        Self {
            turn: None,
            stale_stretches: 0,
            subagents: HashMap::new(),
            spawns: HashMap::new(),
            delegations: HashSet::new(),
            agents: HashMap::new(),
            late_settle_owes_continuation: false,
            watches: HashSet::new(),
            watched_shells: HashSet::new(),
            pricing,
            context_turn: None,
            context_sequence: 0,
            context_continuation: false,
            context_prompt_pending: false,
        }
    }

    /// Opens the Turn a Prompt is about to be delivered into. Copilot hosts one agentic loop per
    /// Session, so a second Turn cannot begin while a prompted one is running — but a Prompt
    /// delivered while a Continuation runs settles that Continuation rather than steering it
    /// (ADR 0015), so the stretch it cuts off becomes a stale one whose idle is owed nothing.
    pub(super) fn begin_turn(&mut self) -> Result<(), ProviderError> {
        match self.turn.take() {
            None => {}
            Some(stretch) if stretch.continuation => {
                // Orchestration already settled the Continuation and the store settles whatever
                // its streams left open, so the stretch's state goes with it.
                self.stale_stretches += 1;
            }
            Some(turn) => {
                self.turn = Some(turn);
                return Err(copilot_error(
                    "Copilot started a Turn while another Turn was active",
                ));
            }
        }
        self.context_continuation = false;
        self.context_prompt_pending = true;
        self.turn = Some(ActiveTurn::new());
        self.late_settle_owes_continuation = false;
        Ok(())
    }

    /// Bind observations at receipt, before the projection queue can fall behind a
    /// later Prompt or Model change. Startup reports during selection still belong
    /// to the previous Turn until the new Prompt is ready to be sent.
    pub(super) fn context_prompt_ready(&mut self, turn_id: TurnId) {
        self.context_prompt_pending = false;
        self.context_turn = Some(turn_id);
        self.context_continuation = false;
    }

    fn context_report(&mut self, event: &SessionEvent) -> Option<AttributedProviderEvent> {
        if event.parsed_type() != SessionEventType::SessionUsageInfo {
            return None;
        }
        let occupied_tokens = event.data.get("currentTokens")?.as_u64()?;
        let turn_id = if event.agent_id.is_some() {
            // Child Turn IDs are assigned by orchestration, under the native agent ID.
            None
        } else {
            Some(self.context_turn?)
        };
        self.context_sequence = self.context_sequence.saturating_add(1);
        Some(attributed(
            event.agent_id.as_deref(),
            ProviderEvent::ContextFill {
                report: ContextFillReport {
                    turn_id,
                    sequence: self.context_sequence,
                    fill: ContextFill {
                        occupied_tokens,
                        // Native tokenLimit is not verified as the raw Model window.
                        // See docs/validation/0299-copilot-context-fill.md.
                        capacity_tokens: None,
                    },
                },
            },
        ))
    }

    /// Gives up the Turn opened by [`Self::begin_turn`] when the Prompt never reached Copilot.
    pub(super) fn abandon_turn(&mut self) {
        self.turn = None;
        // Readiness precedes native.send, which can still reject the Prompt.
        // Neither queued nor later observations may claim that failed delivery.
        self.context_turn = None;
        self.context_continuation = false;
        self.context_prompt_pending = true;
    }

    /// Whether a Turn is running, which is what makes a Prompt delivered now a steer rather than
    /// the start of another Turn, and what there is for an interrupt to stop.
    pub(super) fn is_turn_running(&self) -> bool {
        self.turn.is_some()
    }

    /// Whether main output arriving with no Turn active is owed a Continuation rather than being
    /// stray: some Subagent is still working, a Watch that may wake the loop is still live, or a
    /// Subagent or Watch just settled and its provoked output is still to come.
    fn owes_late_output(&self) -> bool {
        self.late_settle_owes_continuation || !self.subagents.is_empty() || !self.watches.is_empty()
    }

    /// Opens the main conversation's streams to the event in hand: the active Turn's, or the
    /// Continuation stretch that late output begins here — begun is what marks the owed
    /// Continuation delivered. `None` refuses an event outside any Turn, which is dropped.
    fn open_main_streams(&mut self) -> Option<&mut ConversationStreams> {
        if self.turn.is_none() {
            if !self.owes_late_output() {
                return None;
            }
            self.begin_continuation();
        }
        self.turn.as_mut().map(|turn| &mut turn.streams)
    }

    /// Begins the Continuation stretch that main work arriving with no Turn active lands in, which
    /// the loop's next idle settles.
    fn begin_continuation(&mut self) {
        self.turn = Some(ActiveTurn::continuation());
        self.context_continuation = true;
        self.late_settle_owes_continuation = false;
    }
}

/// Where the projection reads the Session's background tasks from when the timeline says they
/// changed: the Copilot Session itself, asked no longer than `request_timeout` — the bound every
/// request Suru makes of the Session's loop waits under — because the timeline projects no
/// further while the read is out.
pub(super) struct TaskRosterSource {
    pub(super) native: Arc<NativeSession>,
    pub(super) request_timeout: Duration,
}

/// Streams the Provider events projected from one Copilot Session's timeline, failing the stream
/// when the shared harness process hosting it dies.
pub(super) fn provider_events(
    subscription: EventSubscription,
    harness: Arc<SharedHarnessHandle<CopilotConnection>>,
    drain: EventDrainCheckpoint,
    correlation: Arc<StdMutex<CopilotCorrelation>>,
    skills: CopilotSkills,
    approvals: Arc<super::approval::CopilotApprovals>,
    roster: TaskRosterSource,
) -> ProviderEventStream {
    // The SDK drops the oldest events on a subscriber that falls behind, and a dropped delta is
    // Transcript content Suru cannot get back, so the timeline is drained as fast as it arrives
    // and queued here rather than at the pace the Session's consumer reads.
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    tokio::spawn(drain_session_timeline(
        subscription,
        events_tx,
        drain.clone(),
        correlation.clone(),
    ));
    Box::pin(stream::unfold(
        CopilotEvents {
            events: events_rx,
            harness,
            drain,
            correlation,
            skills,
            approvals,
            roster,
            pending: VecDeque::new(),
            ended: false,
        },
        next_provider_event,
    ))
}

/// Moves Copilot's timeline off the SDK's bounded subscription as it arrives.
async fn drain_session_timeline(
    mut subscription: EventSubscription,
    events: mpsc::UnboundedSender<Result<TimelineEvent, ProviderError>>,
    drain: EventDrainCheckpoint,
    correlation: Arc<StdMutex<CopilotCorrelation>>,
) {
    loop {
        match subscription.recv().await {
            Ok(event) => {
                let event_id = event.id.clone();
                // Ephemeral context reports use the same lossless drain as durable
                // events, and keep their originating Turn while waiting for projection.
                let context = correlation
                    .lock()
                    .expect("Copilot correlation lock is not poisoned")
                    .context_report(&event);
                let event = match context {
                    Some(context) => TimelineEvent::Context(context),
                    None => TimelineEvent::Native(event),
                };
                if events.send(Ok(event)).is_err() {
                    return;
                }
                drain.delivered(event_id);
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

enum TimelineEvent {
    Native(SessionEvent),
    Context(AttributedProviderEvent),
}

struct CopilotEvents {
    events: mpsc::UnboundedReceiver<Result<TimelineEvent, ProviderError>>,
    harness: Arc<SharedHarnessHandle<CopilotConnection>>,
    drain: EventDrainCheckpoint,
    correlation: Arc<StdMutex<CopilotCorrelation>>,
    skills: CopilotSkills,
    approvals: Arc<super::approval::CopilotApprovals>,
    roster: TaskRosterSource,
    pending: VecDeque<Result<AttributedProviderEvent, ProviderError>>,
    ended: bool,
}

async fn next_provider_event(
    mut events: CopilotEvents,
) -> Option<(
    Result<AttributedProviderEvent, ProviderError>,
    CopilotEvents,
)> {
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
                events.drain.wait_until_drained().await;
                while let Ok(received) = events.events.try_recv() {
                    match received {
                        Ok(event) => queue_projected(&mut events, event),
                        Err(error) => events.pending.push_back(Err(error)),
                    }
                }
                let lost = events
                    .correlation
                    .lock()
                    .expect("Copilot correlation lock is not poisoned")
                    .project_watches_lost();
                events.pending.extend(lost.into_iter().map(Ok));
                events.pending.push_back(Err(crashed));
                events.ended = true;
                continue;
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
                if let TimelineEvent::Native(event) = &event
                    && matches!(
                        event.parsed_type(),
                        SessionEventType::SessionIdle
                            | SessionEventType::SubagentCompleted
                            | SessionEventType::SubagentFailed
                    )
                {
                    events.approvals.wait_for_settled_decision().await;
                }
                let roster_changed = matches!(
                    &event,
                    TimelineEvent::Native(event)
                        if event.parsed_type() == SessionEventType::SessionBackgroundTasksChanged
                );
                queue_projected(&mut events, event);
                if roster_changed {
                    read_task_roster(&mut events).await;
                }
            }
        }
    }
}

/// Reads the Session's background tasks the moment the timeline says they changed, and starts a
/// Watch for each detached shell the read finds newly running.
///
/// `session.background_tasks_changed` carries no payload, so the roster Copilot's own task view
/// refreshes on it (`session.tasks.list`) is where a shell's attachment mode is recorded — and the
/// identity it lists the shell under is the `shellId` the shell's completion notification names,
/// so the Watch's start and settle agree on it. The timeline waits on the read, which puts the
/// Watch's start ahead of the idle that follows it: the Session reads Monitoring from the Turn's
/// settle rather than idle for a moment first. A read that fails costs the Session its Monitoring
/// reading, not the Session: the shell's completion still records its Watch and wakes the loop
/// into a Continuation headed by the Watch Outcome.
///
/// Why the roster rather than the tool call (github-copilot-sdk 1.0.15-preview.3; the CLI wire as
/// captured on 1.0.82):
/// - `ToolExecutionStartData` carries the shell tool's `arguments` — `detach: true` beside
///   `mode: "async"` is how the Model asks for a detached shell — but no shell identity, and
///   `ToolExecutionStartShellToolInfo` only path hints and a display command.
/// - The tool's result names the shell only in text addressed to the Model (`<command started in
///   detached background with shellId: …>`); the one structured content carrying a `shellId`,
///   `ToolExecutionCompleteContentShellExit`, needs an exit code a detached start never has.
/// - `SessionBackgroundTasksChangedData` is empty: it says only that the roster changed.
/// - `session.tasks.list` (`SessionRpcTasks::list`) answers `TaskShellInfo` entries whose `id`
///   is the shell identity the completion notification names, and whose `attachment_mode`
///   (`TaskShellInfoAttachmentMode::Detached`) says outright what the tool arguments only
///   request. The Rust SDK leaves `SystemNotificationData.kind` untyped; the
///   `shell_detached_completed { shellId, description? }` shape is the CLI's wire, as issues
///   #380 and #387 record it (and as the Node and .NET SDKs' generated notification kinds type
///   it).
async fn read_task_roster(events: &mut CopilotEvents) {
    const CONTEXT: &str = "Copilot task roster read failed";
    let listed = timeout(
        events.roster.request_timeout,
        until_crash(
            &events.harness,
            CONTEXT,
            events.roster.native.rpc().tasks().list(),
        ),
    )
    .await;
    let tasks = match listed {
        Ok(Ok(listed)) => listed.tasks,
        Ok(Err(error)) => {
            tracing::warn!("{error}");
            return;
        }
        Err(_) => {
            tracing::warn!(
                "{CONTEXT}: {COPILOT_HARNESS_NAME} timed out handling `session.tasks.list`"
            );
            return;
        }
    };
    let started = events
        .correlation
        .lock()
        .expect("Copilot correlation lock is not poisoned")
        .project_task_roster(&tasks);
    events.pending.extend(started.into_iter().map(Ok));
}

fn queue_projected(events: &mut CopilotEvents, event: TimelineEvent) {
    let event = match event {
        TimelineEvent::Context(mut context) => {
            let correlation = events
                .correlation
                .lock()
                .expect("Copilot correlation lock is not poisoned");
            if context.attribution == ProviderEventAttribution::OwningSession
                && correlation.context_turn.is_none()
            {
                // Startup may have failed after this observation entered the drain.
                return;
            }
            if let ProviderEvent::ContextFill { report } = &mut context.event
                && context.attribution == ProviderEventAttribution::OwningSession
                && !correlation.context_prompt_pending
                && correlation.context_continuation
                && report.turn_id == correlation.context_turn
            {
                // Preceding native content opened a Continuation in this same Prompt
                // generation. Its Turn ID belongs to orchestration, so bind in stream
                // order. A queued report from an older Prompt retains its explicit ID.
                report.turn_id = None;
            }
            events.pending.push_back(Ok(context));
            return;
        }
        TimelineEvent::Native(event) => event,
    };
    match event.parsed_type() {
        SessionEventType::PermissionRequested => {
            if let Some(request_id) = event.data["requestId"]
                .as_str()
                .map(github_copilot_sdk::RequestId::new)
            {
                let attribution = event
                    .agent_id
                    .as_deref()
                    .map_or(ProviderEventAttribution::OwningSession, |agent| {
                        ProviderEventAttribution::Subagent(ProviderSubagentId::new(agent))
                    });
                events.approvals.observe(request_id, attribution);
            }
        }
        SessionEventType::PermissionCompleted => {
            if let Some(request_id) = event.data["requestId"]
                .as_str()
                .map(github_copilot_sdk::RequestId::new)
                && let Some(withdrawn) = events.approvals.complete(&request_id)
            {
                events.pending.push_back(Ok(withdrawn));
            }
        }
        _ => {}
    }
    if matches!(
        event.parsed_type(),
        SessionEventType::CommandsChanged | SessionEventType::SessionSkillsLoaded
    ) {
        events.skills.native_catalog_changed();
    }
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

/// The attribution one conversation's events land under.
fn attributed(subagent: Option<&str>, event: ProviderEvent) -> AttributedProviderEvent {
    AttributedProviderEvent {
        attribution: match subagent {
            None => ProviderEventAttribution::OwningSession,
            Some(subagent) => ProviderEventAttribution::Subagent(ProviderSubagentId::new(subagent)),
        },
        event,
    }
}

fn project_session_event(
    correlation: &mut CopilotCorrelation,
    event: SessionEvent,
) -> Result<Vec<AttributedProviderEvent>, ProviderError> {
    match event.parsed_type() {
        // The Subagent lifecycle is read before the envelope's attribution: it opens and settles
        // the Subagents themselves, whichever conversation's context Copilot stamped on it.
        SessionEventType::SubagentStarted => {
            return Ok(correlation.project_subagent_started(&event));
        }
        SessionEventType::SubagentCompleted => {
            return Ok(reported(&event).map_or_else(
                Vec::new,
                |completed: SubagentCompletedData| {
                    // A torn-down Subagent still reports completion, distinguished only by the
                    // flag; a delegation that never ran to the end did not do its work.
                    let status = if completed.cancelled.unwrap_or(false) {
                        ProviderSubagentStatus::Failed
                    } else {
                        ProviderSubagentStatus::Completed
                    };
                    correlation.project_subagent_settled(&completed.tool_call_id, status)
                },
            ));
        }
        SessionEventType::SubagentFailed => {
            return Ok(
                reported(&event).map_or_else(Vec::new, |failed: SubagentFailedData| {
                    correlation.project_subagent_settled(
                        &failed.tool_call_id,
                        ProviderSubagentStatus::Failed,
                    )
                }),
            );
        }
        // A notification wakes the main loop whichever conversation's context Copilot stamped
        // on it, so it is read before the envelope's attribution too.
        SessionEventType::SystemNotification => {
            return Ok(reported(&event).map_or_else(
                Vec::new,
                |notification: SystemNotificationData| {
                    correlation.project_system_notification(&notification)
                },
            ));
        }
        _ => {}
    }
    // An event the envelope attributes to a Subagent lands in that Subagent's own streams,
    // whether or not the main conversation has a Turn open — a Subagent outlives the Turn that
    // spawned it (ADR 0015). One attributed to an instance no Subagent holds lands nowhere.
    if let Some(subagent) = event.agent_id.clone() {
        match event.parsed_type() {
            SessionEventType::UserMessage => {
                return Ok(correlation.project_subagent_message(&subagent, &event));
            }
            SessionEventType::AssistantTurnEnd => {
                return Ok(correlation.project_subagent_turn_end(&subagent));
            }
            _ => {}
        }
        let Some(working) = correlation.subagents.get_mut(&subagent) else {
            return Ok(Vec::new());
        };
        if event.parsed_type() == SessionEventType::AssistantMessage {
            let no_tools = reported::<AssistantMessageData>(&event).is_some_and(|message| {
                message
                    .tool_requests
                    .is_none_or(|requests| requests.is_empty())
            });
            working.stretch.answer(no_tools);
        }
        let streams = &mut working.streams;
        if event.parsed_type() == SessionEventType::AssistantUsage {
            let model = reported::<AssistantUsageData>(&event)
                .and_then(|usage| (!usage.model.is_empty()).then_some(usage.model));
            let mut projected = Vec::new();
            if let Some(model) = model {
                if let Some(known) = correlation.agents.get_mut(&subagent) {
                    known.model = Some(model.clone());
                }
                projected.push(attributed(
                    None,
                    ProviderEvent::SubagentModelChanged {
                        subagent_id: ProviderSubagentId::new(subagent.clone()),
                        model: crate::protocol::ModelId::new(model),
                    },
                ));
            }
            projected.push(attributed(
                Some(&subagent),
                project_usage_event(streams, &event, &correlation.pricing)?,
            ));
            return Ok(projected);
        }
        return Ok(
            project_conversation_event(streams, &mut correlation.delegations, &event)?
                .into_iter()
                .map(|projected| attributed(Some(&subagent), projected))
                .collect(),
        );
    }
    match event.parsed_type() {
        // A transient error is one Copilot's own loop recovers from by retrying, so it is not the
        // Turn's outcome and stays out of the Transcript.
        SessionEventType::SessionError if event.is_transient_error() => Ok(Vec::new()),
        SessionEventType::SessionError => {
            let failure: SessionErrorData = decode(&event)?;
            correlation.project_session_error(&failure);
            Ok(Vec::new())
        }
        SessionEventType::SessionIdle => {
            let idle: SessionIdleData = decode(&event)?;
            let aborted = idle.aborted.unwrap_or(false);
            let mut projected = correlation.project_resumes_stopped(aborted);
            projected.extend(
                correlation
                    .project_session_idle(aborted)
                    .into_iter()
                    .map(|projected| attributed(None, projected)),
            );
            Ok(projected)
        }
        SessionEventType::AssistantUsage => {
            if correlation.open_main_streams().is_none() {
                return Ok(Vec::new());
            }
            let turn = correlation
                .turn
                .as_mut()
                .expect("usage opened or found the main conversation's Turn");
            Ok(vec![attributed(
                None,
                project_usage_event(&mut turn.streams, &event, &correlation.pricing)?,
            )])
        }
        // Content the Transcript presents belongs to a Turn, so late content — like the error
        // above — opens a Continuation stretch for what it carries; everything else on the
        // timeline is passed over rather than opening a stretch it would put nothing in.
        event_type if is_conversation_content(&event_type) => {
            if correlation.open_main_streams().is_none() {
                return Ok(Vec::new());
            }
            let turn = correlation
                .turn
                .as_mut()
                .expect("the main conversation's streams live inside its Turn");
            Ok(
                project_conversation_event(
                    &mut turn.streams,
                    &mut correlation.delegations,
                    &event,
                )?
                .into_iter()
                .map(|projected| attributed(None, projected))
                .collect(),
            )
        }
        _ => Ok(Vec::new()),
    }
}

fn project_usage_event(
    streams: &mut ConversationStreams,
    event: &SessionEvent,
    pricing: &CopilotPricing,
) -> Result<ProviderEvent, ProviderError> {
    let cache_ttl_seconds = reported_cache_ttl(event);
    let reported: AssistantUsageData = decode(event)?;
    let usage = Usage {
        fresh_input_tokens: exclusive_count(
            reported.input_tokens,
            [reported.cache_read_tokens, reported.cache_write_tokens],
        ),
        cache_read_tokens: reported_count(reported.cache_read_tokens),
        cache_write_tokens: reported_count(reported.cache_write_tokens),
        output_tokens: exclusive_count(reported.output_tokens, [reported.reasoning_tokens]),
        reasoning_tokens: reported_count(reported.reasoning_tokens),
        native_meter: reported.cost.and_then(NativeMeter::from_units),
        model_context_window: reported_context_window(
            reported.max_prompt_tokens,
            reported.max_output_tokens,
        ),
    };
    let reported_cost = pricing.cost(
        &reported.model,
        reported.max_prompt_tokens,
        cache_ttl_seconds,
        &usage,
    );
    Ok(match streams.metering.as_mut() {
        Some(metering) => {
            metering.add(usage, reported_cost);
            metering.event()
        }
        None => {
            let metering = ReportedTurnMetering::new(usage, reported_cost);
            let event = metering.event();
            streams.metering = Some(metering);
            event
        }
    })
}

fn reported_context_window(prompt: Option<i64>, output: Option<i64>) -> Option<u64> {
    reported_count(prompt)?.checked_add(reported_count(output)?)
}

fn reported_cache_ttl(event: &SessionEvent) -> Option<i64> {
    // The SDK receives this protocol field but does not expose it publicly;
    // retain it from the event payload so the catalog's one-hour write price
    // can be selected when it is the applicable published rate.
    event.data.get("cacheTtlSeconds")?.as_i64()
}

/// Whether an event carries conversation content this projection presents.
fn is_conversation_content(event_type: &SessionEventType) -> bool {
    matches!(
        event_type,
        SessionEventType::AssistantMessageStart
            | SessionEventType::AssistantMessageDelta
            | SessionEventType::AssistantMessage
            | SessionEventType::ToolExecutionStart
            | SessionEventType::ToolExecutionPartialResult
            | SessionEventType::ToolExecutionComplete
            | SessionEventType::AssistantReasoningDelta
            | SessionEventType::AssistantReasoning
    )
}

/// Projects one conversation's content event — the main agent's inside its Turn, or a Subagent's
/// for as long as it runs — onto the Provider events that carry it. `delegations` is the roster
/// of spawning tool calls whose Subagent row already answers for them, which is what tells a
/// withheld spawn's settle apart from a delegation that never happened.
fn project_conversation_event(
    streams: &mut ConversationStreams,
    delegations: &mut HashSet<String>,
    event: &SessionEvent,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    match event.parsed_type() {
        SessionEventType::AssistantMessageStart => {
            let started: AssistantMessageStartData = decode(event)?;
            project_message_started(streams, started.message_id)
        }
        SessionEventType::AssistantMessageDelta => {
            let delta: AssistantMessageDeltaData = decode(event)?;
            Ok(project_message_delta(
                streams,
                &delta.message_id,
                delta.delta_content,
            ))
        }
        SessionEventType::AssistantMessage => {
            let message: AssistantMessageData = decode(event)?;
            project_message_completed(streams, &message.message_id, &message.content)
        }
        SessionEventType::ToolExecutionStart => Ok(reported(event)
            .map_or_else(Vec::new, |started: ToolExecutionStartData| {
                project_command_started(streams, &started)
            })),
        SessionEventType::ToolExecutionPartialResult => Ok(reported(event).map_or_else(
            Vec::new,
            |output: ToolExecutionPartialResultData| {
                project_command_output(streams, &output.tool_call_id, output.partial_output)
            },
        )),
        SessionEventType::ToolExecutionComplete => Ok(reported(event).map_or_else(
            Vec::new,
            |completed: ToolExecutionCompleteData| {
                project_command_completed(streams, delegations, &completed)
            },
        )),
        SessionEventType::AssistantReasoningDelta => Ok(reported(event).map_or_else(
            Vec::new,
            |delta: AssistantReasoningDeltaData| {
                project_reasoning_delta(streams, &delta.reasoning_id, &delta.delta_content)
            },
        )),
        SessionEventType::AssistantReasoning => Ok(reported(event).map_or_else(
            Vec::new,
            |reasoning: AssistantReasoningData| {
                project_reasoning_completed(streams, &reasoning.reasoning_id, &reasoning.content)
            },
        )),
        _ => Ok(Vec::new()),
    }
}

impl CopilotCorrelation {
    /// Opens the Subagent a `subagent.started` reports, in the conversation that spawned it. The
    /// envelope's instance identity is what the Subagent's every event is attributed with; a
    /// started that carries none leaves the spawning tool call standing in, so the Subagent's row
    /// and settle still reach the Transcript even though no work can ever be attributed to it.
    /// The spawn's Delegation is the prompt its tool call's execution carried, where that
    /// execution started first; the runtime's own delegation, which never surfaces as one, opens
    /// with none.
    fn project_subagent_started(&mut self, event: &SessionEvent) -> Vec<AttributedProviderEvent> {
        let Some(started) = reported::<SubagentStartedData>(event) else {
            return Vec::new();
        };
        let subagent = event
            .agent_id
            .clone()
            .unwrap_or_else(|| started.tool_call_id.clone());
        if self.subagents.contains_key(&subagent) {
            return Vec::new();
        }
        // A spawn out of a Subagent's own conversation — the spawning tool call is one that
        // Subagent is running — recurses one level down; every other spawn is the main agent's,
        // and lands only inside a Turn Suru is running or the Continuation late work opens.
        let spawner = self.spawning_conversation(&started.tool_call_id);
        if spawner.is_none() && self.open_main_streams().is_none() {
            return Vec::new();
        }
        self.subagents
            .insert(subagent.clone(), WorkingSubagent::new(Stretch::Spawned));
        self.spawns
            .insert(started.tool_call_id.clone(), subagent.clone());
        self.delegations.insert(started.tool_call_id.clone());
        // Copilot's display name is the readable one, but a spawn made through its task tool
        // writes the invocation's description there, leaving the configured name the stable one.
        let name = if started.agent_display_name.is_empty() {
            started.agent_name
        } else {
            started.agent_display_name
        };
        let model = started.model.filter(|model| !model.is_empty());
        self.agents.insert(
            subagent.clone(),
            KnownSubagent {
                name: name.clone(),
                description: started.agent_description.clone(),
                model: model.clone(),
            },
        );
        let delegation = self.spawn_prompt(spawner.as_deref(), &started.tool_call_id);
        let mut projected = vec![attributed(
            spawner.as_deref(),
            ProviderEvent::SubagentStarted {
                subagent_id: ProviderSubagentId::new(subagent.clone()),
                name,
                description: started.agent_description,
                delegation,
            },
        )];
        if let Some(model) = model {
            projected.push(attributed(
                spawner.as_deref(),
                ProviderEvent::SubagentModelChanged {
                    subagent_id: ProviderSubagentId::new(subagent),
                    model: crate::protocol::ModelId::new(model),
                },
            ));
        }
        projected
    }

    /// The prompt the spawning tool call handed its Subagent, read off its withheld execution in
    /// the conversation that ran it — a Subagent's own, or the main agent's.
    fn spawn_prompt(&self, spawner: Option<&str>, tool_call_id: &str) -> Option<String> {
        let streams = match spawner {
            Some(subagent) => &self.subagents.get(subagent)?.streams,
            None => &self.turn.as_ref()?.streams,
        };
        streams.commands.get(tool_call_id)?.spawn_prompt.clone()
    }

    /// The Subagent whose conversation ran `tool_call_id`, or `None` for the main agent's own —
    /// which is also the answer for a spawn whose tool call never surfaced as an execution, the
    /// shape the runtime's own delegation takes.
    fn spawning_conversation(&self, tool_call_id: &str) -> Option<String> {
        self.subagents
            .iter()
            .find(|(_, working)| working.streams.commands.contains_key(tool_call_id))
            .map(|(subagent, _)| subagent.clone())
    }

    /// Reads a message Copilot delivered to a Subagent's own loop, which Copilot reports when the
    /// Subagent consumes it. Whoever sent it, it is attributed to the Agent `source` names —
    /// `agent-<id>` names a sibling Subagent by its instance identity, and every other origin, the
    /// main agent's `agent-<session id>` among them, is the owning Session's.
    ///
    /// One delivered into the stretch a working Subagent is in (`delivery: "steering"`) is a
    /// steer: a Delegation standing at the point the Subagent received it, which is where this
    /// event arrives (ADR 0032). The CLIs verified against never deliver a `write_agent` that way
    /// (docs/validation/0397-copilot-subagent-steer.md).
    ///
    /// Any other message a working spawn receives is its own prompt, which the spawning tool call
    /// already carried in as the Delegation opening its Turn: Copilot holds every later send back
    /// until the stretch ends, so no other message reaches a spawn while it works. Every message a settled Subagent consumes runs it again on its own context — whatever
    /// its `delivery`: `queued` when it was sent during the stretch and held until that stretch's
    /// `subagent.completed`, `idle` when it was sent afterwards — so it is a resume (ADR 0033),
    /// opening the new Turn as its Delegation ([`Self::project_resumed`]). Its arrival is also
    /// proof the previous run ended, so it settles a resume of that Subagent still open before it
    /// begins the next. A message for an instance no Subagent holds stands nowhere.
    fn project_subagent_message(
        &mut self,
        subagent: &str,
        event: &SessionEvent,
    ) -> Vec<AttributedProviderEvent> {
        let Some(message) = reported::<UserMessageData>(event) else {
            return Vec::new();
        };
        // A Subagent that queued a copy of its own send to itself is not its own sender: the
        // resume that copy begins is the owning Session's.
        let sender = message
            .source
            .as_deref()
            .and_then(|source| source.strip_prefix("agent-"))
            .filter(|sender| *sender != subagent && self.agents.contains_key(*sender))
            .map(str::to_owned);
        match self.subagents.get(subagent).map(|working| working.stretch) {
            Some(_) if message.delivery == Some(UserMessageDelivery::Steering) => {
                if message.content.trim().is_empty() {
                    return Vec::new();
                }
                vec![attributed(
                    sender.as_deref(),
                    ProviderEvent::SubagentSteered {
                        subagent_id: ProviderSubagentId::new(subagent),
                        delegation: message.content,
                    },
                )]
            }
            Some(Stretch::Spawned) => Vec::new(),
            Some(Stretch::Resumed { .. }) => {
                let mut projected =
                    self.project_stretch_settled(subagent, ProviderSubagentStatus::Completed);
                projected.extend(self.project_resumed(subagent, sender, message.content));
                projected
            }
            None => self.project_resumed(subagent, sender, message.content),
        }
    }

    /// Resumes a settled Subagent with the message it consumed (ADR 0033): routes its work to it
    /// again, in a stretch its loop's exit settles, and reports the resume from the Agent that
    /// sent it, the message opening the resumed Turn as its Delegation and its first line
    /// describing the stretch's row. The Model the Subagent last ran on carries into that Turn,
    /// because Copilot reports a Subagent's Model only at spawn. A resume the owning Session
    /// delegates while no Turn is running begins a Continuation to hold its row, which the loop's
    /// next idle settles. An instance no Subagent ever held has nothing to resume.
    fn project_resumed(
        &mut self,
        subagent: &str,
        sender: Option<String>,
        content: String,
    ) -> Vec<AttributedProviderEvent> {
        let Some(known) = self.agents.get(subagent) else {
            return Vec::new();
        };
        let delegation = (!content.trim().is_empty()).then_some(content);
        let description = delegation
            .as_deref()
            .and_then(first_line)
            .unwrap_or_else(|| known.description.clone());
        let (name, model) = (known.name.clone(), known.model.clone());
        if sender.is_none() && self.turn.is_none() {
            self.begin_continuation();
        }
        self.subagents.insert(
            subagent.to_owned(),
            WorkingSubagent::new(Stretch::Resumed { answered: false }),
        );
        let mut projected = vec![attributed(
            sender.as_deref(),
            ProviderEvent::SubagentResumed {
                subagent_id: ProviderSubagentId::new(subagent),
                name,
                description,
                delegation,
            },
        )];
        if let Some(model) = model {
            projected.push(attributed(
                None,
                ProviderEvent::SubagentModelChanged {
                    subagent_id: ProviderSubagentId::new(subagent),
                    model: crate::protocol::ModelId::new(model),
                },
            ));
        }
        projected
    }

    /// Settles a resumed Subagent's stretch where its agent loop exits: the turn end following a
    /// model message that requested no tools. Like any Subagent settle, the output it provokes in
    /// the loop that reads it arrives only afterwards, so it leaves a Continuation owed to it. A
    /// spawn's turn ends settle nothing: Copilot settles a spawn itself.
    fn project_subagent_turn_end(&mut self, subagent: &str) -> Vec<AttributedProviderEvent> {
        let exits = self
            .subagents
            .get(subagent)
            .is_some_and(|working| working.stretch.exits_at_turn_end());
        if !exits {
            return Vec::new();
        }
        self.late_settle_owes_continuation = true;
        self.project_stretch_settled(subagent, ProviderSubagentStatus::Completed)
    }

    /// Settles every resumed stretch still open when the loop goes idle — interrupted, when the
    /// idle is an abort's — since Copilot defers the idle while any agent runs. Spawns are
    /// Copilot's own to settle.
    fn project_resumes_stopped(&mut self, aborted: bool) -> Vec<AttributedProviderEvent> {
        let mut resumed = self
            .subagents
            .iter()
            .filter(|(_, working)| working.stretch.is_resumed())
            .map(|(subagent, _)| subagent.clone())
            .collect::<Vec<_>>();
        resumed.sort_unstable();
        let status = if aborted {
            ProviderSubagentStatus::Interrupted
        } else {
            ProviderSubagentStatus::Completed
        };
        resumed
            .iter()
            .flat_map(|subagent| self.project_stretch_settled(subagent, status))
            .collect()
    }

    /// Settles the Subagent the spawning tool call names. A settle for a spawn never opened lands
    /// nowhere, like any other unrouted Subagent event.
    fn project_subagent_settled(
        &mut self,
        tool_call_id: &str,
        status: ProviderSubagentStatus,
    ) -> Vec<AttributedProviderEvent> {
        let Some(subagent) = self.spawns.remove(tool_call_id) else {
            return Vec::new();
        };
        // The output this completion provokes arrives only after the settle, so the settle
        // leaves a Continuation owed to it (ADR 0015).
        self.late_settle_owes_continuation = true;
        self.project_stretch_settled(&subagent, status)
    }

    /// Settles the stretch a Subagent is working, closing whatever its streams leave open — the
    /// split-held Reasoning title above all — before the settle itself drops the routes.
    fn project_stretch_settled(
        &mut self,
        subagent: &str,
        status: ProviderSubagentStatus,
    ) -> Vec<AttributedProviderEvent> {
        let mut streams = self
            .subagents
            .remove(subagent)
            .map(|working| working.streams)
            .unwrap_or_default();
        let mut projected: Vec<AttributedProviderEvent> =
            settle_open_streams(&mut streams, &self.delegations)
                .into_iter()
                .map(|event| attributed(Some(subagent), event))
                .collect();
        projected.push(attributed(
            None,
            ProviderEvent::SubagentCompleted {
                subagent_id: ProviderSubagentId::new(subagent),
                status,
            },
        ));
        projected
    }

    /// Starts a Watch for every detached shell the Session's task roster lists as running that is
    /// not one already. Only a detached shell is a Watch: its tool call returns at once, the loop
    /// goes idle without waiting on it, and its completion is delivered back to the loop. An
    /// attached shell holds the idle back instead, keeping its Turn open (#379), and an agent task
    /// is a Subagent. A shell listed already ended is left alone: there is nothing left to wait
    /// on, and the completion notification it raises records its Watch whole.
    ///
    /// The roster's entries are the SDK's `TaskShellInfo` and `TaskAgentInfo`, told apart by
    /// their `type`, which is why an entry that is not a shell this build can read is passed
    /// over — with a warning, since a shell entry that no longer parses is the CLI drifting from
    /// the SDK — rather than failing the read.
    pub(super) fn project_task_roster(
        &mut self,
        tasks: &[serde_json::Value],
    ) -> Vec<AttributedProviderEvent> {
        tasks
            .iter()
            .filter(|task| task.get("type").and_then(serde_json::Value::as_str) == Some("shell"))
            .filter_map(|task| {
                serde_json::from_value::<TaskShellInfo>(task.clone())
                    .inspect_err(|error| {
                        tracing::warn!(
                            "passed over a Copilot task roster shell Suru could not read: {error}"
                        );
                    })
                    .ok()
            })
            .filter(|shell| {
                shell.attachment_mode == TaskShellInfoAttachmentMode::Detached
                    && matches!(shell.status, TaskStatus::Running | TaskStatus::Idle)
                    && !shell.id.is_empty()
            })
            .filter_map(|shell| {
                if !self.watched_shells.insert(shell.id.clone()) {
                    return None;
                }
                self.watches.insert(shell.id.clone());
                let description = [shell.description, shell.command]
                    .into_iter()
                    .find(|text| !text.trim().is_empty())
                    .unwrap_or_else(|| shell.id.clone());
                Some(attributed(
                    None,
                    ProviderEvent::WatchStarted {
                        watch_id: ProviderWatchId::new(shell.id),
                        description,
                    },
                ))
            })
            .collect()
    }

    /// Reads a notification Copilot delivers to the main loop. The one this projection acts on is
    /// a detached shell completing (`kind.type: "shell_detached_completed"`, naming the shell by
    /// `shellId`): it settles the shell's Watch as completed — the kind carries no exit code, so
    /// completed is all it says — with the notification's text, less the `<system_notification>`
    /// wrapper the loop reads it in, as Copilot's account of how it settled.
    ///
    /// The notification is what wakes the loop, and orchestration owes the woken output a
    /// Continuation only through the settle of a Watch it knows. So a shell never seen as a Watch
    /// — its roster read failed, or found it already finished because it ran shorter than the
    /// round trip — starts its Watch here, described in the notification's own words, and
    /// settles it at once: the woken work is still shown, headed by how the shell settled. A
    /// shell whose Watch already settled — stopped by an interrupt, which records nothing (ADR
    /// 0030) — wakes nothing Suru shows. While a Turn is running the woken work lands in it and
    /// nothing is owed. Every other kind wakes nothing this projection follows.
    fn project_system_notification(
        &mut self,
        notification: &SystemNotificationData,
    ) -> Vec<AttributedProviderEvent> {
        let kind = &notification.kind;
        if kind.get("type").and_then(serde_json::Value::as_str) != Some("shell_detached_completed")
        {
            return Vec::new();
        }
        let Some(shell) = kind
            .get("shellId")
            .and_then(serde_json::Value::as_str)
            .filter(|shell| !shell.is_empty())
        else {
            return Vec::new();
        };
        let mut projected = Vec::with_capacity(2);
        if self.watched_shells.insert(shell.to_owned()) {
            let description = kind
                .get("description")
                .and_then(serde_json::Value::as_str)
                .filter(|description| !description.trim().is_empty())
                .unwrap_or(shell)
                .to_owned();
            projected.push(attributed(
                None,
                ProviderEvent::WatchStarted {
                    watch_id: ProviderWatchId::new(shell),
                    description,
                },
            ));
        } else if !self.watches.remove(shell) {
            return Vec::new();
        }
        if self.turn.is_none() {
            self.late_settle_owes_continuation = true;
        }
        projected.push(attributed(
            None,
            ProviderEvent::WatchSettled {
                watch_id: ProviderWatchId::new(shell),
                outcome: ProviderWatchOutcome::Completed,
                summary: notification_text(&notification.content),
                woke_agent: true,
            },
        ));
        projected
    }

    /// Those of `watches` still live, which are all a stop has left to stop.
    pub(super) fn live_watches(&self, watches: &[ProviderWatchId]) -> Vec<String> {
        watches
            .iter()
            .map(ProviderWatchId::as_str)
            .filter(|shell| self.watches.contains(*shell))
            .map(str::to_owned)
            .collect()
    }

    /// Detached shells the Session stopped and Copilot confirmed cancelling. Each still a live
    /// Watch settles as stopped, waking nothing, so the Session stops Monitoring on the
    /// confirmation alone; a completion notification arriving afterwards finds the Watch already
    /// settled and repeats nothing.
    pub(super) fn project_watches_stopped(
        &mut self,
        shells: &[String],
    ) -> Vec<AttributedProviderEvent> {
        shells
            .iter()
            .filter(|shell| self.watches.remove(*shell))
            .map(|shell| {
                attributed(
                    None,
                    ProviderEvent::WatchSettled {
                        watch_id: ProviderWatchId::new(shell.clone()),
                        outcome: ProviderWatchOutcome::Stopped,
                        summary: None,
                        woke_agent: false,
                    },
                )
            })
            .collect()
    }

    /// The harness process hosting the Session died. A detached shell outlives it, but nothing is
    /// left to deliver its completion to the loop, so it can wake nothing: every live Watch
    /// settles as lost.
    fn project_watches_lost(&mut self) -> Vec<AttributedProviderEvent> {
        let mut lost = std::mem::take(&mut self.watches)
            .into_iter()
            .collect::<Vec<_>>();
        lost.sort_unstable();
        lost.into_iter()
            .map(|shell| {
                attributed(
                    None,
                    ProviderEvent::WatchSettled {
                        watch_id: ProviderWatchId::new(shell),
                        outcome: ProviderWatchOutcome::Lost,
                        summary: None,
                        woke_agent: false,
                    },
                )
            })
            .collect()
    }

    /// Records what the Turn will settle as when Copilot reports an error — authentication,
    /// quota, rate limit, and the rest — naming the category Copilot typed it as, so a remote
    /// failure reads without opening the Log. The loop stopping is what settles the Turn on it.
    fn project_session_error(&mut self, failure: &SessionErrorData) {
        if self.open_main_streams().is_none() {
            return;
        }
        let turn = self
            .turn
            .as_mut()
            .expect("the main conversation's streams live inside its Turn");
        let kind = failure.error_type.replace(['_', '-'], " ");
        turn.failure.get_or_insert_with(|| {
            concise_remote_message(
                &format!("Copilot {kind} error: {}", failure.message),
                COPILOT_FAILURE_FALLBACK,
            )
        });
    }

    /// Settles the Turn on the signal that Copilot's agentic loop has stopped: the stretch is the
    /// Turn, so its idle is the Turn's outcome — whatever the loop met on the way there, and an
    /// idle the abort produced is an interruption. The idle of a stale stretch — a Continuation
    /// the next Prompt already settled — is owed nothing and settles nothing.
    fn project_session_idle(&mut self, aborted: bool) -> Vec<ProviderEvent> {
        if self.stale_stretches > 0 {
            self.stale_stretches -= 1;
            return Vec::new();
        }
        let Some(mut turn) = self.turn.take() else {
            return Vec::new();
        };
        // A loop that stops mid-Message or mid-block leaves both settled rather than running
        // forever.
        let mut projected = settle_open_streams(&mut turn.streams, &self.delegations);
        projected.push(match (turn.failure, aborted) {
            (Some(message), _) => ProviderEvent::TurnFailed { message },
            (None, true) => ProviderEvent::TurnInterrupted,
            (None, false) => ProviderEvent::TurnCompleted,
        });
        projected
    }
}

/// The text of a notification as a reader should see it: Copilot hands the loop its
/// notifications wrapped in `<system_notification>` tags, which are addressed to the Model rather
/// than to anyone reading the Transcript.
fn notification_text(content: &str) -> Option<String> {
    let text = content.trim();
    let text = text
        .strip_prefix("<system_notification>")
        .and_then(|text| text.strip_suffix("</system_notification>"))
        .unwrap_or(text)
        .trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn project_message_started(
    streams: &mut ConversationStreams,
    message_id: String,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    if streams.message.is_some() {
        return Err(copilot_error(
            "Copilot started a second agent Message before completing the first",
        ));
    }
    streams.message = Some(ActiveMessage {
        message_id,
        streamed: String::new(),
    });
    Ok(vec![ProviderEvent::AgentMessageStarted])
}

/// Streams the next of the agent Message into the Transcript, opening the Message on the first
/// delta when Copilot never started it: a Subagent's Message streams with no start of its own,
/// announcing itself by being written into the way a Reasoning block does. The main
/// conversation's Messages do get starts, but a delta is Transcript content either way, so every
/// conversation opens on whichever of the two arrives first rather than losing the Turn to a
/// missing announcement.
fn project_message_delta(
    streams: &mut ConversationStreams,
    message_id: &str,
    delta: String,
) -> Vec<ProviderEvent> {
    let mut projected = Vec::with_capacity(2);
    if streams.message.is_none() {
        streams.message = Some(ActiveMessage {
            message_id: message_id.to_owned(),
            streamed: String::new(),
        });
        projected.push(ProviderEvent::AgentMessageStarted);
    }
    let message = streams
        .message
        .as_mut()
        .expect("the Message is open, having been opened just now when it was not");
    if message.message_id != message_id {
        return Vec::new();
    }
    message.streamed.push_str(&delta);
    projected.push(ProviderEvent::AgentMessageDelta { content: delta });
    projected
}

/// Completes the streaming Message, or stands in for one Copilot never streamed: a Model call that
/// only asked for tools reports an empty Message, and one short enough to arrive whole reports it
/// without a start or a single delta.
fn project_message_completed(
    streams: &mut ConversationStreams,
    message_id: &str,
    content: &str,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    let Some(message) = streams.message.as_ref() else {
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
    streams.message = None;
    Ok(projected)
}

/// Names the Activity one Tool execution projects onto. Copilot draws Tool call and Reasoning
/// identities from namespaces of their own, which the Provider seam gives one identity space, so
/// what tells them apart there is the kind they came from.
fn command_activity_id(tool_call_id: &str) -> ProviderActivityId {
    ProviderActivityId::new(format!("command:{tool_call_id}"))
}

/// Opens the Command a Tool execution is recorded as — withheld for a spawn tool, whose
/// delegation the Subagent row represents. Copilot reports no working directory of its own for
/// one: every Tool runs in the Session's Workspace, which the Session already carries.
fn project_command_started(
    streams: &mut ConversationStreams,
    started: &ToolExecutionStartData,
) -> Vec<ProviderEvent> {
    if streams.commands.contains_key(&started.tool_call_id) {
        // A repeated start reports nothing new: the first record keeps its streamed output and
        // its withheld header.
        return Vec::new();
    }
    if started.tool_name == WRITE_AGENT_TOOL || is_broker_call(started) {
        streams.commands.insert(
            started.tool_call_id.clone(),
            ActiveCommand {
                absorbed: true,
                ..ActiveCommand::default()
            },
        );
        return Vec::new();
    }
    let command = command_text(started);
    if started.tool_name == SPAWN_TOOL {
        let spawn_prompt = started
            .arguments
            .as_ref()
            .and_then(|arguments| arguments.get("prompt"))
            .and_then(serde_json::Value::as_str)
            .filter(|prompt| !prompt.trim().is_empty())
            .map(str::to_owned);
        streams.commands.insert(
            started.tool_call_id.clone(),
            ActiveCommand {
                withheld_spawn: Some(command),
                spawn_prompt,
                ..ActiveCommand::default()
            },
        );
        return Vec::new();
    }
    streams
        .commands
        .insert(started.tool_call_id.clone(), ActiveCommand::default());
    vec![ProviderEvent::CommandStarted {
        activity_id: command_activity_id(&started.tool_call_id),
        command,
        cwd: None,
    }]
}

/// Streams the next of a running Command's output into the Transcript.
fn project_command_output(
    streams: &mut ConversationStreams,
    tool_call_id: &str,
    output: String,
) -> Vec<ProviderEvent> {
    let Some(command) = streams.commands.get_mut(tool_call_id) else {
        return Vec::new();
    };
    if command.absorbed {
        return Vec::new();
    }
    command.streamed_output.push_str(&output);
    if command.withheld_spawn.is_some() {
        return Vec::new();
    }
    vec![ProviderEvent::CommandOutputDelta {
        activity_id: command_activity_id(tool_call_id),
        content: output,
    }]
}

/// Settles the Command on what the Tool execution came to, carrying whatever of its output the
/// stream had not already reached.
fn project_command_completed(
    streams: &mut ConversationStreams,
    delegations: &mut HashSet<String>,
    completed: &ToolExecutionCompleteData,
) -> Vec<ProviderEvent> {
    let Some(command) = streams.commands.remove(&completed.tool_call_id) else {
        return Vec::new();
    };
    if command.absorbed {
        return Vec::new();
    }
    if let Some(withheld) = command.withheld_spawn {
        if delegations.remove(&completed.tool_call_id) {
            return Vec::new();
        }
        // The spawn never opened its Subagent, so no row answers for the delegation: the
        // withheld Command surfaces here, carrying what the execution reported went wrong.
        let mut projected = surface_withheld_spawn(
            &completed.tool_call_id,
            withheld,
            command.streamed_output.clone(),
        );
        projected.extend(settle_command_events(completed, &command.streamed_output));
        return projected;
    }
    settle_command_events(completed, &command.streamed_output)
}

/// Opens the Command a withheld spawn would have been, now that no Subagent row will answer for
/// the delegation, replaying the output the withholding kept back.
fn surface_withheld_spawn(
    tool_call_id: &str,
    command: String,
    streamed_output: String,
) -> Vec<ProviderEvent> {
    let activity_id = command_activity_id(tool_call_id);
    let mut projected = vec![ProviderEvent::CommandStarted {
        activity_id: activity_id.clone(),
        command,
        cwd: None,
    }];
    if !streamed_output.is_empty() {
        projected.push(ProviderEvent::CommandOutputDelta {
            activity_id,
            content: streamed_output,
        });
    }
    projected
}

/// Settles a Command on what its Tool execution came to, carrying whatever of its output the
/// stream had not already reached.
fn settle_command_events(
    completed: &ToolExecutionCompleteData,
    streamed_output: &str,
) -> Vec<ProviderEvent> {
    let mut projected = Vec::with_capacity(2);
    if let Some(trailing) = trailing_output(completed, streamed_output) {
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
    streams: &mut ConversationStreams,
    reasoning_id: &str,
    delta: &str,
) -> Vec<ProviderEvent> {
    let mut projected = Vec::new();
    if !streams
        .reasoning
        .iter()
        .any(|block| block.reasoning_id == reasoning_id)
    {
        projected.push(ProviderEvent::ReasoningStarted {
            activity_id: reasoning_activity_id(reasoning_id),
        });
        streams.reasoning.push(ActiveReasoning {
            reasoning_id: reasoning_id.to_owned(),
            ..ActiveReasoning::default()
        });
    }
    let block = streams
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
    streams: &mut ConversationStreams,
    reasoning_id: &str,
    content: &str,
) -> Vec<ProviderEvent> {
    let mut block = streams
        .reasoning
        .iter()
        .position(|block| block.reasoning_id == reasoning_id)
        .map_or_else(ActiveReasoning::default, |open| {
            streams.reasoning.remove(open)
        });
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

/// Settles everything a stopped conversation left open: its Reasoning blocks, the Message it was
/// still streaming, and any withheld spawn no Subagent row ever answered for — surfaced here,
/// because a Command the store never saw is one the store cannot settle, and the delegation would
/// otherwise vanish with the conversation. Open Commands are otherwise left to the store — their
/// outcome is Copilot's to report, not ours to invent — which settles a Turn's open Activities
/// from its own snapshot; only the split here knows the title it is still withholding, which
/// would otherwise go with the block.
fn settle_open_streams(
    streams: &mut ConversationStreams,
    delegations: &HashSet<String>,
) -> Vec<ProviderEvent> {
    let mut projected = Vec::new();
    for mut block in std::mem::take(&mut streams.reasoning) {
        let reasoning_id = std::mem::take(&mut block.reasoning_id);
        projected.extend(settle_reasoning(&reasoning_id, &mut block));
    }
    if streams.message.take().is_some() {
        projected.push(ProviderEvent::AgentMessageCompleted);
    }
    for (tool_call_id, command) in &mut streams.commands {
        if let Some(withheld) = command
            .withheld_spawn
            .take()
            .filter(|_| !delegations.contains(tool_call_id))
        {
            projected.extend(surface_withheld_spawn(
                tool_call_id,
                withheld,
                std::mem::take(&mut command.streamed_output),
            ));
        }
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

    fn agent_event(agent: &str, event_type: &str, data: serde_json::Value) -> SessionEvent {
        SessionEvent {
            agent_id: Some(agent.to_owned()),
            ..event(event_type, data)
        }
    }

    fn project_attributed(
        correlation: &mut CopilotCorrelation,
        event: SessionEvent,
    ) -> Vec<AttributedProviderEvent> {
        let event_type = event.event_type.clone();
        project_session_event(correlation, event)
            .unwrap_or_else(|error| panic!("`{event_type}` projects cleanly, got: {error}"))
    }

    /// Projects an event whose every projection belongs to the owning Session, unwrapped to the
    /// bare events for assertion.
    fn project(
        correlation: &mut CopilotCorrelation,
        event_type: &str,
        data: serde_json::Value,
    ) -> Vec<ProviderEvent> {
        project_attributed(correlation, event(event_type, data))
            .into_iter()
            .map(|attributed| {
                assert_eq!(
                    attributed.attribution,
                    ProviderEventAttribution::OwningSession,
                    "the event lands in the owning Session, got {:?}",
                    attributed.attribution
                );
                attributed.event
            })
            .collect()
    }

    fn in_turn() -> CopilotCorrelation {
        let mut correlation = CopilotCorrelation::new();
        correlation.begin_turn().expect("open the Turn");
        correlation
    }

    fn spawn_started(agent: &str, tool_call_id: &str) -> SessionEvent {
        agent_event(
            agent,
            "subagent.started",
            json!({
                "toolCallId": tool_call_id,
                "agentName": "researcher",
                "agentDisplayName": "Researcher",
                "agentDescription": "Scout the workspace",
            }),
        )
    }

    /// A correlation whose Turn has one Subagent open under instance `agent-1`.
    fn with_subagent() -> CopilotCorrelation {
        let mut correlation = in_turn();
        project_attributed(&mut correlation, spawn_started("agent-1", "t-spawn"));
        correlation
    }

    fn subagent(id: &str) -> ProviderEventAttribution {
        ProviderEventAttribution::Subagent(ProviderSubagentId::new(id))
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
    fn a_message_copilot_never_started_opens_on_its_first_delta() {
        // Copilot streams a Subagent's Message without ever starting it: the deltas simply
        // begin, the way a Reasoning block announces itself by being reasoned into.
        let mut correlation = with_subagent();
        assert_eq!(
            project_attributed(
                &mut correlation,
                agent_event(
                    "agent-1",
                    "assistant.message_delta",
                    json!({ "messageId": "sub-m1", "deltaContent": "Scouting" }),
                ),
            ),
            [
                AttributedProviderEvent {
                    attribution: subagent("agent-1"),
                    event: ProviderEvent::AgentMessageStarted,
                },
                AttributedProviderEvent {
                    attribution: subagent("agent-1"),
                    event: ProviderEvent::AgentMessageDelta {
                        content: "Scouting".to_owned()
                    },
                },
            ]
        );
        assert_eq!(
            project_attributed(
                &mut correlation,
                agent_event(
                    "agent-1",
                    "assistant.message",
                    json!({ "messageId": "sub-m1", "content": "Scouting the workspace." }),
                ),
            ),
            [
                AttributedProviderEvent {
                    attribution: subagent("agent-1"),
                    event: ProviderEvent::AgentMessageDelta {
                        content: " the workspace.".to_owned()
                    },
                },
                AttributedProviderEvent {
                    attribution: subagent("agent-1"),
                    event: ProviderEvent::AgentMessageCompleted,
                },
            ],
            "the completed Message reconciles against the stream the delta opened"
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

    #[test]
    fn a_subagent_started_opens_the_subagent_in_the_owning_session() {
        let mut correlation = in_turn();
        assert_eq!(
            project_attributed(&mut correlation, spawn_started("agent-1", "t-spawn")),
            [AttributedProviderEvent {
                attribution: ProviderEventAttribution::OwningSession,
                event: ProviderEvent::SubagentStarted {
                    subagent_id: ProviderSubagentId::new("agent-1"),
                    name: "Researcher".to_owned(),
                    description: "Scout the workspace".to_owned(),
                    delegation: None,
                },
            }],
            "a spawn whose tool call never surfaced as an execution carries no Delegation"
        );
        assert!(
            project_attributed(&mut correlation, spawn_started("agent-1", "t-spawn")).is_empty(),
            "a repeated started opens nothing twice"
        );
    }

    #[test]
    fn a_subagent_started_carries_the_prompt_its_spawning_tool_call_handed_it() {
        let mut correlation = in_turn();
        project(
            &mut correlation,
            "tool.execution_start",
            json!({
                "toolCallId": "t-spawn",
                "toolName": "task",
                "arguments": {
                    "agent_type": "explore",
                    "description": "Scout the workspace",
                    "prompt": "Find every TODO marker and report the files.",
                },
            }),
        );
        let projected = project_attributed(&mut correlation, spawn_started("agent-1", "t-spawn"));
        let [
            AttributedProviderEvent {
                event: ProviderEvent::SubagentStarted { delegation, .. },
                ..
            },
        ] = projected.as_slice()
        else {
            panic!("the spawn opens one Subagent, got {projected:?}");
        };
        assert_eq!(
            delegation.as_deref(),
            Some("Find every TODO marker and report the files."),
            "the task tool's prompt is the spawn's Delegation"
        );

        // A spawn out of a Subagent's own tool call reads the prompt from that Subagent's
        // conversation.
        project_attributed(
            &mut correlation,
            agent_event(
                "agent-1",
                "tool.execution_start",
                json!({
                    "toolCallId": "t-nested",
                    "toolName": "task",
                    "arguments": { "prompt": "Read the TODO in src/main.rs." },
                }),
            ),
        );
        let nested = project_attributed(&mut correlation, spawn_started("agent-2", "t-nested"));
        let [
            AttributedProviderEvent {
                event: ProviderEvent::SubagentStarted { delegation, .. },
                ..
            },
        ] = nested.as_slice()
        else {
            panic!("the nested spawn opens one Subagent, got {nested:?}");
        };
        assert_eq!(delegation.as_deref(), Some("Read the TODO in src/main.rs."));
    }

    #[test]
    fn a_spawn_outside_any_turn_with_nothing_owed_lands_nowhere() {
        let mut correlation = CopilotCorrelation::new();
        assert!(
            project_attributed(&mut correlation, spawn_started("agent-1", "t-spawn")).is_empty()
        );
        assert!(
            project_attributed(
                &mut correlation,
                agent_event(
                    "agent-1",
                    "assistant.message",
                    json!({ "messageId": "sub-m1", "content": "orphan" }),
                ),
            )
            .is_empty(),
            "work attributed to the dropped spawn lands nowhere too"
        );
    }

    #[test]
    fn a_subagents_stream_lands_in_the_subagent_while_the_main_message_streams_its_own() {
        let mut correlation = with_subagent();
        project(
            &mut correlation,
            "assistant.message_start",
            json!({ "messageId": "m1" }),
        );

        // The subagent's whole Message arrives while the main agent's is still open: it lands
        // attributed to the Subagent rather than colliding with the main Message.
        assert_eq!(
            project_attributed(
                &mut correlation,
                agent_event(
                    "agent-1",
                    "assistant.message",
                    json!({ "messageId": "sub-m1", "content": "Scouted." }),
                ),
            ),
            [
                AttributedProviderEvent {
                    attribution: subagent("agent-1"),
                    event: ProviderEvent::AgentMessageStarted,
                },
                AttributedProviderEvent {
                    attribution: subagent("agent-1"),
                    event: ProviderEvent::AgentMessageDelta {
                        content: "Scouted.".to_owned()
                    },
                },
                AttributedProviderEvent {
                    attribution: subagent("agent-1"),
                    event: ProviderEvent::AgentMessageCompleted,
                },
            ]
        );
        assert_eq!(
            project(
                &mut correlation,
                "assistant.message_delta",
                json!({ "messageId": "m1", "deltaContent": "Still the main answer" }),
            ),
            [ProviderEvent::AgentMessageDelta {
                content: "Still the main answer".to_owned()
            }],
            "the main Message streams on undisturbed"
        );
    }

    #[test]
    fn a_subagents_stream_keeps_landing_after_the_turn_settled() {
        let mut correlation = with_subagent();
        project(&mut correlation, "session.idle", json!({}));

        let projected = project_attributed(
            &mut correlation,
            agent_event(
                "agent-1",
                "tool.execution_start",
                json!({ "toolCallId": "t-sub", "toolName": "bash", "arguments": { "command": "cargo audit" } }),
            ),
        );
        assert_eq!(
            projected,
            [AttributedProviderEvent {
                attribution: subagent("agent-1"),
                event: ProviderEvent::CommandStarted {
                    activity_id: command_activity_id("t-sub"),
                    command: "cargo audit".to_owned(),
                    cwd: None,
                },
            }],
            "a Subagent outlives the Turn that spawned it"
        );
    }

    #[test]
    fn a_subagent_settle_closes_its_open_streams_and_reports_the_outcome() {
        let mut correlation = with_subagent();
        project_attributed(
            &mut correlation,
            agent_event(
                "agent-1",
                "assistant.reasoning_delta",
                json!({ "reasoningId": "sub-r1", "deltaContent": "**Weighing the findings**" }),
            ),
        );

        assert_eq!(
            project_attributed(
                &mut correlation,
                event(
                    "subagent.completed",
                    json!({ "toolCallId": "t-spawn", "agentName": "researcher", "agentDisplayName": "Researcher" }),
                ),
            ),
            [
                AttributedProviderEvent {
                    attribution: subagent("agent-1"),
                    event: ProviderEvent::ReasoningTitleChanged {
                        activity_id: reasoning_activity_id("sub-r1"),
                        title: "Weighing the findings".to_owned(),
                    },
                },
                AttributedProviderEvent {
                    attribution: subagent("agent-1"),
                    event: ProviderEvent::ReasoningCompleted {
                        activity_id: reasoning_activity_id("sub-r1"),
                    },
                },
                AttributedProviderEvent {
                    attribution: ProviderEventAttribution::OwningSession,
                    event: ProviderEvent::SubagentCompleted {
                        subagent_id: ProviderSubagentId::new("agent-1"),
                        status: ProviderSubagentStatus::Completed,
                    },
                },
            ],
            "a block cut short keeps the title its split was still withholding"
        );
        assert!(
            project_attributed(
                &mut correlation,
                agent_event(
                    "agent-1",
                    "assistant.message",
                    json!({ "messageId": "sub-m2", "content": "too late" }),
                ),
            )
            .is_empty(),
            "nothing more of the settled Subagent's lands anywhere"
        );
    }

    #[test]
    fn a_cancelled_completion_and_a_failure_both_settle_the_subagent_as_failed() {
        for (event_type, data) in [
            (
                "subagent.completed",
                json!({ "toolCallId": "t-spawn", "agentName": "researcher", "agentDisplayName": "Researcher", "cancelled": true }),
            ),
            (
                "subagent.failed",
                json!({ "toolCallId": "t-spawn", "agentName": "researcher", "agentDisplayName": "Researcher", "error": "the researcher crashed" }),
            ),
        ] {
            let mut correlation = with_subagent();
            let projected = project_attributed(&mut correlation, event(event_type, data));
            assert_eq!(
                projected,
                [AttributedProviderEvent {
                    attribution: ProviderEventAttribution::OwningSession,
                    event: ProviderEvent::SubagentCompleted {
                        subagent_id: ProviderSubagentId::new("agent-1"),
                        status: ProviderSubagentStatus::Failed,
                    },
                }],
                "`{event_type}` settles the Subagent as failed"
            );
        }
    }

    #[test]
    fn work_attributed_to_an_unknown_instance_lands_nowhere() {
        let mut correlation = in_turn();
        assert!(
            project_attributed(
                &mut correlation,
                agent_event(
                    "never-started",
                    "assistant.message",
                    json!({ "messageId": "m9", "content": "whose is this" }),
                ),
            )
            .is_empty()
        );
    }

    #[test]
    fn late_output_owed_to_a_settled_subagent_begins_a_continuation_settled_by_the_next_idle() {
        let mut correlation = with_subagent();
        project(&mut correlation, "session.idle", json!({}));
        project_attributed(
            &mut correlation,
            event(
                "subagent.completed",
                json!({ "toolCallId": "t-spawn", "agentName": "researcher", "agentDisplayName": "Researcher" }),
            ),
        );

        assert_eq!(
            project(
                &mut correlation,
                "assistant.message",
                json!({ "messageId": "m2", "content": "The scout came back." }),
            ),
            [
                ProviderEvent::AgentMessageStarted,
                ProviderEvent::AgentMessageDelta {
                    content: "The scout came back.".to_owned()
                },
                ProviderEvent::AgentMessageCompleted,
            ],
            "output the settle provoked projects with no Turn active"
        );
        assert_eq!(
            project(&mut correlation, "session.idle", json!({})),
            [ProviderEvent::TurnCompleted],
            "the stretch's own idle settles the Continuation"
        );
        assert!(
            project(
                &mut correlation,
                "assistant.message",
                json!({ "messageId": "m3", "content": "stray" }),
            )
            .is_empty(),
            "with nothing owed any longer, stray output is dropped as it always was"
        );
    }

    #[test]
    fn a_prompt_during_a_continuation_swallows_the_stale_stretches_idle() {
        let mut correlation = with_subagent();
        project(&mut correlation, "session.idle", json!({}));
        project_attributed(
            &mut correlation,
            event(
                "subagent.completed",
                json!({ "toolCallId": "t-spawn", "agentName": "researcher", "agentDisplayName": "Researcher" }),
            ),
        );
        project(
            &mut correlation,
            "assistant.message_start",
            json!({ "messageId": "m2" }),
        );

        correlation
            .begin_turn()
            .expect("a Prompt settles the Continuation rather than colliding with it");
        assert!(
            project(&mut correlation, "session.idle", json!({})).is_empty(),
            "the stale stretch's idle settles nothing"
        );
        assert_eq!(
            project(
                &mut correlation,
                "assistant.message",
                json!({ "messageId": "m3", "content": "Prompted answer" }),
            ),
            [
                ProviderEvent::AgentMessageStarted,
                ProviderEvent::AgentMessageDelta {
                    content: "Prompted answer".to_owned()
                },
                ProviderEvent::AgentMessageCompleted,
            ]
        );
        assert_eq!(
            project(&mut correlation, "session.idle", json!({})),
            [ProviderEvent::TurnCompleted],
            "the Prompt's own Turn settles on its own idle"
        );
    }

    #[test]
    fn a_spawn_tool_execution_is_represented_by_the_subagent_row_alone() {
        let mut correlation = in_turn();
        assert!(
            project(
                &mut correlation,
                "tool.execution_start",
                json!({
                    "toolCallId": "t-spawn",
                    "toolName": "task",
                    "arguments": { "agent_type": "explore", "prompt": "Scout the workspace" },
                }),
            )
            .is_empty(),
            "the spawning tool call opens no Command row of its own"
        );
        assert!(
            matches!(
                project_attributed(&mut correlation, spawn_started("agent-1", "t-spawn"))
                    .as_slice(),
                [AttributedProviderEvent {
                    event: ProviderEvent::SubagentStarted { .. },
                    ..
                }]
            ),
            "the Subagent row is the delegation's whole representation"
        );
        assert!(
            project(
                &mut correlation,
                "tool.execution_complete",
                json!({ "toolCallId": "t-spawn", "success": true, "result": { "content": "done" } }),
            )
            .is_empty(),
            "the spawn's completion settles nothing: the Subagent lifecycle already did"
        );
    }

    #[test]
    fn a_spawn_execution_reported_after_its_subagent_opened_stays_withheld() {
        // The runtime's own delegation may report `subagent.started` before — or without — the
        // spawning tool call ever surfacing as an execution.
        let mut correlation = in_turn();
        project_attributed(&mut correlation, spawn_started("agent-1", "t-spawn"));
        assert!(
            project(
                &mut correlation,
                "tool.execution_start",
                json!({ "toolCallId": "t-spawn", "toolName": "task", "arguments": {} }),
            )
            .is_empty(),
        );

        assert!(
            project(
                &mut correlation,
                "tool.execution_complete",
                json!({ "toolCallId": "t-spawn", "success": true, "result": { "content": "done" } }),
            )
            .is_empty(),
            "the Subagent row already answers for the delegation, whichever event opened it first"
        );
    }

    #[test]
    fn a_spawn_that_never_opened_its_subagent_surfaces_as_the_command_it_was() {
        let mut correlation = in_turn();
        project(
            &mut correlation,
            "tool.execution_start",
            json!({
                "toolCallId": "t-spawn",
                "toolName": "task",
                "arguments": { "agent_type": "no-such-agent" },
            }),
        );

        assert_eq!(
            project(
                &mut correlation,
                "tool.execution_complete",
                json!({
                    "toolCallId": "t-spawn",
                    "success": false,
                    "error": { "message": "unknown agent type" },
                }),
            ),
            [
                ProviderEvent::CommandStarted {
                    activity_id: command_activity_id("t-spawn"),
                    command: r#"task {"agent_type":"no-such-agent"}"#.to_owned(),
                    cwd: None,
                },
                ProviderEvent::CommandOutputDelta {
                    activity_id: command_activity_id("t-spawn"),
                    content: "unknown agent type".to_owned(),
                },
                ProviderEvent::CommandCompleted {
                    activity_id: command_activity_id("t-spawn"),
                    status: ProviderCommandStatus::Failed,
                    exit_status: None,
                },
            ],
            "with no Subagent row answering for the delegation, the failed spawn stays visible"
        );
    }

    #[test]
    fn a_repeated_spawn_execution_start_does_not_forget_the_delegation() {
        let mut correlation = in_turn();
        let start = json!({ "toolCallId": "t-spawn", "toolName": "task", "arguments": {} });
        project(&mut correlation, "tool.execution_start", start.clone());
        project_attributed(&mut correlation, spawn_started("agent-1", "t-spawn"));
        assert!(project(&mut correlation, "tool.execution_start", start).is_empty());

        assert!(
            project(
                &mut correlation,
                "tool.execution_complete",
                json!({ "toolCallId": "t-spawn", "success": true, "result": { "content": "done" } }),
            )
            .is_empty(),
            "a repeated start neither reopens the Command nor un-delegates the spawn"
        );
    }

    #[test]
    fn a_withheld_spawn_still_open_at_the_turns_settle_surfaces_for_the_store_to_settle() {
        let mut correlation = in_turn();
        project(
            &mut correlation,
            "tool.execution_start",
            json!({ "toolCallId": "t-spawn", "toolName": "task", "arguments": { "agent_type": "explore" } }),
        );

        assert_eq!(
            project(&mut correlation, "session.idle", json!({})),
            [
                ProviderEvent::CommandStarted {
                    activity_id: command_activity_id("t-spawn"),
                    command: r#"task {"agent_type":"explore"}"#.to_owned(),
                    cwd: None,
                },
                ProviderEvent::TurnCompleted,
            ],
            "a delegation no Subagent row ever answered for does not vanish with the Turn"
        );
    }

    #[test]
    fn a_spawn_out_of_a_subagents_own_tool_call_recurses_one_level_down() {
        let mut correlation = with_subagent();
        project_attributed(
            &mut correlation,
            agent_event(
                "agent-1",
                "tool.execution_start",
                json!({ "toolCallId": "t-nested", "toolName": "task", "arguments": {} }),
            ),
        );

        let projected = project_attributed(&mut correlation, spawn_started("agent-2", "t-nested"));
        let [AttributedProviderEvent { attribution, event }] = projected.as_slice() else {
            panic!("a nested spawn opens one Subagent, got {projected:?}");
        };
        assert_eq!(
            *attribution,
            subagent("agent-1"),
            "the grandchild hangs under the Subagent whose tool call spawned it"
        );
        assert!(matches!(
            event,
            ProviderEvent::SubagentStarted { subagent_id, .. }
                if *subagent_id == ProviderSubagentId::new("agent-2")
        ));
    }

    #[test]
    fn a_started_without_an_instance_identity_still_opens_the_row() {
        let mut correlation = in_turn();
        let projected = project_attributed(
            &mut correlation,
            event(
                "subagent.started",
                json!({
                    "toolCallId": "t-spawn",
                    "agentName": "researcher",
                    "agentDisplayName": "",
                    "agentDescription": "Scout the workspace",
                }),
            ),
        );
        let [
            AttributedProviderEvent {
                event:
                    ProviderEvent::SubagentStarted {
                        subagent_id, name, ..
                    },
                ..
            },
        ] = projected.as_slice()
        else {
            panic!("the spawn still opens the Subagent, got {projected:?}");
        };
        assert_eq!(
            *subagent_id,
            ProviderSubagentId::new("t-spawn"),
            "the spawning tool call stands in for the missing instance identity"
        );
        assert_eq!(
            name, "researcher",
            "an empty display name falls back to the configured one"
        );

        assert_eq!(
            project_attributed(
                &mut correlation,
                event(
                    "subagent.completed",
                    json!({ "toolCallId": "t-spawn", "agentName": "researcher", "agentDisplayName": "" }),
                ),
            ),
            [AttributedProviderEvent {
                attribution: ProviderEventAttribution::OwningSession,
                event: ProviderEvent::SubagentCompleted {
                    subagent_id: ProviderSubagentId::new("t-spawn"),
                    status: ProviderSubagentStatus::Completed,
                },
            }],
            "the settle still reaches the row it opened"
        );
    }

    /// One shell as `session.tasks.list` lists it.
    fn listed_shell(id: &str, attachment: &str, status: &str) -> serde_json::Value {
        json!({
            "type": "shell",
            "id": id,
            "description": "Serve the docs",
            "command": "mdbook serve",
            "status": status,
            "startedAt": "2026-01-01T00:00:00Z",
            "attachmentMode": attachment,
        })
    }

    fn detached_completion(shell: &str) -> serde_json::Value {
        json!({
            "content": "<system_notification>\nDetached shell \"Serve the docs\" (shellId: shell-1) has completed.\n</system_notification>",
            "kind": { "type": "shell_detached_completed", "shellId": shell, "description": "Serve the docs" },
        })
    }

    fn roster(
        correlation: &mut CopilotCorrelation,
        tasks: &[serde_json::Value],
    ) -> Vec<ProviderEvent> {
        correlation
            .project_task_roster(tasks)
            .into_iter()
            .map(|attributed| {
                assert_eq!(
                    attributed.attribution,
                    ProviderEventAttribution::OwningSession
                );
                attributed.event
            })
            .collect()
    }

    #[test]
    fn a_running_detached_shell_on_the_roster_starts_a_watch_under_its_shell_identity() {
        let mut correlation = in_turn();
        assert_eq!(
            roster(
                &mut correlation,
                &[
                    listed_shell("shell-1", "detached", "running"),
                    json!({ "type": "agent", "id": "agent-1", "description": "Audit" }),
                ],
            ),
            [ProviderEvent::WatchStarted {
                watch_id: ProviderWatchId::new("shell-1"),
                description: "Serve the docs".to_owned(),
            }]
        );
        assert!(
            roster(
                &mut correlation,
                &[listed_shell("shell-1", "detached", "running")]
            )
            .is_empty(),
            "a shell already watched is not announced again"
        );
    }

    #[test]
    fn attached_and_already_ended_shells_on_the_roster_are_no_watches() {
        let mut correlation = in_turn();
        assert!(
            roster(
                &mut correlation,
                &[
                    listed_shell("attached", "attached", "running"),
                    listed_shell("ended", "detached", "completed"),
                    listed_shell("cancelled", "detached", "cancelled"),
                ],
            )
            .is_empty()
        );
    }

    #[test]
    fn a_detached_shell_described_by_nothing_is_described_by_its_command() {
        let mut correlation = in_turn();
        let mut shell = listed_shell("shell-1", "detached", "running");
        shell["description"] = json!(" ");
        assert_eq!(
            roster(&mut correlation, &[shell]),
            [ProviderEvent::WatchStarted {
                watch_id: ProviderWatchId::new("shell-1"),
                description: "mdbook serve".to_owned(),
            }]
        );
    }

    #[test]
    fn a_detached_shells_completion_settles_its_watch_and_begins_a_continuation() {
        let mut correlation = in_turn();
        roster(
            &mut correlation,
            &[listed_shell("shell-1", "detached", "running")],
        );
        project(&mut correlation, "session.idle", json!({}));

        assert_eq!(
            project(
                &mut correlation,
                "system.notification",
                detached_completion("shell-1")
            ),
            [ProviderEvent::WatchSettled {
                watch_id: ProviderWatchId::new("shell-1"),
                outcome: ProviderWatchOutcome::Completed,
                summary: Some(
                    r#"Detached shell "Serve the docs" (shellId: shell-1) has completed."#
                        .to_owned()
                ),
                woke_agent: true,
            }],
            "the notification's text, out of its wrapper, is how the Watch settled"
        );
        assert_eq!(
            project(
                &mut correlation,
                "assistant.message",
                json!({ "messageId": "m2", "content": "The docs server stopped." }),
            )
            .len(),
            3,
            "the woken loop's output opens a Continuation rather than being dropped"
        );
        assert_eq!(
            project(&mut correlation, "session.idle", json!({})),
            [ProviderEvent::TurnCompleted]
        );
        assert!(
            project(
                &mut correlation,
                "system.notification",
                detached_completion("shell-1")
            )
            .is_empty(),
            "a settled Watch settles once"
        );
        assert!(
            roster(
                &mut correlation,
                &[listed_shell("shell-1", "detached", "running")]
            )
            .is_empty(),
            "a roster read racing the completion does not start the settled Watch again"
        );
    }

    #[test]
    fn a_live_watch_owes_the_loops_output_a_continuation() {
        let mut correlation = in_turn();
        roster(
            &mut correlation,
            &[listed_shell("shell-1", "detached", "running")],
        );
        project(&mut correlation, "session.idle", json!({}));

        assert!(
            !project(
                &mut correlation,
                "assistant.message",
                json!({ "messageId": "m2", "content": "Checking the server." }),
            )
            .is_empty()
        );
    }

    #[test]
    fn a_completion_for_a_shell_never_seen_records_its_watch_whole_and_owes_the_woken_loop_a_continuation()
     {
        let mut correlation = in_turn();
        project(&mut correlation, "session.idle", json!({}));

        assert_eq!(
            project(
                &mut correlation,
                "system.notification",
                detached_completion("unseen")
            ),
            [
                ProviderEvent::WatchStarted {
                    watch_id: ProviderWatchId::new("unseen"),
                    description: "Serve the docs".to_owned(),
                },
                ProviderEvent::WatchSettled {
                    watch_id: ProviderWatchId::new("unseen"),
                    outcome: ProviderWatchOutcome::Completed,
                    summary: Some(
                        r#"Detached shell "Serve the docs" (shellId: shell-1) has completed."#
                            .to_owned()
                    ),
                    woke_agent: true,
                },
            ],
            "orchestration owes the woken output a Continuation only through a Watch it knows"
        );
        assert!(
            !project(
                &mut correlation,
                "assistant.message",
                json!({ "messageId": "m2", "content": "Woken." }),
            )
            .is_empty(),
            "the loop wakes all the same, and its output is shown"
        );
        project(&mut correlation, "session.idle", json!({}));
        assert!(
            project(
                &mut correlation,
                "system.notification",
                detached_completion("unseen")
            )
            .is_empty(),
            "a repeated completion records nothing twice"
        );
    }

    #[test]
    fn a_completion_for_a_shell_suru_stopped_wakes_nothing_suru_shows() {
        let mut correlation = in_turn();
        roster(
            &mut correlation,
            &[listed_shell("shell-1", "detached", "running")],
        );
        project(&mut correlation, "session.idle", json!({}));
        correlation.project_watches_stopped(&["shell-1".to_owned()]);

        assert!(
            project(
                &mut correlation,
                "system.notification",
                detached_completion("shell-1")
            )
            .is_empty()
        );
        assert!(
            project(
                &mut correlation,
                "assistant.message",
                json!({ "messageId": "m2", "content": "stray" }),
            )
            .is_empty(),
            "a Watch stopped by an interrupt wakes nothing Suru records"
        );
    }

    #[test]
    fn other_notifications_wake_nothing_this_projection_follows() {
        let mut correlation = in_turn();
        project(&mut correlation, "session.idle", json!({}));

        assert!(
            project(
                &mut correlation,
                "system.notification",
                json!({ "content": "done", "kind": { "type": "shell_completed", "shellId": "s" } }),
            )
            .is_empty()
        );
        assert!(
            project(
                &mut correlation,
                "assistant.message",
                json!({ "messageId": "m2", "content": "stray" }),
            )
            .is_empty()
        );
    }

    #[test]
    fn stopped_watches_settle_as_stopped_and_a_later_completion_repeats_nothing() {
        let mut correlation = in_turn();
        roster(
            &mut correlation,
            &[
                listed_shell("shell-1", "detached", "running"),
                listed_shell("shell-2", "detached", "running"),
            ],
        );
        project(&mut correlation, "session.idle", json!({}));
        assert_eq!(
            correlation.live_watches(&[
                ProviderWatchId::new("shell-1"),
                ProviderWatchId::new("gone")
            ]),
            ["shell-1"]
        );

        let stopped = correlation.project_watches_stopped(&["shell-1".to_owned()]);
        assert_eq!(
            stopped
                .into_iter()
                .map(|event| event.event)
                .collect::<Vec<_>>(),
            [ProviderEvent::WatchSettled {
                watch_id: ProviderWatchId::new("shell-1"),
                outcome: ProviderWatchOutcome::Stopped,
                summary: None,
                woke_agent: false,
            }]
        );
        assert!(
            correlation
                .project_watches_stopped(&["shell-1".to_owned()])
                .is_empty()
        );
        let lost = correlation.project_watches_lost();
        assert_eq!(
            lost.into_iter()
                .map(|event| event.event)
                .collect::<Vec<_>>(),
            [ProviderEvent::WatchSettled {
                watch_id: ProviderWatchId::new("shell-2"),
                outcome: ProviderWatchOutcome::Lost,
                summary: None,
                woke_agent: false,
            }],
            "a process that dies loses every Watch still live"
        );
    }

    #[test]
    fn a_notification_out_of_its_wrapper_is_what_a_reader_sees() {
        assert_eq!(
            notification_text("<system_notification>\n  Shell done.\n</system_notification>\n"),
            Some("Shell done.".to_owned())
        );
        assert_eq!(
            notification_text("Unwrapped."),
            Some("Unwrapped.".to_owned())
        );
        assert_eq!(
            notification_text("<system_notification></system_notification>"),
            None
        );
    }

    /// A `user.message` Copilot delivers to `agent`'s own loop, as `delivery`, from `source`.
    fn delivered_message(
        agent: &str,
        delivery: &str,
        source: Option<&str>,
        content: &str,
    ) -> SessionEvent {
        let mut data = json!({ "content": content, "delivery": delivery, "turnId": "1" });
        if let Some(source) = source {
            data["source"] = json!(source);
        }
        agent_event(agent, "user.message", data)
    }

    fn steer(subagent: &str, delegation: &str) -> ProviderEvent {
        ProviderEvent::SubagentSteered {
            subagent_id: ProviderSubagentId::new(subagent),
            delegation: delegation.to_owned(),
        }
    }

    #[test]
    fn a_message_steering_a_working_subagent_is_a_steer_the_main_agent_sent() {
        let mut correlation = with_subagent();
        assert_eq!(
            project_attributed(
                &mut correlation,
                delivered_message(
                    "agent-1",
                    "steering",
                    Some("agent-94bf4cb4-74ac-43d9-8903-038759ca8a86"),
                    "Also say PINEAPPLE.",
                ),
            ),
            [AttributedProviderEvent {
                attribution: ProviderEventAttribution::OwningSession,
                event: steer("agent-1", "Also say PINEAPPLE."),
            }],
            "the main agent sends as `agent-<its session id>`, which names no Subagent"
        );
        assert_eq!(
            project_attributed(
                &mut correlation,
                delivered_message("agent-1", "steering", None, "And MANGO."),
            ),
            [AttributedProviderEvent {
                attribution: ProviderEventAttribution::OwningSession,
                event: steer("agent-1", "And MANGO."),
            }],
            "a steer naming no sender is the owning Session's"
        );
    }

    #[test]
    fn a_message_a_sibling_sent_steers_on_that_siblings_behalf_even_once_it_settled() {
        let mut correlation = with_subagent();
        project_attributed(&mut correlation, spawn_started("agent-2", "t-sibling"));
        assert_eq!(
            project_attributed(
                &mut correlation,
                delivered_message("agent-1", "steering", Some("agent-agent-2"), "Say MANGO."),
            ),
            [AttributedProviderEvent {
                attribution: subagent("agent-2"),
                event: steer("agent-1", "Say MANGO."),
            }]
        );

        project_attributed(
            &mut correlation,
            agent_event(
                "agent-2",
                "subagent.completed",
                json!({ "toolCallId": "t-sibling", "agentName": "researcher", "agentDisplayName": "Researcher" }),
            ),
        );
        assert_eq!(
            project_attributed(
                &mut correlation,
                delivered_message("agent-1", "steering", Some("agent-agent-2"), "Say KIWI."),
            ),
            [AttributedProviderEvent {
                attribution: subagent("agent-2"),
                event: steer("agent-1", "Say KIWI."),
            }],
            "the sender is still the sibling that sent it after that sibling settled"
        );
    }

    #[test]
    fn a_message_that_does_not_steer_a_working_subagent_is_no_steer() {
        let mut correlation = with_subagent();
        for delivery in ["idle", "queued", "later"] {
            assert!(
                project_attributed(
                    &mut correlation,
                    delivered_message("agent-1", delivery, None, "Read the notes."),
                )
                .is_empty(),
                "a `{delivery}` delivery begins or awaits a run of its own rather than steering"
            );
        }
        assert!(
            project_attributed(
                &mut correlation,
                agent_event(
                    "agent-1",
                    "user.message",
                    json!({ "content": "Read the notes." })
                ),
            )
            .is_empty(),
            "a message with no delivery reported steers nothing"
        );
        assert!(
            project_attributed(
                &mut correlation,
                delivered_message("agent-1", "steering", None, "  "),
            )
            .is_empty(),
            "a steer carrying nothing to read stands nowhere"
        );
    }

    #[test]
    fn a_message_for_an_instance_no_subagent_holds_stands_nowhere() {
        let mut correlation = with_subagent();
        for delivery in ["steering", "queued", "idle"] {
            assert!(
                project_attributed(
                    &mut correlation,
                    delivered_message("agent-9", delivery, None, "Who?"),
                )
                .is_empty(),
                "an instance no Subagent holds has no Session for a `{delivery}` message to reach"
            );
        }
    }

    #[test]
    fn a_main_agent_message_is_not_a_subagent_steer() {
        let mut correlation = with_subagent();
        assert!(
            project(
                &mut correlation,
                "user.message",
                json!({ "content": "Answer in French", "delivery": "steering" }),
            )
            .is_empty(),
            "the main loop's own messages are the user's Prompts, which Suru already holds"
        );
    }

    /// The Broker's Tools reach Copilot as executions on the MCP server `suru`. None of it is work
    /// a Transcript presents — the Broker adds whatever row stands for what a call did — so the
    /// execution projects nothing in the conversation that made it, the main agent's or a native
    /// Subagent's, while a call to any other MCP server stands as a Command as ever.
    #[test]
    fn a_broker_call_adds_nothing_to_the_transcript_of_the_agent_that_made_it() {
        let mut correlation = with_subagent();
        let broker_call = |tool_call_id: &str| {
            json!({
                "toolCallId": tool_call_id,
                "toolName": "suru-spawn_subagent",
                "mcpServerName": "suru",
                "mcpToolName": "spawn_subagent",
                "arguments": { "provider": "codex", "model": "gpt-5.5", "prompt": "Map it." },
            })
        };
        for (event_type, data) in [
            ("tool.execution_start", broker_call("t-broker")),
            (
                "tool.execution_partial_result",
                json!({ "toolCallId": "t-broker", "partialOutput": "Spawning" }),
            ),
            (
                "tool.execution_complete",
                json!({
                    "toolCallId": "t-broker",
                    "success": true,
                    "result": { "content": "Spawned the Subagent." },
                }),
            ),
        ] {
            assert!(
                project(&mut correlation, event_type, data).is_empty(),
                "the main agent's Broker call projects nothing at `{event_type}`"
            );
        }
        for event in [
            agent_event(
                "agent-1",
                "tool.execution_start",
                broker_call("t-sub-broker"),
            ),
            agent_event(
                "agent-1",
                "tool.execution_complete",
                json!({
                    "toolCallId": "t-sub-broker",
                    "success": false,
                    "error": { "message": "The spawn was refused." },
                }),
            ),
        ] {
            assert!(
                project_attributed(&mut correlation, event).is_empty(),
                "a Subagent's Broker call projects nothing in its own conversation either"
            );
        }

        assert_eq!(
            project(
                &mut correlation,
                "tool.execution_start",
                json!({
                    "toolCallId": "t-linear",
                    "toolName": "linear-list_issues",
                    "mcpServerName": "linear",
                    "mcpToolName": "list_issues",
                    "arguments": {},
                }),
            ),
            [ProviderEvent::CommandStarted {
                activity_id: ProviderActivityId::new("command:t-linear"),
                command: "linear/list_issues".to_owned(),
                cwd: None,
            }],
            "another MCP server's call is still a Command"
        );
    }

    #[test]
    fn a_write_agent_execution_adds_nothing_to_the_senders_transcript() {
        let mut correlation = with_subagent();
        let write_agent = |tool_call_id: &str| {
            json!({
                "toolCallId": tool_call_id,
                "toolName": "write_agent",
                "arguments": { "agent_id": "agent-1", "message": "Also say PINEAPPLE." },
            })
        };
        assert!(
            project(
                &mut correlation,
                "tool.execution_start",
                write_agent("t-write")
            )
            .is_empty()
        );
        assert!(
            project(
                &mut correlation,
                "tool.execution_partial_result",
                json!({ "toolCallId": "t-write", "partialOutput": "Message delivered" }),
            )
            .is_empty()
        );
        assert!(
            project(
                &mut correlation,
                "tool.execution_complete",
                json!({
                    "toolCallId": "t-write",
                    "success": true,
                    "result": { "content": "Message delivered to agent agent-1." },
                }),
            )
            .is_empty(),
            "the send is a Delegation standing in the Subagent it reached, not a Command"
        );
        assert!(
            project(
                &mut correlation,
                "tool.execution_start",
                write_agent("t-refused")
            )
            .is_empty()
        );
        assert!(
            project(
                &mut correlation,
                "tool.execution_complete",
                json!({
                    "toolCallId": "t-refused",
                    "success": false,
                    "error": { "message": "No agent agent-1" },
                }),
            )
            .is_empty(),
            "a send that delivered nothing stands nowhere either"
        );

        // A sibling's send out of its own conversation is absorbed there just the same.
        project_attributed(&mut correlation, spawn_started("agent-2", "t-sibling"));
        for event in [
            agent_event(
                "agent-2",
                "tool.execution_start",
                write_agent("t-sib-write"),
            ),
            agent_event(
                "agent-2",
                "tool.execution_complete",
                json!({ "toolCallId": "t-sib-write", "success": true }),
            ),
        ] {
            assert!(project_attributed(&mut correlation, event).is_empty());
        }

        // One still running when the loop stops surfaces nothing for the store to settle.
        project(
            &mut correlation,
            "tool.execution_start",
            write_agent("t-open"),
        );
        assert_eq!(
            project(&mut correlation, "session.idle", json!({})),
            [ProviderEvent::TurnCompleted]
        );
    }

    /// The main agent's own identity as a sender: `agent-<its Copilot Session id>`, which names
    /// no Subagent.
    const MAIN_AGENT: &str = "agent-94bf4cb4-74ac-43d9-8903-038759ca8a86";

    /// A correlation whose Turn spawned `agent-1`, whose stretch has since settled.
    fn with_settled_subagent() -> CopilotCorrelation {
        let mut correlation = with_subagent();
        project_attributed(
            &mut correlation,
            agent_event(
                "agent-1",
                "subagent.completed",
                json!({ "toolCallId": "t-spawn", "agentName": "researcher", "agentDisplayName": "Researcher" }),
            ),
        );
        correlation
    }

    fn resumed(subagent: &str, description: &str, delegation: Option<&str>) -> ProviderEvent {
        ProviderEvent::SubagentResumed {
            subagent_id: ProviderSubagentId::new(subagent),
            name: "Researcher".to_owned(),
            description: description.to_owned(),
            delegation: delegation.map(str::to_owned),
        }
    }

    fn settled(subagent: &str, status: ProviderSubagentStatus) -> AttributedProviderEvent {
        AttributedProviderEvent {
            attribution: ProviderEventAttribution::OwningSession,
            event: ProviderEvent::SubagentCompleted {
                subagent_id: ProviderSubagentId::new(subagent),
                status,
            },
        }
    }

    /// The model message `agent` closes one agent-loop turn with: one requesting `tools`, or with
    /// none, the answer that exits the loop.
    fn loop_message(agent: &str, message_id: &str, content: &str, tools: &[&str]) -> SessionEvent {
        let requests = tools
            .iter()
            .map(|tool| json!({ "toolCallId": tool, "name": "bash", "arguments": {} }))
            .collect::<Vec<_>>();
        agent_event(
            agent,
            "assistant.message",
            json!({ "messageId": message_id, "content": content, "toolRequests": requests }),
        )
    }

    fn turn_end(agent: &str, turn_id: &str) -> SessionEvent {
        agent_event(agent, "assistant.turn_end", json!({ "turnId": turn_id }))
    }

    #[test]
    fn a_message_a_settled_subagent_consumes_resumes_it_whatever_its_delivery() {
        for delivery in ["queued", "idle", "steering"] {
            let mut correlation = with_settled_subagent();
            assert_eq!(
                project_attributed(
                    &mut correlation,
                    delivered_message(
                        "agent-1",
                        delivery,
                        Some(MAIN_AGENT),
                        "Also say PINEAPPLE.\nThen stop."
                    ),
                ),
                [AttributedProviderEvent {
                    attribution: ProviderEventAttribution::OwningSession,
                    event: resumed(
                        "agent-1",
                        "Also say PINEAPPLE.",
                        Some("Also say PINEAPPLE.\nThen stop."),
                    ),
                }],
                "a `{delivery}` message the settled Subagent consumed runs it again: the main \
                 agent's resume, opened by the message, described by its first line"
            );
            assert_eq!(
                project_attributed(
                    &mut correlation,
                    agent_event(
                        "agent-1",
                        "assistant.message",
                        json!({ "messageId": "r1", "content": "PINEAPPLE" }),
                    ),
                ),
                [
                    ProviderEvent::AgentMessageStarted,
                    ProviderEvent::AgentMessageDelta {
                        content: "PINEAPPLE".to_owned(),
                    },
                    ProviderEvent::AgentMessageCompleted,
                ]
                .map(|event| AttributedProviderEvent {
                    attribution: subagent("agent-1"),
                    event,
                }),
                "the resume routes the Subagent's work to it again"
            );
        }
    }

    #[test]
    fn a_resume_a_sibling_sent_is_that_siblings_even_once_it_settled() {
        let mut correlation = with_settled_subagent();
        project_attributed(&mut correlation, spawn_started("agent-2", "t-sibling"));
        project_attributed(
            &mut correlation,
            agent_event(
                "agent-2",
                "subagent.completed",
                json!({ "toolCallId": "t-sibling", "agentName": "researcher", "agentDisplayName": "Researcher" }),
            ),
        );
        assert_eq!(
            project_attributed(
                &mut correlation,
                delivered_message("agent-1", "queued", Some("agent-agent-2"), "Say MANGO."),
            ),
            [AttributedProviderEvent {
                attribution: subagent("agent-2"),
                event: resumed("agent-1", "Say MANGO.", Some("Say MANGO.")),
            }]
        );
    }

    #[test]
    fn a_resume_carries_the_model_the_subagent_last_ran_on() {
        let mut correlation = with_subagent();
        project_attributed(
            &mut correlation,
            agent_event(
                "agent-1",
                "assistant.usage",
                json!({ "model": "gpt-5.6-luna", "inputTokens": 10, "outputTokens": 2 }),
            ),
        );
        project_attributed(
            &mut correlation,
            agent_event(
                "agent-1",
                "subagent.completed",
                json!({ "toolCallId": "t-spawn", "agentName": "researcher", "agentDisplayName": "Researcher" }),
            ),
        );
        assert_eq!(
            project_attributed(
                &mut correlation,
                delivered_message("agent-1", "idle", Some(MAIN_AGENT), "Once more."),
            ),
            [
                AttributedProviderEvent {
                    attribution: ProviderEventAttribution::OwningSession,
                    event: resumed("agent-1", "Once more.", Some("Once more.")),
                },
                AttributedProviderEvent {
                    attribution: ProviderEventAttribution::OwningSession,
                    event: ProviderEvent::SubagentModelChanged {
                        subagent_id: ProviderSubagentId::new("agent-1"),
                        model: crate::protocol::ModelId::new("gpt-5.6-luna"),
                    },
                },
            ],
            "Copilot reports a Subagent's Model change only at spawn, so it follows the resume"
        );
    }

    #[test]
    fn a_resume_that_carries_nothing_to_read_is_described_as_the_subagent_was() {
        let mut correlation = with_settled_subagent();
        assert_eq!(
            project_attributed(
                &mut correlation,
                delivered_message("agent-1", "queued", None, "  "),
            ),
            [AttributedProviderEvent {
                attribution: ProviderEventAttribution::OwningSession,
                event: resumed("agent-1", "Scout the workspace", None),
            }]
        );
    }

    #[test]
    fn the_spawns_own_prompt_is_no_resume() {
        let mut correlation = with_subagent();
        assert!(
            project_attributed(
                &mut correlation,
                delivered_message("agent-1", "idle", Some(MAIN_AGENT), "Scout the workspace."),
            )
            .is_empty(),
            "the spawn's prompt arrives before the Subagent has worked, and its spawn already \
             carried it in"
        );
    }

    #[test]
    fn a_resumed_turn_settles_at_the_turn_end_after_a_message_requesting_no_tools() {
        let mut correlation = with_settled_subagent();
        project_attributed(
            &mut correlation,
            delivered_message("agent-1", "idle", Some(MAIN_AGENT), "Run it again."),
        );
        project_attributed(
            &mut correlation,
            loop_message("agent-1", "r1", "", &["t-bash"]),
        );
        assert!(
            project_attributed(&mut correlation, turn_end("agent-1", "0")).is_empty(),
            "a model message requesting tools keeps the loop going"
        );
        project_attributed(
            &mut correlation,
            loop_message("agent-1", "r2", "Done.", &[]),
        );
        project_attributed(
            &mut correlation,
            agent_event(
                "agent-1",
                "assistant.reasoning",
                json!({ "reasoningId": "rs1", "content": "Wrapping up." }),
            ),
        );
        assert_eq!(
            project_attributed(&mut correlation, turn_end("agent-1", "1")),
            [settled("agent-1", ProviderSubagentStatus::Completed)],
            "the loop exits at the turn end after an answer requesting no tools"
        );
        assert!(
            project_attributed(
                &mut correlation,
                loop_message("agent-1", "r3", "Late.", &[])
            )
            .is_empty(),
            "the settled Subagent is routed no longer"
        );
        assert!(
            project_attributed(&mut correlation, turn_end("agent-1", "2")).is_empty(),
            "and its stretch settles once"
        );
    }

    #[test]
    fn a_spawns_turn_end_settles_nothing() {
        let mut correlation = with_subagent();
        project_attributed(
            &mut correlation,
            loop_message("agent-1", "s1", "Found it.", &[]),
        );
        assert!(
            project_attributed(&mut correlation, turn_end("agent-1", "0")).is_empty(),
            "Copilot settles a spawn's stretch itself, with `subagent.completed`"
        );
    }

    #[test]
    fn a_later_message_settles_a_resume_still_open_before_beginning_the_next() {
        let mut correlation = with_settled_subagent();
        project_attributed(
            &mut correlation,
            delivered_message("agent-1", "queued", Some(MAIN_AGENT), "First."),
        );
        project_attributed(
            &mut correlation,
            agent_event(
                "agent-1",
                "assistant.message_delta",
                json!({ "messageId": "r1", "deltaContent": "Half" }),
            ),
        );
        assert_eq!(
            project_attributed(
                &mut correlation,
                delivered_message("agent-1", "queued", Some(MAIN_AGENT), "Second."),
            ),
            [
                AttributedProviderEvent {
                    attribution: subagent("agent-1"),
                    event: ProviderEvent::AgentMessageCompleted,
                },
                settled("agent-1", ProviderSubagentStatus::Completed),
                AttributedProviderEvent {
                    attribution: ProviderEventAttribution::OwningSession,
                    event: resumed("agent-1", "Second.", Some("Second.")),
                },
            ],
            "Copilot delivers the next message only once the previous run has ended"
        );
    }

    #[test]
    fn the_loops_idle_settles_a_resume_still_open() {
        let mut correlation = with_settled_subagent();
        project_attributed(
            &mut correlation,
            delivered_message("agent-1", "queued", Some(MAIN_AGENT), "Again."),
        );
        assert_eq!(
            project_attributed(&mut correlation, event("session.idle", json!({}))),
            [
                settled("agent-1", ProviderSubagentStatus::Completed),
                AttributedProviderEvent {
                    attribution: ProviderEventAttribution::OwningSession,
                    event: ProviderEvent::TurnCompleted,
                },
            ]
        );
    }

    #[test]
    fn an_aborted_idle_settles_a_resume_interrupted() {
        let mut correlation = with_settled_subagent();
        project_attributed(
            &mut correlation,
            delivered_message("agent-1", "queued", Some(MAIN_AGENT), "Again."),
        );
        assert_eq!(
            project_attributed(
                &mut correlation,
                event("session.idle", json!({ "aborted": true }))
            ),
            [
                settled("agent-1", ProviderSubagentStatus::Interrupted),
                AttributedProviderEvent {
                    attribution: ProviderEventAttribution::OwningSession,
                    event: ProviderEvent::TurnInterrupted,
                },
            ]
        );
    }

    #[test]
    fn a_resume_the_main_agent_sends_with_no_turn_running_begins_a_continuation() {
        let mut correlation = with_settled_subagent();
        project(&mut correlation, "session.idle", json!({}));
        assert!(!correlation.is_turn_running());
        assert_eq!(
            project_attributed(
                &mut correlation,
                delivered_message("agent-1", "idle", Some(MAIN_AGENT), "Again."),
            ),
            [AttributedProviderEvent {
                attribution: ProviderEventAttribution::OwningSession,
                event: resumed("agent-1", "Again.", Some("Again.")),
            }]
        );
        assert!(
            correlation.is_turn_running(),
            "the resume's row stands in a Continuation the loop's next idle settles"
        );
        assert_eq!(
            project_attributed(&mut correlation, event("session.idle", json!({}))),
            [
                settled("agent-1", ProviderSubagentStatus::Completed),
                AttributedProviderEvent {
                    attribution: ProviderEventAttribution::OwningSession,
                    event: ProviderEvent::TurnCompleted,
                },
            ]
        );
    }

    #[test]
    fn an_agent_idle_notification_projects_nothing() {
        let mut correlation = with_settled_subagent();
        project_attributed(
            &mut correlation,
            delivered_message("agent-1", "queued", Some(MAIN_AGENT), "Again."),
        );
        assert!(
            project(
                &mut correlation,
                "system.notification",
                json!({
                    "content": "<system_notification>\nAgent \"researcher\" has finished.\n</system_notification>",
                    "kind": {
                        "type": "agent_idle",
                        "agentId": "agent-1",
                        "agentType": "task",
                        "description": "Scout the workspace",
                        "displayName": "Researcher",
                    },
                }),
            )
            .is_empty(),
            "Copilot names a resumed agent idle in some runs and not others, so it settles nothing"
        );
    }
}
