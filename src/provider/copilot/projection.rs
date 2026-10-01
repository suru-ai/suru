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
//! that stretch. The spawning tool execution itself projects no row while the Subagent row answers
//! for the delegation ([`SPAWN_TOOL`](super::tools::SPAWN_TOOL)). Main-conversation content
//! outside a Turn Suru is running is dropped unless owed a Continuation; context snapshots can
//! refresh idle Sessions.
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
//! Transcript ([`WRITE_AGENT_TOOL`](super::tools::WRITE_AGENT_TOOL)). Nor does a Broker call
//! that spawns, sends to, or stops a Subagent, which the Broker's own rows answer for.
//!
//! Which Activity records a tool execution is decided once, as it starts ([`ToolDisposition`]):
//! a shell execution is a Command, an edit a File Change naming the files it touches, an execution
//! another Activity records or that is Copilot's plumbing is nothing, and every other execution is
//! a Tool Call — named, given its arguments as input, streamed its partial results, and settled by
//! its completion. A File Change is settled by its completion too, but streams nothing: it records
//! which files changed and how, never what the Tool reported of it.
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
//!
//! Copilot compacting a conversation's context is a **Compaction** of that conversation:
//! `session.compaction_start` begins it and `session.compaction_complete` settles it, completed
//! with the `preCompactionTokens` and `postCompactionTokens` it measured and the `summaryContent`
//! it left, or failed with its `error`, so a failed attempt and its retry are two. The `model.*` events of the summarising
//! call, like `session.truncation`, project nothing. Copilot compacts in the background of the
//! Session rather than in a stretch of its loop, so the main conversation's compaction holds the
//! stretch it fell in open past the loop's idle — a steer meanwhile hands the stretch back to the
//! loop it wakes — and one reported while no Turn runs begins a Continuation of its own that its
//! settle settles (ADR 0042, [`CopilotCorrelation::project_compaction`]). Copilot owns any
//! Continuation it compacts in, so stopping it — by interrupt or by the next Prompt — cancels the
//! compaction (`session.history.cancelBackgroundCompaction`), which the loop's abort leaves
//! running. A Subagent's compaction is followed across its stretches, so one its stretch settled
//! without ends nowhere, least of all in the stretch resuming it, and one Copilot begins while the
//! Subagent works no stretch wakes it into a Continuation of its own Session, which the
//! compaction's end settles.
//!
//! A Compaction the user asks for is Copilot's `session.history.compact`, which runs no stretch of
//! the loop and reports no turn of its own: Suru opens the Turn it is asked in
//! ([`CopilotCorrelation::begin_manual_compaction`]), the same `session.compaction_start` and
//! `session.compaction_complete` report the compaction in it, and Copilot's answer to the request
//! settles it ([`CopilotCorrelation::project_manual_compaction_answer`]). Only reports under the
//! manual trigger Suru asks with decide anything of that Turn: one under another trigger, or none,
//! could be the end of a background compaction Suru stopped following, and records nothing there.
//! The SDK hands answers and events to Suru apart, so the answer waits for the compaction's own
//! end, whatever it says, so that its counts and the `compactionTokensUsed` that is the Turn's
//! Usage land on the Compaction and its Turn — unless Copilot rejected the request outright,
//! before it could compact anything. Only an end that never reaches Suru within the bound every
//! request waits under leaves the Turn to settle on the answer alone; the reports Copilot then
//! still owes would carry the trigger the next manual compaction's do, so the next request is
//! turned away rather than begun while they could still arrive. Stopping it is
//! `session.history.abortManualCompaction`, after which Copilot fails the compaction and the
//! request as cancelled: that is the stop Suru asked for, and settles both as interrupted. It is
//! no background compaction, so neither the background cancel nor the loop's abort ever reaches
//! it.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
};

use futures_util::stream;
use github_copilot_sdk::{
    EventSubscription, SessionEvent,
    rpc::{TaskShellInfo, TaskShellInfoAttachmentMode, TaskStatus},
    session::Session as NativeSession,
    session_events::{
        AssistantMessageData, AssistantMessageDeltaData, AssistantMessageStartData,
        AssistantReasoningData, AssistantReasoningDeltaData, AssistantUsageData,
        SessionCompactionCompleteData, SessionErrorData, SessionEventType, SessionIdleData,
        SubagentCompletedData, SubagentFailedData, SubagentStartedData, SystemNotificationData,
        ToolExecutionCompleteContent, ToolExecutionCompleteData, ToolExecutionPartialResultData,
        ToolExecutionStartData, UserMessageData, UserMessageDelivery,
    },
    subscription::RecvErrorKind,
};
use tokio::{
    sync::{Notify, mpsc},
    time::{Duration, timeout},
};

use super::super::command_presentation::PresentedCommand;
use super::{
    COPILOT_FAILURE_FALLBACK, COPILOT_HARNESS_NAME, copilot_error,
    event_drain::EventDrainCheckpoint,
    pricing::CopilotPricing,
    session::until_crash,
    skills::CopilotSkills,
    tools::{PresentedToolCall, ToolDisposition, presented_tool_call, tool_activity_id},
    transport::CopilotConnection,
};
use crate::protocol::{AgentSelection, ContextFill, Cost, NativeMeter, TurnId, Usage};
use crate::provider::{
    AttributedProviderEvent, ContextFillReport, ProviderActivityId, ProviderCommandStatus,
    ProviderError, ProviderEvent, ProviderEventAttribution, ProviderEventStream,
    ProviderFileChangeStatus, ProviderSubagentId, ProviderSubagentStatus, ProviderToolCallStatus,
    ProviderWatchId, ProviderWatchOutcome, ReportedTurnMetering, concise_remote_message,
    exclusive_count, first_line,
    harness::SharedHarnessHandle,
    reasoning::{ReasoningSegment, ReasoningSummarySplitter},
    reported_count,
};

/// Everything the projection must remember between events for one Copilot Session.
pub(super) struct CopilotCorrelation {
    /// Where the Session works, which is where a relative path a Tool names a file by is looked up.
    execution_directory: PathBuf,
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
    /// Where Copilot is with compacting the main conversation's context.
    compaction: Compacting,
    /// Counts the main conversation's stretches, so an interrupt still at work once the stretch it
    /// was for has settled stops nothing begun after it.
    stretches: u64,
    /// Raised when the timeline carries Copilot's next idle, for an interrupt waiting on the idle
    /// its abort ends in.
    abort_idle: Option<Arc<Notify>>,
    /// An abort an interrupt gave up waiting on still owes the aborted idle it ends in, which
    /// belongs to the stretch the interrupt settled and so is swallowed wherever it lands. Copilot's
    /// idle names no run, so this is a count of one rather than a match: a genuine idle of a later
    /// Turn is never taken for it, since only an aborted idle is swallowed, and an aborted idle
    /// follows only an abort Suru sends — each of which forgives the debt before it goes out.
    owes_aborted_idle: bool,
    /// The Agent Selection the Copilot Session runs under, which a Continuation Copilot owns
    /// begins under.
    selection: AgentSelection,
    /// Copilot still owes reports of a manual compaction whose Turn settled on its answer alone.
    /// They carry the manual trigger a new manual compaction's would, so none begins until they
    /// are in — or until a stretch of the loop has run to its idle since, which every report
    /// Copilot wrote before that answer has reached Suru ahead of.
    manual_reports_owed: bool,
}

/// Where Copilot is with compacting one conversation's context. It compacts in the background of
/// the Session rather than in a stretch of the conversation's loop, so a compaction can outlive
/// the stretch it began in — the main loop going idle, or a Subagent's stretch settling — and can
/// run while none does.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum Compacting {
    #[default]
    Idle,
    /// Copilot is summarising, and its `session.compaction_complete` is still to come.
    Running,
    /// Copilot is summarising, and an interrupt has asked it to stop: until Copilot answers, the
    /// compaction still holds its stretch open, and the interrupt settles that stretch.
    Cancelling,
    /// Copilot reported the compaction failing while the interrupt's cancel was still out. Only
    /// Copilot's answer says whether that failure is the cancel taking or the compaction's own, so
    /// the report is kept, and the stretch held, until it does.
    FailedWhileCancelling { error: Option<String> },
    /// The cancel found nothing to cancel, so the failure Copilot reported while it was out was
    /// the compaction's own: its Compaction settles failed, with Copilot's error, ahead of the
    /// stretch the interrupt stops ([`CopilotCorrelation::settle_turn`]).
    Failed { error: Option<String> },
    /// Suru stopped following the compaction — cancelled it, or the stretch it stood in settled
    /// without it — and its Compaction settled with that stretch. Its
    /// `session.compaction_complete` is owed nothing, so it records nothing in whatever stretch
    /// runs by the time it arrives — unless another compaction starts first, which is read as the
    /// stopped one's end never coming. That ordering is assumed rather than verified against a live
    /// CLI: Copilot is taken to run one compaction of a conversation at a time and to report its
    /// end before the next one's start.
    Stopped,
}

impl Compacting {
    /// Whether the compaction still holds the stretch it stands in open.
    fn holds_stretch(&self) -> bool {
        matches!(
            self,
            Self::Running | Self::Cancelling | Self::FailedWhileCancelling { .. }
        )
    }

    /// The compaction the stretch it stood in settled without: its end is owed nothing, or has
    /// already come.
    fn outlived(&mut self) {
        match self {
            Self::Running | Self::Cancelling => *self = Self::Stopped,
            Self::FailedWhileCancelling { .. } | Self::Failed { .. } => *self = Self::Idle,
            Self::Idle | Self::Stopped => {}
        }
    }

    /// Reads a compaction report, returning whether it is one Suru follows: a start always is,
    /// and an end is unless it ends a compaction Suru stopped following.
    fn follows(&mut self, started: bool) -> bool {
        let before = std::mem::replace(self, if started { Self::Running } else { Self::Idle });
        started || before != Self::Stopped
    }
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
    /// The idle the loop reported while Copilot was still compacting — `Some(aborted)` — which
    /// the stretch settles on once the compaction does. A Continuation a compaction began has its
    /// loop idle from the start. Loop output arriving, or a steer about to wake the loop, clears
    /// it: the loop is running again, and its own next idle is the stretch's.
    idled: Option<bool>,
    /// Whether Copilot owns this Continuation — it was reported with
    /// [`ProviderEvent::ContinuationStarted`] — so that the next Prompt or an interrupt stops what
    /// Copilot runs in it rather than Suru settling it alone.
    owned: bool,
    /// The manual compaction this Turn was begun for (ADR 0041), which Copilot runs as
    /// `session.history.compact` rather than a stretch of its loop: no idle ends it, and Copilot's
    /// answer to the request settles it. Nothing for any other Turn.
    manual: Option<ManualCompactionRun>,
}

impl ActiveTurn {
    fn new() -> Self {
        Self {
            continuation: false,
            streams: ConversationStreams::default(),
            failure: None,
            idled: None,
            owned: false,
            manual: None,
        }
    }

    fn continuation() -> Self {
        Self {
            continuation: true,
            ..Self::new()
        }
    }
}

/// Where a manual compaction Copilot runs for `session.history.compact` stands, as its reports,
/// Suru's abort of it and Copilot's answer have said.
#[derive(Debug)]
struct ManualCompactionRun {
    /// The Turn the request began, which Copilot's answer is matched to.
    turn_id: TurnId,
    reported: ManualReport,
    /// Copilot's account of why the compaction failed, from its `session.compaction_complete`,
    /// which the request's answer does not carry.
    failure: Option<String>,
    /// Copilot answered Suru's `session.history.abortManualCompaction` saying it aborted the
    /// compaction, so a failure from here on is that abort.
    aborted: bool,
    /// Copilot's answer to the request, held while the compaction's own end has still to reach
    /// Suru.
    answer: Option<ManualCompactionAnswer>,
}

impl ManualCompactionRun {
    fn new(turn_id: TurnId) -> Self {
        Self {
            turn_id,
            reported: ManualReport::Unreported,
            failure: None,
            aborted: false,
            answer: None,
        }
    }

    /// Whether Copilot's answer settles the Turn now rather than once the compaction's own end
    /// has reached Suru: only when that end is in, or when Copilot rejected the request outright,
    /// before it could compact anything. Any other answer may still have reports on their way,
    /// which the Turn waits for, however the answer reads.
    fn settles_on(&self, answer: &ManualCompactionAnswer) -> bool {
        match self.reported {
            ManualReport::Ended => true,
            ManualReport::Running => false,
            ManualReport::Unreported => matches!(
                answer,
                ManualCompactionAnswer::NotCompacted { rejected: true, .. }
            ),
        }
    }

    /// Whether Copilot still owes reports of this compaction once its Turn settles: the end of
    /// one it reported starting, or both reports of one it answered it made.
    fn owes_reports(&self) -> bool {
        match self.reported {
            ManualReport::Ended => false,
            ManualReport::Running => true,
            ManualReport::Unreported => {
                matches!(self.answer, Some(ManualCompactionAnswer::Compacted { .. }))
            }
        }
    }
}

/// How far Copilot has reported a manual compaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManualReport {
    Unreported,
    Running,
    Ended,
}

/// Copilot's answer to `session.history.compact`, which settles the Turn the request runs in.
#[derive(Debug)]
pub(super) enum ManualCompactionAnswer {
    /// Copilot compacted the Session's context, leaving `summary` where its answer carried one.
    Compacted { summary: Option<String> },
    /// Copilot compacted nothing: it failed, or an abort cancelled it. `error` is its account of
    /// why, where the answer carried one. `rejected` says Copilot rejected the request outright,
    /// as a request it could not take, before it could compact anything or report doing so.
    NotCompacted {
        error: Option<String>,
        rejected: bool,
    },
}

/// Where Copilot's answer to a manual compaction joins the Session's timeline. Holds the timeline
/// only weakly, so the timeline still ends when Copilot's does.
#[derive(Clone)]
pub(super) struct ManualCompactionAnswers {
    timeline: mpsc::WeakUnboundedSender<Result<TimelineEvent, ProviderError>>,
}

impl ManualCompactionAnswers {
    /// Puts `answer`, Copilot's answer to the manual compaction `turn_id` was begun for, on the
    /// timeline. The SDK hands answers and events to Suru apart, so the compaction's own reports
    /// may still be on their way: should the Turn still be waiting on them `bound` on, the
    /// timeline then says so, so that the Turn settles on the answer.
    pub(super) async fn answer(
        &self,
        turn_id: TurnId,
        answer: ManualCompactionAnswer,
        bound: Duration,
    ) {
        if !self.send(TimelineEvent::ManualCompactionAnswered { turn_id, answer }) {
            return;
        }
        tokio::time::sleep(bound).await;
        self.send(TimelineEvent::ManualCompactionOverdue { turn_id });
    }

    /// Puts `event` on the timeline, answering whether the timeline was still there to take it.
    fn send(&self, event: TimelineEvent) -> bool {
        self.timeline
            .upgrade()
            .is_some_and(|timeline| timeline.send(Ok(event)).is_ok())
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
    /// A compaction Copilot began while the Subagent worked no stretch: a Continuation of the
    /// Subagent's own Session, in which no loop runs, which the compaction ending settles.
    Compacting,
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
    /// Where Copilot is with compacting the Subagent's context, followed across its stretches: a
    /// compaction its stretch settled without must not end in the stretch that resumes it.
    compaction: Compacting,
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
    /// The tool executions the conversation is still running, by the tool call identity Copilot
    /// gave each. Copilot runs several Tools at once, so a conversation holds as many as it
    /// started.
    tools: HashMap<String, RunningTool>,
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

/// A tool execution Copilot is still running, by what records it — and for the kinds a row records,
/// the output it has streamed so far, against which the completed execution's repeat of it is
/// reconciled.
enum RunningTool {
    /// A shell execution, recorded as a Command.
    Command { streamed_output: String },
    /// A file tool's execution, recorded as a File Change, which keeps none of its output.
    FileChange,
    /// Every other execution no more specific Activity records, recorded as a Tool Call.
    ToolCall { streamed_output: String },
    /// A [`SPAWN_TOOL`](super::tools::SPAWN_TOOL) execution, whose Tool Call is held back while
    /// the Subagent row answers for the delegation. A spawn that never opens its Subagent has no
    /// row answering for it, so the withheld Tool Call surfaces when the execution settles or the
    /// conversation stops — a failed delegation stays visible.
    Spawn {
        withheld: PresentedToolCall,
        /// What the execution hands the Subagent it spawns — the tool's `prompt` argument — kept
        /// for the `subagent.started` that opens it, which names the spawning tool call but never
        /// carries the prompt itself.
        prompt: Option<String>,
        streamed_output: String,
    },
    /// An execution another Activity records, or Copilot's plumbing, which projects nothing at
    /// all, whatever it came to.
    Unrecorded,
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
        Self::working_in(
            std::env::temp_dir(),
            CopilotPricing::default(),
            tests::selection(),
        )
    }

    /// The projection for a Session working in `execution_directory`, costing its usage at
    /// `pricing`, under `selection`.
    pub(super) fn working_in(
        execution_directory: PathBuf,
        pricing: CopilotPricing,
        selection: AgentSelection,
    ) -> Self {
        Self {
            execution_directory,
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
            compaction: Compacting::Idle,
            stretches: 0,
            abort_idle: None,
            owes_aborted_idle: false,
            selection,
            manual_reports_owed: false,
        }
    }

    /// Opens the Turn a Prompt is about to be delivered into. Copilot hosts one agentic loop per
    /// Session, so a second Turn cannot begin while a prompted one is running — but a Prompt
    /// delivered while a Continuation runs settles that Continuation rather than steering it
    /// (ADR 0015), so the stretch it cuts off becomes a stale one whose idle is owed nothing —
    /// unless its loop is already idle, and only a compaction held it open. A compaction the
    /// Continuation held settled with it, so Copilot's report of that one ending is owed nothing
    /// either.
    pub(super) fn begin_turn(&mut self) -> Result<(), ProviderError> {
        match self.turn.take() {
            None => {}
            Some(stretch) if stretch.continuation => {
                // Orchestration already settled the Continuation and the store settles whatever
                // its streams left open, so the stretch's state goes with it.
                if stretch.idled.is_none() {
                    self.stale_stretches += 1;
                }
                self.compaction.outlived();
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
        self.stretches += 1;
        self.turn = Some(ActiveTurn::new());
        self.late_settle_owes_continuation = false;
        Ok(())
    }

    /// Bind observations at receipt, before the projection queue can fall behind a
    /// later Prompt or Model change. Startup reports during selection still belong
    /// to the previous Turn until the new Prompt is ready to be sent, by which time
    /// `selection` is in force on the Copilot Session.
    pub(super) fn context_prompt_ready(&mut self, turn_id: TurnId, selection: AgentSelection) {
        self.context_prompt_pending = false;
        self.context_turn = Some(turn_id);
        self.context_continuation = false;
        self.selection = selection;
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

    /// Opens the Turn a Compaction request began (ADR 0041), which Copilot runs as
    /// `session.history.compact`. Suru asks only while the Session is idle, so no Turn may be
    /// running: Copilot would take the compaction mid-Turn, report success, and lose it. Context
    /// readings bind to it once its Agent Selection is in force, as a Prompt's Turn's do
    /// ([`Self::context_prompt_ready`]).
    ///
    /// Nor does one begin while Copilot still owes the reports of an earlier manual compaction,
    /// which would be taken for this one's: the request is turned away, saying so.
    pub(super) fn begin_manual_compaction(&mut self, turn_id: TurnId) -> Result<(), ProviderError> {
        if self.turn.is_some() {
            return Err(copilot_error(
                "Copilot was asked to compact while a Turn was active",
            ));
        }
        if self.manual_reports_owed {
            return Err(copilot_error(
                "Copilot compaction refused: Copilot has yet to report an earlier compaction it \
                 answered, and that report could be taken for this one's; ask again once it has, \
                 or after the Agent's next answer",
            ));
        }
        self.context_continuation = false;
        self.context_prompt_pending = true;
        self.stretches += 1;
        self.turn = Some(ActiveTurn {
            manual: Some(ManualCompactionRun::new(turn_id)),
            ..ActiveTurn::new()
        });
        self.late_settle_owes_continuation = false;
        Ok(())
    }

    /// Whether the running Turn is a manual compaction's, which an interrupt aborts as one.
    pub(super) fn is_compacting_on_request(&self) -> bool {
        self.turn.as_ref().is_some_and(|turn| turn.manual.is_some())
    }

    /// Copilot answered Suru's abort of the manual compaction: `aborted` says it found one to
    /// abort. One it did not find had already ended, and its answer settles the Turn as it ended.
    pub(super) fn manual_compaction_aborted(&mut self, aborted: bool) {
        if let Some(manual) = self.turn.as_mut().and_then(|turn| turn.manual.as_mut()) {
            manual.aborted = aborted;
        }
    }

    /// Projects one of Copilot's reports on compacting the main conversation, as whichever of its
    /// two operations it belongs to. Suru asks for a manual compaction with the manual trigger,
    /// and Copilot reports it under that trigger; anything else it compacts is a background
    /// compaction ([`Self::project_compaction`]). Only a report under the manual trigger decides
    /// anything of a manual compaction's Turn: one under another trigger, or none, could be the
    /// end of a background compaction Suru stopped following — which it then is, owed nothing —
    /// and Copilot runs no other compaction of the conversation meanwhile. A report under the
    /// manual trigger with no manual compaction's Turn running belongs to one that has already
    /// settled, and is owed nothing either.
    fn project_main_compaction_report(
        &mut self,
        event: &SessionEvent,
    ) -> Vec<AttributedProviderEvent> {
        let started = event.parsed_type() == SessionEventType::SessionCompactionStart;
        let trigger = event
            .data
            .get("trigger")
            .and_then(serde_json::Value::as_str);
        let manual = trigger == Some(MANUAL_TRIGGER);
        if self.is_compacting_on_request() {
            if manual {
                return self.project_manual_compaction(event);
            }
            if self.compaction == Compacting::Stopped && !started {
                self.compaction = Compacting::Idle;
            } else {
                tracing::warn!(
                    trigger,
                    "Copilot reported a compaction under no manual trigger while it ran a manual one"
                );
            }
            return Vec::new();
        }
        if manual {
            if !started {
                self.manual_reports_owed = false;
            }
            return Vec::new();
        }
        self.project_compaction(compaction_event(event))
    }

    /// Projects one of Copilot's reports on the manual compaction the running Turn was begun for:
    /// its start, and its end, with what the summarising call spent as the Turn's Usage — and,
    /// once Copilot has answered the request, the Turn's settle.
    fn project_manual_compaction(&mut self, event: &SessionEvent) -> Vec<AttributedProviderEvent> {
        let Some(turn) = self.turn.as_mut().filter(|turn| turn.manual.is_some()) else {
            return Vec::new();
        };
        let manual = turn
            .manual
            .as_mut()
            .expect("a manual compaction's Turn holds it");
        let reported = compaction_event(event);
        match &reported {
            ProviderEvent::CompactionStarted => manual.reported = ManualReport::Running,
            ProviderEvent::CompactionFailed { error } => {
                manual.reported = ManualReport::Ended;
                manual.failure.clone_from(error);
            }
            _ => manual.reported = ManualReport::Ended,
        }
        let ended = manual.reported == ManualReport::Ended;
        let answered = manual.answer.is_some();
        let mut projected = vec![attributed(None, reported)];
        if ended {
            if let Some(usage) = compaction_usage(event, &self.pricing, &mut turn.streams) {
                projected.push(attributed(None, usage));
            }
            if answered {
                projected.extend(self.settle_manual_compaction());
            }
        }
        projected
    }

    /// Takes Copilot's answer to the manual compaction `turn_id` was begun for, which settles its
    /// Turn — at once, unless the compaction's own end has still to reach Suru
    /// ([`ManualCompactionRun::settles_on`]); then that end settles it. An answer for a Turn no
    /// longer running is owed nothing.
    fn project_manual_compaction_answer(
        &mut self,
        turn_id: TurnId,
        answer: ManualCompactionAnswer,
    ) -> Vec<AttributedProviderEvent> {
        let Some(manual) = self.running_manual_compaction(turn_id) else {
            return Vec::new();
        };
        let settles = manual.settles_on(&answer);
        manual.answer = Some(answer);
        if settles {
            self.settle_manual_compaction()
        } else {
            Vec::new()
        }
    }

    /// The manual compaction `turn_id` was begun for answered a bound ago, and its own end has
    /// still not reached Suru: its Turn settles on the answer alone, and the reports, should they
    /// come, are owed nothing ([`Self::begin_manual_compaction`]).
    fn project_manual_compaction_overdue(
        &mut self,
        turn_id: TurnId,
    ) -> Vec<AttributedProviderEvent> {
        let Some(manual) = self.running_manual_compaction(turn_id) else {
            return Vec::new();
        };
        if manual.answer.is_none() {
            return Vec::new();
        }
        tracing::warn!(
            reported = ?manual.reported,
            "Copilot's report of a manual compaction it answered never reached Suru; its Turn \
             settles on the answer, without the counts and Usage the report would carry"
        );
        self.settle_manual_compaction()
    }

    /// The manual compaction the running Turn was begun for, if it is `turn_id`'s.
    fn running_manual_compaction(&mut self, turn_id: TurnId) -> Option<&mut ManualCompactionRun> {
        self.turn
            .as_mut()
            .and_then(|turn| turn.manual.as_mut())
            .filter(|manual| manual.turn_id == turn_id)
    }

    /// Settles the manual compaction's Turn on Copilot's answer to the request. A compaction it
    /// answered it made whose end never reached Suru completes as the answer says. One that did
    /// not compact fails the Turn with Copilot's account of why — or stops it, when Suru's abort
    /// is what cancelled it.
    fn settle_manual_compaction(&mut self) -> Vec<AttributedProviderEvent> {
        let mut turn = self
            .turn
            .take()
            .expect("a manual compaction's Turn is running");
        let manual = turn
            .manual
            .take()
            .expect("a manual compaction's Turn holds it");
        self.manual_reports_owed = manual.owes_reports();
        let mut projected = Vec::with_capacity(2);
        let outcome = match manual
            .answer
            .expect("a manual compaction's Turn settles on Copilot's answer")
        {
            ManualCompactionAnswer::Compacted { summary } => {
                if manual.reported != ManualReport::Ended {
                    projected.push(ProviderEvent::CompactionCompleted {
                        before_tokens: None,
                        after_tokens: None,
                        summary,
                    });
                }
                ProviderEvent::TurnCompleted
            }
            ManualCompactionAnswer::NotCompacted { .. } if manual.aborted => {
                ProviderEvent::TurnInterrupted
            }
            ManualCompactionAnswer::NotCompacted { error, .. } => ProviderEvent::TurnFailed {
                message: manual
                    .failure
                    .or(error)
                    .unwrap_or_else(|| NOT_COMPACTED.to_owned()),
            },
        };
        let reasoning = if matches!(outcome, ProviderEvent::TurnCompleted) {
            OpenReasoning::Complete
        } else {
            OpenReasoning::Release
        };
        projected.extend(settle_open_streams(
            &mut turn.streams,
            &self.delegations,
            reasoning,
        ));
        projected.push(outcome);
        projected
            .into_iter()
            .map(|settled| attributed(None, settled))
            .collect()
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
        self.open_main_turn().map(|turn| {
            // Output is the loop running, whatever idle it reported while a compaction held the
            // stretch open: the stretch is the loop's again, and its next idle settles it.
            turn.idled = None;
            &mut turn.streams
        })
    }

    /// The main conversation's Turn, as [`Self::open_main_streams`] opens it, for a report that
    /// is no output of the loop's.
    fn open_main_turn(&mut self) -> Option<&mut ActiveTurn> {
        if self.turn.is_none() {
            if !self.owes_late_output() {
                return None;
            }
            self.begin_continuation();
        }
        self.turn.as_mut()
    }

    /// Begins the Continuation stretch that main work arriving with no Turn active lands in, which
    /// the loop's next idle settles.
    fn begin_continuation(&mut self) {
        self.stretches += 1;
        self.turn = Some(ActiveTurn::continuation());
        self.context_continuation = true;
        self.late_settle_owes_continuation = false;
    }
}

/// What an interrupt is for, as it begins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct InterruptScope {
    /// The main conversation's stretch it stops, if one is running.
    stretch: Option<u64>,
    /// Copilot is compacting the main conversation's context, which its loop's abort leaves
    /// running, so the interrupt cancels it first.
    pub(super) compacting: bool,
}

/// What an interrupt has left to do once any compaction it cancelled has stopped.
#[derive(Debug, PartialEq)]
pub(super) struct InterruptRemainder {
    /// Copilot's loop is running for the stretch the interrupt is for — the main conversation's,
    /// or a Subagent's working past it — which only its abort stops.
    pub(super) abort: bool,
    /// What settles a stretch whose loop had already stopped, which nothing else will report.
    pub(super) settled: Vec<AttributedProviderEvent>,
    /// The abort is for Subagents working past a stretch whose loop had already stopped, which
    /// the idle it ends in settles: the interrupt is not over until the timeline has carried it.
    pub(super) awaits_idle: bool,
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
/// when the shared harness process hosting it dies, beside where the Session's manual compactions
/// put Copilot's answers on that timeline.
pub(super) fn provider_events(
    subscription: EventSubscription,
    harness: Arc<SharedHarnessHandle<CopilotConnection>>,
    drain: EventDrainCheckpoint,
    correlation: Arc<StdMutex<CopilotCorrelation>>,
    skills: CopilotSkills,
    approvals: Arc<super::approval::CopilotApprovals>,
    roster: TaskRosterSource,
) -> (ProviderEventStream, ManualCompactionAnswers) {
    // The SDK drops the oldest events on a subscriber that falls behind, and a dropped delta is
    // Transcript content Suru cannot get back, so the timeline is drained as fast as it arrives
    // and queued here rather than at the pace the Session's consumer reads.
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let answers = ManualCompactionAnswers {
        timeline: events_tx.downgrade(),
    };
    tokio::spawn(drain_session_timeline(
        subscription,
        events_tx,
        drain.clone(),
        correlation.clone(),
    ));
    let events = Box::pin(stream::unfold(
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
    ));
    (events, answers)
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
                let main_idle = event.parsed_type() == SessionEventType::SessionIdle
                    && event.agent_id.is_none();
                log_context_contents(&event);
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
                if main_idle {
                    correlation
                        .lock()
                        .expect("Copilot correlation lock is not poisoned")
                        .idle_carried();
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

/// Logs what Copilot reports filling the context window with: the breakdown behind each context
/// report, and the MCP servers the Session loaded, whose tool definitions count against it.
fn log_context_contents(event: &SessionEvent) {
    let count = |field: &str| event.data.get(field).and_then(serde_json::Value::as_i64);
    match event.parsed_type() {
        SessionEventType::SessionUsageInfo => tracing::debug!(
            agent = event.agent_id.as_deref(),
            current = count("currentTokens"),
            system = count("systemTokens"),
            tool_definitions = count("toolDefinitionsTokens"),
            conversation = count("conversationTokens"),
            limit = count("tokenLimit"),
            messages = count("messagesLength"),
            "Copilot context breakdown"
        ),
        SessionEventType::SessionMcpServersLoaded => {
            let servers = event
                .data
                .get("servers")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten();
            for server in servers {
                let field = |name: &str| server.get(name).and_then(serde_json::Value::as_str);
                tracing::debug!(
                    name = field("name"),
                    status = field("status"),
                    source = field("source"),
                    transport = field("transport"),
                    error = field("error"),
                    "Copilot MCP server loaded"
                );
            }
        }
        SessionEventType::SessionMcpServerStatusChanged => tracing::debug!(
            name = event
                .data
                .get("serverName")
                .and_then(serde_json::Value::as_str),
            status = event.data.get("status").and_then(serde_json::Value::as_str),
            error = event.data.get("error").and_then(serde_json::Value::as_str),
            "Copilot MCP server status changed"
        ),
        _ => {}
    }
}

enum TimelineEvent {
    Native(SessionEvent),
    Context(AttributedProviderEvent),
    /// Copilot's answer to the manual compaction `turn_id` was begun for.
    ManualCompactionAnswered {
        turn_id: TurnId,
        answer: ManualCompactionAnswer,
    },
    /// Copilot answered the manual compaction `turn_id` was begun for a bound ago.
    ManualCompactionOverdue {
        turn_id: TurnId,
    },
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
        TimelineEvent::ManualCompactionAnswered { turn_id, answer } => {
            let settled = events
                .correlation
                .lock()
                .expect("Copilot correlation lock is not poisoned")
                .project_manual_compaction_answer(turn_id, answer);
            events.pending.extend(settled.into_iter().map(Ok));
            return;
        }
        TimelineEvent::ManualCompactionOverdue { turn_id } => {
            let settled = events
                .correlation
                .lock()
                .expect("Copilot correlation lock is not poisoned")
                .project_manual_compaction_overdue(turn_id);
            events.pending.extend(settled.into_iter().map(Ok));
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
            event_type if is_compaction_report(&event_type) => {
                return Ok(
                    correlation.project_subagent_compaction(&subagent, compaction_event(&event))
                );
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
        return Ok(project_conversation_event(
            streams,
            &mut correlation.delegations,
            &correlation.execution_directory,
            &event,
        )?
        .into_iter()
        .map(|projected| attributed(Some(&subagent), projected))
        .collect());
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
            if correlation.owed_aborted_idle(aborted) {
                return Ok(Vec::new());
            }
            // No loop runs a manual compaction, and its answer settles its Turn.
            if correlation.is_compacting_on_request() {
                return Ok(Vec::new());
            }
            let mut projected = correlation.project_resumes_stopped(aborted);
            projected.extend(
                correlation
                    .project_session_idle(aborted)
                    .into_iter()
                    .map(|projected| attributed(None, projected)),
            );
            Ok(projected)
        }
        event_type if is_compaction_report(&event_type) => {
            Ok(correlation.project_main_compaction_report(&event))
        }
        // A manual compaction's summarising call is the Turn's Usage as its
        // `compactionTokensUsed` reports it, once.
        SessionEventType::AssistantUsage if correlation.is_compacting_on_request() => {
            Ok(Vec::new())
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
            Ok(project_conversation_event(
                &mut turn.streams,
                &mut correlation.delegations,
                &correlation.execution_directory,
                &event,
            )?
            .into_iter()
            .map(|projected| attributed(None, projected))
            .collect())
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
    Ok(metered(streams, usage, reported_cost))
}

/// Adds one model call's `usage`, costing `reported_cost`, to the reading the conversation's Turn
/// has accumulated, answering the Turn's Usage so far.
fn metered(
    streams: &mut ConversationStreams,
    usage: Usage,
    reported_cost: Option<Cost>,
) -> ProviderEvent {
    match streams.metering.as_mut() {
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
    }
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

/// Whether an event is one of Copilot's reports on compacting a conversation's context.
fn is_compaction_report(event_type: &SessionEventType) -> bool {
    matches!(
        event_type,
        SessionEventType::SessionCompactionStart | SessionEventType::SessionCompactionComplete
    )
}

/// The Provider event a compaction report is, for whichever conversation it compacted: the start,
/// or how it ended — completed with the context Copilot measured before and after it and the
/// summary it left, or failed with Copilot's account of why. Copilot's trigger is not read: whether a Compaction was
/// automatic is Suru's to say (ADR 0041). An end Suru cannot read still ends the compaction,
/// failed, since nothing else will.
fn compaction_event(event: &SessionEvent) -> ProviderEvent {
    if event.parsed_type() == SessionEventType::SessionCompactionStart {
        return ProviderEvent::CompactionStarted;
    }
    match reported::<SessionCompactionCompleteData>(event) {
        Some(completed) if completed.success => ProviderEvent::CompactionCompleted {
            before_tokens: reported_count(completed.pre_compaction_tokens),
            after_tokens: reported_count(completed.post_compaction_tokens),
            summary: completed.summary_content,
        },
        Some(failed) => ProviderEvent::CompactionFailed {
            error: failed.error,
        },
        None => ProviderEvent::CompactionFailed { error: None },
    }
}

/// Why a manual compaction's Turn fails when Copilot answered that it compacted nothing and said
/// nothing of why.
const NOT_COMPACTED: &str = "Copilot did not compact the Session's context";

/// The trigger Suru asks Copilot to compact on request under, which Copilot reports the
/// compaction's events under.
const MANUAL_TRIGGER: &str = "manual";

/// What a compaction's summarising call spent, from the `compactionTokensUsed` its
/// `session.compaction_complete` carries in the shape `assistant.usage` reports a model call's,
/// priced as one: the Usage of the manual compaction's Turn. Nothing where Copilot reported none.
fn compaction_usage(
    event: &SessionEvent,
    pricing: &CopilotPricing,
    streams: &mut ConversationStreams,
) -> Option<ProviderEvent> {
    let used = reported::<SessionCompactionCompleteData>(event)?.compaction_tokens_used?;
    let usage = Usage {
        fresh_input_tokens: exclusive_count(
            used.input_tokens,
            [used.cache_read_tokens, used.cache_write_tokens],
        ),
        cache_read_tokens: reported_count(used.cache_read_tokens),
        cache_write_tokens: reported_count(used.cache_write_tokens),
        output_tokens: reported_count(used.output_tokens),
        ..Usage::default()
    };
    let reported_cost = used
        .model
        .as_deref()
        .and_then(|model| pricing.cost(model, None, None, &usage));
    Some(metered(streams, usage, reported_cost))
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
/// withheld spawn's settle apart from a delegation that never happened; `execution_directory` is
/// where the Session works, which every conversation in it shares.
fn project_conversation_event(
    streams: &mut ConversationStreams,
    delegations: &mut HashSet<String>,
    execution_directory: &Path,
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
        SessionEventType::ToolExecutionStart => Ok(reported(event).map_or_else(
            Vec::new,
            |started: ToolExecutionStartData| {
                project_tool_started(streams, &started, execution_directory)
            },
        )),
        SessionEventType::ToolExecutionPartialResult => Ok(reported(event).map_or_else(
            Vec::new,
            |output: ToolExecutionPartialResultData| {
                project_tool_output(streams, &output.tool_call_id, output.partial_output)
            },
        )),
        SessionEventType::ToolExecutionComplete => Ok(reported(event).map_or_else(
            Vec::new,
            |completed: ToolExecutionCompleteData| {
                project_tool_completed(streams, delegations, &completed)
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
                compaction: Compacting::Idle,
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
        match streams.tools.get(tool_call_id)? {
            RunningTool::Spawn { prompt, .. } => prompt.clone(),
            _ => None,
        }
    }

    /// The Subagent whose conversation ran `tool_call_id`, or `None` for the main agent's own —
    /// which is also the answer for a spawn whose tool call never surfaced as an execution, the
    /// shape the runtime's own delegation takes.
    fn spawning_conversation(&self, tool_call_id: &str) -> Option<String> {
        self.subagents
            .iter()
            .find(|(_, working)| working.streams.tools.contains_key(tool_call_id))
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
    /// begins the next — or the Continuation a compaction of its context began, which that
    /// compaction outlasts. A message for an instance no Subagent holds stands nowhere.
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
            Some(Stretch::Resumed { .. } | Stretch::Compacting) => {
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
    /// split-held Reasoning title above all — before the settle itself drops the routes. A
    /// compaction still running settles with the stretch, and its end is owed nothing.
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
        if let Some(known) = self.agents.get_mut(subagent) {
            known.compaction.outlived();
        }
        let mut projected: Vec<AttributedProviderEvent> =
            settle_open_streams(&mut streams, &self.delegations, OpenReasoning::Complete)
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
        let Some(turn) = self.open_main_turn() else {
            return;
        };
        let kind = failure.error_type.replace(['_', '-'], " ");
        turn.failure.get_or_insert_with(|| {
            concise_remote_message(
                &format!("Copilot {kind} error: {}", failure.message),
                COPILOT_FAILURE_FALLBACK,
            )
        });
    }

    /// Projects one of Copilot's reports on compacting the main conversation's context: `event` is
    /// what [`compaction_event`] read it as.
    ///
    /// Copilot compacts in the background of the Session rather than in a stretch of its loop, so
    /// a report arriving while no Turn runs begins a Continuation of its own, whose loop is idle:
    /// the compaction settling settles it, since no idle will. Copilot owns that Continuation, and
    /// a Continuation late output began becomes Copilot's once Copilot compacts in it: either is
    /// reported with [`ProviderEvent::ContinuationStarted`], so an interrupt, or the next Prompt,
    /// cancels the compaction rather than leaving it to finish in a Turn Suru no longer holds it
    /// in. The end of a compaction Suru stopped following is owed nothing, and one an interrupt is
    /// cancelling leaves the stretch for the interrupt to settle.
    fn project_compaction(&mut self, event: ProviderEvent) -> Vec<AttributedProviderEvent> {
        let started = matches!(event, ProviderEvent::CompactionStarted);
        if !started {
            match &self.compaction {
                Compacting::Stopped => {
                    self.compaction = Compacting::Idle;
                    return Vec::new();
                }
                // The end raced the cancel. One that completed did compact; one that failed is
                // kept until Copilot answers the cancel, which says whose failure it was. Either
                // way a stretch the compaction held is left for the interrupt to settle.
                Compacting::Cancelling => {
                    return match event {
                        ProviderEvent::CompactionFailed { error } => {
                            self.compaction = Compacting::FailedWhileCancelling { error };
                            Vec::new()
                        }
                        event => {
                            self.compaction = Compacting::Idle;
                            if self.turn.is_some() {
                                vec![attributed(None, event)]
                            } else {
                                Vec::new()
                            }
                        }
                    };
                }
                // The end of the one already kept.
                Compacting::FailedWhileCancelling { .. } | Compacting::Failed { .. } => {
                    return Vec::new();
                }
                Compacting::Idle | Compacting::Running => {}
            }
        }
        self.compaction = if started {
            Compacting::Running
        } else {
            Compacting::Idle
        };
        let mut projected = Vec::with_capacity(3);
        if self.turn.is_none() {
            self.begin_continuation();
            if let Some(turn) = self.turn.as_mut() {
                turn.idled = Some(false);
            }
        }
        if let Some(turn) = self.turn.as_mut()
            && turn.continuation
            && !turn.owned
        {
            turn.owned = true;
            projected.push(attributed(
                None,
                ProviderEvent::ContinuationStarted {
                    selection: self.selection.clone(),
                },
            ));
        }
        projected.push(attributed(None, event));
        if !started && let Some(aborted) = self.turn.as_ref().and_then(|turn| turn.idled) {
            projected.extend(
                self.settle_turn(aborted)
                    .into_iter()
                    .map(|settled| attributed(None, settled)),
            );
        }
        projected
    }

    /// A compaction report for a Subagent's conversation, which stands in the stretch the Subagent
    /// is working. One Copilot begins while the Subagent works none is work of the Subagent's own,
    /// as its own Watch waking it would be: it begins a Continuation of the Subagent's Session,
    /// which the compaction ending settles, since no loop runs there to end it. The end of a
    /// compaction a settled stretch held records nothing, least of all in the stretch resuming
    /// the Subagent.
    fn project_subagent_compaction(
        &mut self,
        subagent: &str,
        event: ProviderEvent,
    ) -> Vec<AttributedProviderEvent> {
        let started = matches!(event, ProviderEvent::CompactionStarted);
        let Some(known) = self.agents.get_mut(subagent) else {
            return Vec::new();
        };
        if !known.compaction.follows(started) {
            return Vec::new();
        }
        let mut projected = Vec::with_capacity(3);
        if !self.subagents.contains_key(subagent) {
            self.subagents.insert(
                subagent.to_owned(),
                WorkingSubagent::new(Stretch::Compacting),
            );
            projected.push(attributed(
                None,
                ProviderEvent::SubagentWoken {
                    subagent_id: ProviderSubagentId::new(subagent),
                },
            ));
        }
        projected.push(attributed(Some(subagent), event));
        if !started
            && self
                .subagents
                .get(subagent)
                .is_some_and(|working| working.stretch == Stretch::Compacting)
        {
            projected
                .extend(self.project_stretch_settled(subagent, ProviderSubagentStatus::Completed));
        }
        projected
    }

    /// Begins an interrupt of whatever runs, returning what it is for. A compaction Copilot is
    /// running is being cancelled from here: until Copilot answers, it still holds its stretch
    /// open, and the interrupt settles that stretch once it has.
    pub(super) fn begin_interrupt(&mut self) -> InterruptScope {
        self.sending_abort();
        let compacting = self.compaction == Compacting::Running;
        if compacting {
            self.compaction = Compacting::Cancelling;
        }
        InterruptScope {
            stretch: self.turn.as_ref().map(|_| self.stretches),
            compacting,
        }
    }

    /// Copilot failed to cancel the compaction the interrupt began cancelling, which is still
    /// followed. A failure Copilot reported meanwhile was the compaction's own, since the cancel
    /// never took, and settles it with Copilot's error. If it ended and its stretch is held on it,
    /// nothing is left to settle that stretch but this: it settles as its loop's idle said.
    pub(super) fn cancel_failed(&mut self) -> Vec<AttributedProviderEvent> {
        let mut projected = Vec::new();
        match std::mem::take(&mut self.compaction) {
            Compacting::Cancelling => {
                self.compaction = Compacting::Running;
                return projected;
            }
            Compacting::FailedWhileCancelling { error } => {
                if self.turn.is_some() {
                    projected.push(attributed(None, ProviderEvent::CompactionFailed { error }));
                }
            }
            other => self.compaction = other,
        }
        if let Some(aborted) = self.turn.as_ref().and_then(|turn| turn.idled)
            && !self.compaction.holds_stretch()
        {
            projected.extend(
                self.settle_turn(aborted)
                    .into_iter()
                    .map(|settled| attributed(None, settled)),
            );
        }
        projected
    }

    /// What is left of the interrupt `scope` began once the compaction it cancelled, if any, has
    /// stopped, which Suru follows no further. Only the stretch it was for is its to stop: one that
    /// settled meanwhile left nothing running, and whatever began since is no business of this
    /// interrupt. A loop still running is aborted, and the idle the abort ends in settles its
    /// stretch. A stretch whose loop had already stopped settles here as interrupted, since nothing
    /// else will report its end — unless Subagents work on past it: the abort they need ends in an
    /// idle of the loop's own, which settles the stretch instead, so that idle cannot land in
    /// whatever Turn begins next. The interrupt waits for it ([`Self::expect_abort_idle`]).
    ///
    /// `cancelled` is Copilot's answer to the cancel, if one was sent: a failure Copilot reported
    /// while it was out is the cancel taking when it found something to cancel, and the
    /// compaction's own when it found nothing.
    pub(super) fn finish_interrupt(
        &mut self,
        scope: InterruptScope,
        cancelled: bool,
    ) -> InterruptRemainder {
        let current = scope.stretch == Some(self.stretches) && self.turn.is_some();
        self.compaction = match std::mem::take(&mut self.compaction) {
            Compacting::Cancelling => Compacting::Stopped,
            Compacting::FailedWhileCancelling { error } if !cancelled && current => {
                Compacting::Failed { error }
            }
            Compacting::FailedWhileCancelling { .. } => Compacting::Idle,
            other => other,
        };
        let running = InterruptRemainder {
            abort: true,
            settled: Vec::new(),
            awaits_idle: false,
        };
        let Some(stretch) = scope.stretch else {
            return running;
        };
        let Some(turn) = self.turn.as_ref().filter(|_| self.stretches == stretch) else {
            return InterruptRemainder {
                abort: false,
                ..running
            };
        };
        if turn.idled.is_none() {
            return running;
        }
        if self.compaction == Compacting::Running {
            self.compaction = Compacting::Stopped;
        }
        if self
            .subagents
            .values()
            .any(|working| working.stretch != Stretch::Compacting)
        {
            return InterruptRemainder {
                awaits_idle: true,
                ..running
            };
        }
        InterruptRemainder {
            abort: false,
            settled: self
                .settle_turn(true)
                .into_iter()
                .map(|settled| attributed(None, settled))
                .collect(),
            awaits_idle: false,
        }
    }

    /// Readies the signal that the abort an interrupt is about to send has ended in the idle that
    /// settles the stretch it was for ([`InterruptRemainder::awaits_idle`]): the timeline carrying
    /// Copilot's next idle raises it, ahead of anything the Turn after could produce.
    pub(super) fn expect_abort_idle(&mut self) -> Arc<Notify> {
        let carried = Arc::new(Notify::new());
        self.abort_idle = Some(carried.clone());
        carried
    }

    /// An abort is about to go out, so the next aborted idle is its own: none is owed any longer
    /// to one an interrupt gave up waiting on.
    pub(super) fn sending_abort(&mut self) {
        self.owes_aborted_idle = false;
    }

    /// Whether the aborted idle in hand is the one an earlier abort still owes, which settles
    /// nothing.
    fn owed_aborted_idle(&mut self, aborted: bool) -> bool {
        aborted && std::mem::take(&mut self.owes_aborted_idle)
    }

    /// The timeline has carried Copilot's main loop going idle — to the projection's queue, ahead
    /// of whatever follows it — which an interrupt may be waiting on.
    fn idle_carried(&mut self) {
        if let Some(carried) = self.abort_idle.take() {
            carried.notify_one();
        }
    }

    /// The abort an interrupt sent ended in no idle within the wait: settles, as interrupted, the
    /// stretch `scope` was for if it is still waiting on that idle, since nothing else will, and
    /// owes that idle should it come after all.
    pub(super) fn abort_idle_missing(
        &mut self,
        scope: InterruptScope,
    ) -> Vec<AttributedProviderEvent> {
        self.abort_idle = None;
        self.owes_aborted_idle = true;
        let waiting = scope.stretch == Some(self.stretches)
            && self.turn.as_ref().is_some_and(|turn| turn.idled.is_some());
        if !waiting {
            return Vec::new();
        }
        self.settle_turn(true)
            .into_iter()
            .map(|settled| attributed(None, settled))
            .collect()
    }

    /// A steer is about to reach Copilot. A loop already idle, with only a compaction holding the
    /// stretch open, runs again on it, so the stretch is the loop's until its next idle rather
    /// than the compaction's to settle — whichever of the steered loop's output and the
    /// compaction's end comes first. Returns the idle it held, for [`Self::steer_undelivered`].
    pub(super) fn steer_delivering(&mut self) -> Option<bool> {
        self.turn.as_mut().and_then(|turn| turn.idled.take())
    }

    /// The steer [`Self::steer_delivering`] prepared for never reached Copilot, whose loop is as
    /// idle as it was.
    pub(super) fn steer_undelivered(&mut self, idled: Option<bool>) {
        if let Some(turn) = self.turn.as_mut()
            && idled.is_some()
        {
            turn.idled = idled;
        }
    }

    /// Settles the Turn on the signal that Copilot's agentic loop has stopped: the stretch is the
    /// Turn, so its idle is the Turn's outcome — whatever the loop met on the way there, and an
    /// idle the abort produced is an interruption. The idle of a stale stretch — a Continuation
    /// the next Prompt already settled — is owed nothing and settles nothing.
    ///
    /// Copilot compacts in the background of the Session, so its loop can go idle while it still
    /// summarises. The stretch is not over until the compaction is — settling it now would fail a
    /// Compaction Copilot is about to complete — so the idle is held, and the compaction settling
    /// settles the stretch as the idle said (ADR 0042).
    fn project_session_idle(&mut self, aborted: bool) -> Vec<ProviderEvent> {
        // Every report Copilot wrote before answering an earlier manual compaction has reached
        // Suru ahead of an idle of its loop since, so any still owed will never come.
        self.manual_reports_owed = false;
        if self.stale_stretches > 0 {
            self.stale_stretches -= 1;
            return Vec::new();
        }
        if self.compaction.holds_stretch()
            && let Some(turn) = self.turn.as_mut()
        {
            turn.idled = Some(aborted);
            return Vec::new();
        }
        self.settle_turn(aborted)
    }

    /// Settles the main conversation's stretch on its loop having stopped — on an abort, if
    /// `aborted`. A compaction whose failure an interrupt found to be its own settles failed
    /// first.
    fn settle_turn(&mut self, aborted: bool) -> Vec<ProviderEvent> {
        let Some(mut turn) = self.turn.take() else {
            return Vec::new();
        };
        let mut projected = Vec::new();
        if let Compacting::Failed { error } = &mut self.compaction {
            projected.push(ProviderEvent::CompactionFailed {
                error: error.take(),
            });
            self.compaction = Compacting::Idle;
        }
        let outcome = match (turn.failure, aborted) {
            (Some(message), _) => ProviderEvent::TurnFailed { message },
            (None, true) => ProviderEvent::TurnInterrupted,
            (None, false) => ProviderEvent::TurnCompleted,
        };
        // A loop that stops mid-Message or mid-block leaves neither running forever. A block a
        // failure or an abort cut off is left to the Turn's settle, which closes it as the Turn
        // settled (ADR 0039).
        let reasoning = if matches!(outcome, ProviderEvent::TurnCompleted) {
            OpenReasoning::Complete
        } else {
            OpenReasoning::Release
        };
        projected.extend(settle_open_streams(
            &mut turn.streams,
            &self.delegations,
            reasoning,
        ));
        projected.push(outcome);
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

/// Opens the row a tool execution is recorded as: a Command for a shell execution, a File Change
/// for an edit, a Tool Call for one no more specific Activity records, and nothing for the rest —
/// a spawn's Tool Call withheld while the Subagent row represents its delegation. Copilot reports
/// no working directory of its own for a command, so only one that changes directory as it opens
/// names where it runs; any other leaves it to the Session's Workspace, which the Session already
/// carries.
fn project_tool_started(
    streams: &mut ConversationStreams,
    started: &ToolExecutionStartData,
    execution_directory: &Path,
) -> Vec<ProviderEvent> {
    if streams.tools.contains_key(&started.tool_call_id) {
        // A repeated start reports nothing new: the first record keeps its streamed output and
        // its withheld Tool Call.
        return Vec::new();
    }
    let activity_id = tool_activity_id(&started.tool_call_id);
    let (running, projected) = match ToolDisposition::of(started, execution_directory) {
        ToolDisposition::Command(PresentedCommand { command, cwd }) => (
            RunningTool::Command {
                streamed_output: String::new(),
            },
            vec![ProviderEvent::CommandStarted {
                activity_id,
                command,
                cwd,
            }],
        ),
        ToolDisposition::FileChange(changes) => (
            RunningTool::FileChange,
            vec![ProviderEvent::FileChangeStarted {
                activity_id,
                changes,
            }],
        ),
        ToolDisposition::ToolCall => {
            let PresentedToolCall {
                name,
                server,
                input,
            } = presented_tool_call(started);
            (
                RunningTool::ToolCall {
                    streamed_output: String::new(),
                },
                vec![ProviderEvent::ToolCallStarted {
                    activity_id,
                    name,
                    server,
                    input: Some(input),
                }],
            )
        }
        ToolDisposition::Spawn => {
            let prompt = started
                .arguments
                .as_ref()
                .and_then(|arguments| arguments.get("prompt"))
                .and_then(serde_json::Value::as_str)
                .filter(|prompt| !prompt.trim().is_empty())
                .map(str::to_owned);
            (
                RunningTool::Spawn {
                    withheld: presented_tool_call(started),
                    prompt,
                    streamed_output: String::new(),
                },
                Vec::new(),
            )
        }
        ToolDisposition::WriteAgent
        | ToolDisposition::Questionnaire
        | ToolDisposition::BrokeredDelegation
        | ToolDisposition::Plumbing => (RunningTool::Unrecorded, Vec::new()),
    };
    streams.tools.insert(started.tool_call_id.clone(), running);
    projected
}

/// Streams the next of a running Command's or Tool Call's output into the Transcript. A File
/// Change keeps none.
fn project_tool_output(
    streams: &mut ConversationStreams,
    tool_call_id: &str,
    output: String,
) -> Vec<ProviderEvent> {
    let activity_id = tool_activity_id(tool_call_id);
    match streams.tools.get_mut(tool_call_id) {
        Some(RunningTool::Command { streamed_output }) => {
            streamed_output.push_str(&output);
            vec![ProviderEvent::CommandOutputDelta {
                activity_id,
                content: output,
            }]
        }
        Some(RunningTool::ToolCall { streamed_output }) => {
            streamed_output.push_str(&output);
            vec![ProviderEvent::ToolCallOutputDelta {
                activity_id,
                content: output,
            }]
        }
        Some(RunningTool::Spawn {
            streamed_output, ..
        }) => {
            streamed_output.push_str(&output);
            Vec::new()
        }
        Some(RunningTool::FileChange | RunningTool::Unrecorded) | None => Vec::new(),
    }
}

/// Settles the row a tool execution is recorded as on what it came to, carrying whatever of its
/// output the stream had not already reached.
fn project_tool_completed(
    streams: &mut ConversationStreams,
    delegations: &mut HashSet<String>,
    completed: &ToolExecutionCompleteData,
) -> Vec<ProviderEvent> {
    match streams.tools.remove(&completed.tool_call_id) {
        Some(RunningTool::Command { streamed_output }) => {
            settle_command_events(completed, &streamed_output)
        }
        Some(RunningTool::FileChange) => vec![ProviderEvent::FileChangeCompleted {
            activity_id: tool_activity_id(&completed.tool_call_id),
            status: if completed.success {
                ProviderFileChangeStatus::Completed
            } else {
                ProviderFileChangeStatus::Failed
            },
        }],
        Some(RunningTool::ToolCall { streamed_output }) => {
            settle_tool_call_events(completed, &streamed_output)
        }
        Some(RunningTool::Spawn {
            withheld,
            streamed_output,
            ..
        }) => {
            if delegations.remove(&completed.tool_call_id) {
                return Vec::new();
            }
            // The spawn never opened its Subagent, so no row answers for the delegation: the
            // withheld Tool Call surfaces here, carrying what the execution reported went wrong.
            let mut projected =
                surface_withheld_spawn(&completed.tool_call_id, withheld, streamed_output.clone());
            projected.extend(settle_tool_call_events(completed, &streamed_output));
            projected
        }
        Some(RunningTool::Unrecorded) | None => Vec::new(),
    }
}

/// Opens the Tool Call a withheld spawn would have been, now that no Subagent row will answer for
/// the delegation, replaying the output the withholding kept back.
fn surface_withheld_spawn(
    tool_call_id: &str,
    withheld: PresentedToolCall,
    streamed_output: String,
) -> Vec<ProviderEvent> {
    let activity_id = tool_activity_id(tool_call_id);
    let PresentedToolCall {
        name,
        server,
        input,
    } = withheld;
    let mut projected = vec![ProviderEvent::ToolCallStarted {
        activity_id: activity_id.clone(),
        name,
        server,
        input: Some(input),
    }];
    if !streamed_output.is_empty() {
        projected.push(ProviderEvent::ToolCallOutputDelta {
            activity_id,
            content: streamed_output,
        });
    }
    projected
}

/// Settles a Command on what its tool execution came to, carrying whatever of its output the
/// stream had not already reached.
fn settle_command_events(
    completed: &ToolExecutionCompleteData,
    streamed_output: &str,
) -> Vec<ProviderEvent> {
    let activity_id = tool_activity_id(&completed.tool_call_id);
    let mut projected = Vec::with_capacity(2);
    if let Some(trailing) = trailing_output(completed, streamed_output) {
        projected.push(ProviderEvent::CommandOutputDelta {
            activity_id: activity_id.clone(),
            content: trailing,
        });
    }
    projected.push(ProviderEvent::CommandCompleted {
        activity_id,
        status: if completed.success {
            ProviderCommandStatus::Completed
        } else {
            ProviderCommandStatus::Failed
        },
        exit_status: command_exit_status(completed),
    });
    projected
}

/// The exit code a completed shell execution reports: the `shell_exit` part of its result, or
/// failing that the experimental `shellExecution` facts Copilot keeps beside the result. A code
/// outside what a process can exit with is no exit status at all.
fn command_exit_status(completed: &ToolExecutionCompleteData) -> Option<i32> {
    completed
        .result
        .as_ref()
        .and_then(|result| result.contents.as_deref())
        .unwrap_or_default()
        .iter()
        .find_map(|part| match part {
            ToolExecutionCompleteContent::ShellExit(shell_exit) => Some(shell_exit.exit_code),
            _ => None,
        })
        .or_else(|| {
            completed
                .shell_execution
                .as_ref()
                .map(|execution| execution.exit_code)
        })
        .and_then(|code| i32::try_from(code).ok())
}

/// Settles a Tool Call on what its tool execution came to: the rest of its result's text, or the
/// error a failed one reports instead, and a count of the result's parts that are not text.
fn settle_tool_call_events(
    completed: &ToolExecutionCompleteData,
    streamed_output: &str,
) -> Vec<ProviderEvent> {
    let activity_id = tool_activity_id(&completed.tool_call_id);
    let mut projected = Vec::with_capacity(2);
    if let Some(trailing) = trailing_output(completed, streamed_output) {
        projected.push(ProviderEvent::ToolCallOutputDelta {
            activity_id: activity_id.clone(),
            content: trailing,
        });
    }
    projected.push(ProviderEvent::ToolCallCompleted {
        activity_id,
        status: if completed.success {
            ProviderToolCallStatus::Completed
        } else {
            ProviderToolCallStatus::Failed
        },
        omitted_parts: omitted_result_parts(completed),
    });
    projected
}

/// How many parts of a completed execution's result are not text — images, audio, resources — and
/// so are left out of its output. Text, and the shell output a terminal part reports, is output.
fn omitted_result_parts(completed: &ToolExecutionCompleteData) -> u32 {
    let omitted = completed
        .result
        .as_ref()
        .and_then(|result| result.contents.as_deref())
        .unwrap_or_default()
        .iter()
        .filter(|part| {
            matches!(
                part,
                ToolExecutionCompleteContent::Image(_)
                    | ToolExecutionCompleteContent::Audio(_)
                    | ToolExecutionCompleteContent::ResourceLink(_)
                    | ToolExecutionCompleteContent::Resource(_)
            )
        })
        .count();
    u32::try_from(omitted).unwrap_or(u32::MAX)
}

/// What the completed execution adds to the output already streamed: the rest of a result the
/// stream had not reached, and for a failed execution what went wrong. The same rule settles a
/// Command and a Tool Call.
///
/// A result that is not what streamed extends adds nothing to a successful execution. Copilot cuts
/// the result it hands the Model down for token efficiency, so the stream is the fuller record, and
/// replacing it would show the reader the same output twice.
///
/// A failed execution may report a result, an error, or both, and nothing ranks one over the
/// other, so both reach the output: the rest of the result first, then the error's message. A
/// failure reports why rather than more output, so neither a result that does not continue the
/// stream nor the error continues it: each is added below what the execution had produced rather
/// than onto the end of its last line. An error the output already ends with — the result
/// restating it, or the stream having carried it — is not repeated.
fn trailing_output(completed: &ToolExecutionCompleteData, streamed: &str) -> Option<String> {
    let mut trailing = String::new();
    if let Some(result) = completed.result.as_ref() {
        let reported = result
            .detailed_content
            .as_deref()
            .unwrap_or(result.content.as_str());
        match reported.strip_prefix(streamed) {
            Some(rest) => trailing.push_str(rest),
            None if !completed.success && !reported.is_empty() => {
                push_below(streamed, &mut trailing, reported);
            }
            None => {}
        }
    }
    let error = completed
        .error
        .as_ref()
        .filter(|_| !completed.success)
        .map_or("", |error| error.message.as_str());
    let restated = format!("{streamed}{trailing}")
        .trim_end()
        .ends_with(error.trim_end());
    if !error.trim().is_empty() && !restated {
        push_below(streamed, &mut trailing, error);
    }
    (!trailing.is_empty()).then_some(trailing)
}

/// Adds `text` to `trailing` on a line of its own below everything the output holds so far — what
/// streamed, then what `trailing` already adds to it.
fn push_below(streamed: &str, trailing: &mut String, text: &str) {
    let so_far_ends_a_line = if trailing.is_empty() {
        streamed.is_empty() || streamed.ends_with('\n')
    } else {
        trailing.ends_with('\n')
    };
    if !so_far_ends_a_line {
        trailing.push('\n');
    }
    trailing.push_str(text);
}

/// Names the Activity one Reasoning block projects onto, in the identity space
/// [`tool_activity_id`] explains.
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

/// What settling a stopped conversation does with the Reasoning blocks it left open.
#[derive(Clone, Copy)]
enum OpenReasoning {
    /// The conversation finished its work, so each block completes.
    Complete,
    /// Something cut the conversation off: each block releases what its split withholds and is
    /// left running, for the Turn's settle to close as the Turn settled.
    Release,
}

/// Settles everything a stopped conversation left open: its Reasoning blocks, the Message it was
/// still streaming, and any withheld spawn no Subagent row ever answered for — surfaced here,
/// because a Tool Call the store never saw is one the store cannot settle, and the delegation
/// would otherwise vanish with the conversation. Open Commands, File Changes and Tool Calls are
/// otherwise left to the store — their outcome is Copilot's to report, not ours to invent — which
/// settles a Turn's open Activities from its own snapshot; only the split here knows the title it
/// is still withholding, which would otherwise go with the block.
fn settle_open_streams(
    streams: &mut ConversationStreams,
    delegations: &HashSet<String>,
    reasoning: OpenReasoning,
) -> Vec<ProviderEvent> {
    let mut projected = Vec::new();
    for mut block in std::mem::take(&mut streams.reasoning) {
        let reasoning_id = std::mem::take(&mut block.reasoning_id);
        projected.extend(match reasoning {
            OpenReasoning::Complete => settle_reasoning(&reasoning_id, &mut block),
            OpenReasoning::Release => {
                reasoning_segment_events(&reasoning_id, block.splitter.finish())
            }
        });
    }
    if streams.message.take().is_some() {
        projected.push(ProviderEvent::AgentMessageCompleted);
    }
    for (tool_call_id, running) in streams.tools.drain() {
        if let RunningTool::Spawn {
            withheld,
            streamed_output,
            ..
        } = running
            && !delegations.contains(&tool_call_id)
        {
            projected.extend(surface_withheld_spawn(
                &tool_call_id,
                withheld,
                streamed_output,
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
    fn an_aborted_idle_leaves_the_reasoning_it_cut_off_to_the_turns_settle() {
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
                ProviderEvent::TurnInterrupted,
            ],
            "a block cut short keeps the title its split was still withholding, and settles \
             with the Turn rather than completing"
        );
    }

    #[test]
    fn an_idle_completes_the_reasoning_the_turn_never_finished() {
        let mut correlation = in_turn();
        project(
            &mut correlation,
            "assistant.reasoning_delta",
            json!({ "reasoningId": "r1", "deltaContent": "**Reading the seam**" }),
        );

        assert_eq!(
            project(&mut correlation, "session.idle", json!({})),
            [
                ProviderEvent::ReasoningTitleChanged {
                    activity_id: reasoning_activity_id("r1"),
                    title: "Reading the seam".to_owned()
                },
                ProviderEvent::ReasoningCompleted {
                    activity_id: reasoning_activity_id("r1")
                },
                ProviderEvent::TurnCompleted,
            ]
        );
    }

    #[test]
    fn a_command_streams_its_output_into_the_transcript_and_settles_on_its_outcome() {
        let mut correlation = in_turn();
        let command = tool_activity_id("t1");
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
    fn a_tool_call_streams_its_output_and_settles_counting_what_it_left_out() {
        let mut correlation = in_turn();
        let tool_call = tool_activity_id("t1");
        assert_eq!(
            project(
                &mut correlation,
                "tool.execution_start",
                json!({
                    "toolCallId": "t1",
                    "toolName": "browser-screenshot",
                    "mcpServerName": "browser",
                    "mcpToolName": "screenshot",
                    "arguments": { "url": "https://example.com" },
                }),
            ),
            [ProviderEvent::ToolCallStarted {
                activity_id: tool_call.clone(),
                name: "screenshot".to_owned(),
                server: Some("browser".to_owned()),
                input: Some("url=https://example.com".to_owned()),
            }]
        );
        assert!(
            project(
                &mut correlation,
                "tool.execution_progress",
                json!({ "toolCallId": "t1", "progressMessage": "Loading the page" }),
            )
            .is_empty(),
            "a progress message is no output"
        );
        assert_eq!(
            project(
                &mut correlation,
                "tool.execution_partial_result",
                json!({ "toolCallId": "t1", "partialOutput": "Captured " }),
            ),
            [ProviderEvent::ToolCallOutputDelta {
                activity_id: tool_call.clone(),
                content: "Captured ".to_owned(),
            }]
        );
        assert_eq!(
            project(
                &mut correlation,
                "tool.execution_complete",
                json!({
                    "toolCallId": "t1",
                    "success": true,
                    "result": {
                        "content": "Captured the page",
                        "contents": [
                            { "type": "text", "text": "Captured the page" },
                            { "type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png" },
                            { "type": "audio", "data": "UklGRg==", "mimeType": "audio/wav" },
                        ],
                    },
                }),
            ),
            [
                ProviderEvent::ToolCallOutputDelta {
                    activity_id: tool_call.clone(),
                    content: "the page".to_owned(),
                },
                ProviderEvent::ToolCallCompleted {
                    activity_id: tool_call,
                    status: ProviderToolCallStatus::Completed,
                    omitted_parts: 2,
                },
            ]
        );
    }

    #[test]
    fn copilots_plumbing_projects_nothing_at_all() {
        let mut correlation = in_turn();
        for tool in ["report_intent", "task_complete"] {
            for (event_type, data) in [
                (
                    "tool.execution_start",
                    json!({ "toolCallId": tool, "toolName": tool, "arguments": {} }),
                ),
                (
                    "tool.execution_partial_result",
                    json!({ "toolCallId": tool, "partialOutput": "noted" }),
                ),
                (
                    "tool.execution_complete",
                    json!({ "toolCallId": tool, "success": true, "result": { "content": "ok" } }),
                ),
            ] {
                assert!(
                    project(&mut correlation, event_type, data).is_empty(),
                    "`{tool}` projects nothing at `{event_type}`"
                );
            }
        }
    }

    #[test]
    fn a_command_that_failed_settles_as_failed_with_what_copilot_said_went_wrong() {
        let mut correlation = in_turn();
        let command = tool_activity_id("t1");
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
    fn a_command_settles_with_the_exit_code_its_shell_exit_reports() {
        let mut correlation = in_turn();
        let command = tool_activity_id("t1");
        project(
            &mut correlation,
            "tool.execution_start",
            json!({
                "toolCallId": "t1",
                "toolName": "bash",
                "arguments": { "command": "cargo nextest run" },
            }),
        );

        let settled = project(
            &mut correlation,
            "tool.execution_complete",
            json!({
                "toolCallId": "t1",
                "success": false,
                "result": {
                    "content": "1 test failed",
                    "contents": [
                        { "type": "text", "text": "1 test failed" },
                        { "type": "shell_exit", "shellId": "s1", "exitCode": 101 },
                    ],
                },
                "shellExecution": { "exitCode": 7 },
                "error": { "message": "1 test failed" },
            }),
        );

        assert_eq!(
            settled.last(),
            Some(&ProviderEvent::CommandCompleted {
                activity_id: command,
                status: ProviderCommandStatus::Failed,
                exit_status: Some(101),
            })
        );
    }

    #[test]
    fn a_command_whose_result_reports_no_shell_exit_settles_with_its_shell_executions_exit_code() {
        let mut correlation = in_turn();
        let command = tool_activity_id("t1");
        project(
            &mut correlation,
            "tool.execution_start",
            json!({
                "toolCallId": "t1",
                "toolName": "bash",
                "arguments": { "command": "cargo nextest run" },
            }),
        );

        let settled = project(
            &mut correlation,
            "tool.execution_complete",
            json!({
                "toolCallId": "t1",
                "success": false,
                "shellExecution": { "exitCode": 2 },
                "error": { "message": "no such file" },
            }),
        );

        assert_eq!(
            settled.last(),
            Some(&ProviderEvent::CommandCompleted {
                activity_id: command,
                status: ProviderCommandStatus::Failed,
                exit_status: Some(2),
            })
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
    fn a_failed_commands_error_follows_a_result_reported_beside_it() {
        let mut correlation = in_turn();
        let complete = |tool_call_id: &str, result: &str, error: &str| {
            json!({
                "toolCallId": tool_call_id,
                "success": false,
                "result": { "content": result },
                "error": { "message": error },
            })
        };
        let trailing = |correlation: &mut CopilotCorrelation,
                        tool_call_id: &str,
                        streamed: &str,
                        completed: serde_json::Value| {
            project(
                correlation,
                "tool.execution_start",
                json!({
                    "toolCallId": tool_call_id,
                    "toolName": "bash",
                    "arguments": { "command": "cargo nextest run" },
                }),
            );
            if !streamed.is_empty() {
                project(
                    correlation,
                    "tool.execution_partial_result",
                    json!({ "toolCallId": tool_call_id, "partialOutput": streamed }),
                );
            }
            project(correlation, "tool.execution_complete", completed)
                .into_iter()
                .filter_map(|event| match event {
                    ProviderEvent::CommandOutputDelta { content, .. } => Some(content),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(
            trailing(
                &mut correlation,
                "t-empty",
                "",
                complete("t-empty", "", "command timed out")
            ),
            ["command timed out"],
            "an empty result leaves the error as the whole output"
        );
        assert_eq!(
            trailing(
                &mut correlation,
                "t-repeated",
                "running tests",
                complete("t-repeated", "running tests", "command timed out"),
            ),
            ["\ncommand timed out"],
            "a result repeating the stream leaves the error on a line of its own below it"
        );
        assert_eq!(
            trailing(
                &mut correlation,
                "t-extended",
                "running tests\n",
                complete(
                    "t-extended",
                    "running tests\n1 failed\n",
                    "command timed out"
                ),
            ),
            ["1 failed\ncommand timed out"],
            "the rest of the result comes first, and the error after it"
        );
        assert_eq!(
            trailing(
                &mut correlation,
                "t-restated",
                "",
                complete("t-restated", "command timed out", "command timed out"),
            ),
            ["command timed out"],
            "an error the result already reports is not repeated"
        );
        assert_eq!(
            trailing(
                &mut correlation,
                "t-finished",
                "command ti",
                complete("t-finished", "command timed out", "command timed out"),
            ),
            ["med out"],
            "nor is one the stream began and the result finished"
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
                    activity_id: tool_activity_id("t-sub"),
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
            "the spawning tool call opens no row of its own"
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
    fn a_spawn_that_never_opened_its_subagent_surfaces_as_the_tool_call_it_was() {
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
                ProviderEvent::ToolCallStarted {
                    activity_id: tool_activity_id("t-spawn"),
                    name: "task".to_owned(),
                    server: None,
                    input: Some("agent_type=no-such-agent".to_owned()),
                },
                ProviderEvent::ToolCallOutputDelta {
                    activity_id: tool_activity_id("t-spawn"),
                    content: "unknown agent type".to_owned(),
                },
                ProviderEvent::ToolCallCompleted {
                    activity_id: tool_activity_id("t-spawn"),
                    status: ProviderToolCallStatus::Failed,
                    omitted_parts: 0,
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
            "a repeated start neither reopens the row nor un-delegates the spawn"
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
                ProviderEvent::ToolCallStarted {
                    activity_id: tool_activity_id("t-spawn"),
                    name: "task".to_owned(),
                    server: None,
                    input: Some("agent_type=explore".to_owned()),
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

    /// The Broker's Tools reach Copilot as executions on the MCP server `suru`. A call that spawns,
    /// sends to, or stops a Subagent is no work a Transcript presents — the Broker adds the row
    /// that stands for what it did — so the execution projects nothing in the conversation that
    /// made it, the main agent's or a native Subagent's, while a call to any other MCP server is a
    /// Tool Call like any other Tool's.
    #[test]
    fn a_broker_call_affecting_a_subagent_adds_nothing_to_the_transcript_of_its_agent() {
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
            [ProviderEvent::ToolCallStarted {
                activity_id: tool_activity_id("t-linear"),
                name: "list_issues".to_owned(),
                server: Some("linear".to_owned()),
                input: Some(String::new()),
            }],
            "another MCP server's call is a Tool Call"
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

    fn compaction_started() -> serde_json::Value {
        json!({ "currentTokens": 182_000, "trigger": "threshold" })
    }

    fn compaction_completed() -> serde_json::Value {
        json!({ "success": true, "preCompactionTokens": 182_000, "postCompactionTokens": 31_000 })
    }

    fn compacted() -> ProviderEvent {
        ProviderEvent::CompactionCompleted {
            before_tokens: Some(182_000),
            after_tokens: Some(31_000),
            summary: None,
        }
    }

    pub(super) fn selection() -> AgentSelection {
        AgentSelection {
            provider: crate::protocol::ProviderId::new("copilot"),
            model: crate::protocol::ModelId::new("claude-fixture"),
            options: Vec::new(),
        }
    }

    /// A correlation whose first Turn settled under `selection()` with nothing left owing.
    fn after_a_turn() -> CopilotCorrelation {
        let mut correlation = in_turn();
        correlation.context_prompt_ready(TurnId::new(), selection());
        project(&mut correlation, "session.idle", json!({}));
        correlation
    }

    #[test]
    fn a_compaction_report_settles_as_copilot_says_whatever_it_was_triggered_by() {
        let mut correlation = in_turn();
        for trigger in [
            "threshold",
            "context_limit_retry",
            "memory_pressure",
            "model_switch",
        ] {
            assert_eq!(
                project(
                    &mut correlation,
                    "session.compaction_start",
                    json!({ "trigger": trigger }),
                ),
                [ProviderEvent::CompactionStarted]
            );
            assert_eq!(
                project(
                    &mut correlation,
                    "session.compaction_complete",
                    json!({ "success": false, "trigger": trigger, "error": "too long" }),
                ),
                [ProviderEvent::CompactionFailed {
                    error: Some("too long".to_owned())
                }]
            );
        }
        assert_eq!(
            project(
                &mut correlation,
                "session.compaction_complete",
                json!({ "preCompactionTokens": 1 }),
            ),
            [ProviderEvent::CompactionFailed { error: None }],
            "an end Suru cannot read still ends the compaction"
        );
    }

    #[test]
    fn copilots_truncation_and_summarising_model_calls_project_nothing() {
        let mut correlation = in_turn();
        project(
            &mut correlation,
            "session.compaction_start",
            compaction_started(),
        );
        for (event_type, data) in [
            (
                "session.truncation",
                json!({
                    "messagesRemovedDuringTruncation": 4,
                    "performedBy": "BasicTruncator",
                    "postTruncationMessagesLength": 10,
                    "postTruncationTokensInMessages": 9000,
                    "preTruncationMessagesLength": 14,
                    "preTruncationTokensInMessages": 12000,
                    "tokenLimit": 200_000,
                    "tokensRemovedDuringTruncation": 3000,
                }),
            ),
            (
                "model.call_start",
                json!({ "turnId": "compaction-1", "model": "claude-fixture" }),
            ),
            (
                "model.call_failure",
                json!({ "turnId": "compaction-1", "statusCode": 429 }),
            ),
            (
                "model.call_finished",
                json!({
                    "turnId": "compaction-1",
                    "dispatchDurationMs": 40_000.0,
                    "editClassifierVersion": 1,
                    "outcome": "success",
                }),
            ),
        ] {
            assert!(
                project(&mut correlation, event_type, data.clone()).is_empty(),
                "`{event_type}` records nothing"
            );
            assert!(
                project_attributed(
                    &mut with_subagent(),
                    agent_event("agent-1", event_type, data)
                )
                .is_empty(),
                "a sub-agent's `{event_type}` records nothing"
            );
        }
    }

    #[test]
    fn summarising_model_calls_while_the_loop_is_idle_neither_wake_it_nor_open_a_turn() {
        let mut correlation = in_turn();
        project(
            &mut correlation,
            "session.compaction_start",
            compaction_started(),
        );
        assert!(project(&mut correlation, "session.idle", json!({})).is_empty());
        assert!(
            project(
                &mut correlation,
                "model.call_start",
                json!({ "turnId": "compaction-1" }),
            )
            .is_empty()
        );
        assert_eq!(
            project(
                &mut correlation,
                "session.compaction_complete",
                compaction_completed(),
            ),
            [compacted(), ProviderEvent::TurnCompleted],
            "the compaction's own model call leaves the held idle standing"
        );
        assert!(
            project(
                &mut correlation,
                "model.call_finished",
                json!({ "turnId": "compaction-1" }),
            )
            .is_empty(),
            "a model call reported after the Turn settled opens no Continuation"
        );
    }

    #[test]
    fn an_idle_mid_compaction_settles_the_turn_as_the_idle_said_once_the_compaction_ends() {
        let mut correlation = in_turn();
        project(
            &mut correlation,
            "session.compaction_start",
            compaction_started(),
        );
        assert!(
            project(&mut correlation, "session.idle", json!({ "aborted": true })).is_empty(),
            "the Turn is held open while Copilot compacts"
        );
        assert!(correlation.is_turn_running());
        assert_eq!(
            project(
                &mut correlation,
                "session.compaction_complete",
                json!({ "success": false, "error": "too long" }),
            ),
            [
                ProviderEvent::CompactionFailed {
                    error: Some("too long".to_owned())
                },
                ProviderEvent::TurnInterrupted
            ]
        );
        assert!(!correlation.is_turn_running());
    }

    #[test]
    fn loop_output_after_a_held_idle_leaves_the_turn_to_the_loops_next_idle() {
        let mut correlation = in_turn();
        project(
            &mut correlation,
            "session.compaction_start",
            compaction_started(),
        );
        project(&mut correlation, "session.idle", json!({}));
        project(
            &mut correlation,
            "assistant.message",
            json!({ "messageId": "m-steered", "content": "Steered." }),
        );
        assert_eq!(
            project(
                &mut correlation,
                "session.compaction_complete",
                compaction_completed(),
            ),
            [compacted()],
            "the loop is running again, so the compaction ending settles nothing"
        );
        assert_eq!(
            project(&mut correlation, "session.idle", json!({})),
            [ProviderEvent::TurnCompleted]
        );
    }

    #[test]
    fn a_compaction_while_no_turn_runs_begins_a_continuation_copilot_owns_and_its_end_settles() {
        let mut correlation = after_a_turn();
        assert_eq!(
            project(
                &mut correlation,
                "session.compaction_start",
                compaction_started(),
            ),
            [
                ProviderEvent::ContinuationStarted {
                    selection: selection()
                },
                ProviderEvent::CompactionStarted
            ]
        );
        assert_eq!(
            project(
                &mut correlation,
                "session.compaction_complete",
                compaction_completed(),
            ),
            [compacted(), ProviderEvent::TurnCompleted]
        );
        assert_eq!(
            project(
                &mut correlation,
                "session.compaction_complete",
                compaction_completed(),
            ),
            [
                ProviderEvent::ContinuationStarted {
                    selection: selection()
                },
                compacted(),
                ProviderEvent::TurnCompleted
            ],
            "an end with no start still records its Compaction, in a Continuation it settles"
        );
    }

    #[test]
    fn a_compaction_continuation_woken_by_the_loop_settles_on_the_loops_idle() {
        let mut correlation = after_a_turn();
        project(
            &mut correlation,
            "session.compaction_start",
            compaction_started(),
        );
        project(
            &mut correlation,
            "assistant.message",
            json!({ "messageId": "m-woken", "content": "Woken." }),
        );
        assert_eq!(
            project(
                &mut correlation,
                "session.compaction_complete",
                compaction_completed(),
            ),
            [compacted()]
        );
        assert_eq!(
            project(&mut correlation, "session.idle", json!({})),
            [ProviderEvent::TurnCompleted]
        );
    }

    /// A correlation whose Turn's loop went idle while Copilot still compacts.
    fn held_by_a_compaction() -> CopilotCorrelation {
        let mut correlation = in_turn();
        project(
            &mut correlation,
            "session.compaction_start",
            compaction_started(),
        );
        assert!(project(&mut correlation, "session.idle", json!({})).is_empty());
        correlation
    }

    fn interrupted(settled: bool) -> Vec<AttributedProviderEvent> {
        if settled {
            vec![attributed(None, ProviderEvent::TurnInterrupted)]
        } else {
            Vec::new()
        }
    }

    #[test]
    fn an_interrupt_with_nothing_compacting_aborts_the_loop_alone() {
        let mut correlation = in_turn();
        let scope = correlation.begin_interrupt();
        assert!(!scope.compacting);
        assert_eq!(
            correlation.finish_interrupt(scope, scope.compacting),
            InterruptRemainder {
                abort: true,
                settled: Vec::new(),
                awaits_idle: false,
            }
        );
    }

    #[test]
    fn an_interrupt_with_no_turn_running_aborts_the_subagents_working_past_it() {
        let mut correlation = with_subagent();
        project(&mut correlation, "session.idle", json!({}));
        let scope = correlation.begin_interrupt();
        assert_eq!(
            correlation.finish_interrupt(scope, scope.compacting),
            InterruptRemainder {
                abort: true,
                settled: Vec::new(),
                awaits_idle: false,
            }
        );
    }

    #[test]
    fn an_interrupt_while_the_loop_works_and_copilot_compacts_stops_both_and_waits_on_neither() {
        let mut correlation = in_turn();
        project(
            &mut correlation,
            "session.compaction_start",
            compaction_started(),
        );
        let scope = correlation.begin_interrupt();
        assert!(scope.compacting);
        assert_eq!(
            correlation.finish_interrupt(scope, scope.compacting),
            InterruptRemainder {
                abort: true,
                settled: Vec::new(),
                awaits_idle: false,
            },
            "a working loop's own aborted idle settles its Turn"
        );
        assert!(
            project(
                &mut correlation,
                "session.compaction_complete",
                compaction_completed(),
            )
            .is_empty(),
            "the cancelled compaction's end is owed nothing, even one the cancel was too late for"
        );
        assert_eq!(
            project(&mut correlation, "session.idle", json!({ "aborted": true })),
            [ProviderEvent::TurnInterrupted],
            "the aborted idle waits on no compaction"
        );
    }

    #[test]
    fn an_interrupt_of_a_turn_only_a_compaction_holds_settles_it_with_nothing_to_abort() {
        let mut correlation = held_by_a_compaction();
        let scope = correlation.begin_interrupt();
        assert!(scope.compacting);
        assert_eq!(
            correlation.finish_interrupt(scope, scope.compacting),
            InterruptRemainder {
                abort: false,
                settled: interrupted(true),
                awaits_idle: false,
            }
        );
        assert!(
            project(
                &mut correlation,
                "session.compaction_complete",
                json!({ "success": false, "error": "Compaction cancelled" }),
            )
            .is_empty(),
            "the cancelled compaction's end is owed nothing: its Compaction settled with the Turn"
        );
    }

    #[test]
    fn an_interrupt_of_a_turn_a_compaction_holds_still_aborts_the_subagents_working_past_it() {
        let mut correlation = with_subagent();
        project(
            &mut correlation,
            "session.compaction_start",
            compaction_started(),
        );
        project(&mut correlation, "session.idle", json!({}));
        let scope = correlation.begin_interrupt();
        assert_eq!(
            correlation.finish_interrupt(scope, scope.compacting),
            InterruptRemainder {
                abort: true,
                settled: Vec::new(),
                awaits_idle: true,
            },
            "the abort the Subagent needs ends in an idle of the loop's own, which settles the Turn"
        );
        let carried = correlation.expect_abort_idle();
        assert!(
            project_attributed(
                &mut correlation,
                agent_event(
                    "agent-1",
                    "subagent.completed",
                    json!({ "toolCallId": "t-spawn", "agentName": "researcher", "agentDisplayName": "Researcher", "cancelled": true }),
                ),
            )
            .iter()
            .all(|event| event.event != ProviderEvent::TurnInterrupted)
        );
        correlation.idle_carried();
        assert!(
            futures_util::FutureExt::now_or_never(carried.notified()).is_some(),
            "the interrupt hears the timeline carried the idle it waits on"
        );
        assert_eq!(
            project(&mut correlation, "session.idle", json!({ "aborted": true })),
            [ProviderEvent::TurnInterrupted],
            "the abort's own idle settles the Turn it was for, with nothing compacting to hold it"
        );
        assert!(
            correlation.abort_idle_missing(scope).is_empty(),
            "a Turn its idle settled is not settled again"
        );
    }

    #[test]
    fn a_held_turn_whose_subagents_abort_ends_in_no_idle_settles_when_the_wait_runs_out() {
        let mut correlation = with_subagent();
        project(
            &mut correlation,
            "session.compaction_start",
            compaction_started(),
        );
        project(&mut correlation, "session.idle", json!({}));
        let scope = correlation.begin_interrupt();
        assert!(
            correlation
                .finish_interrupt(scope, scope.compacting)
                .awaits_idle
        );
        correlation.expect_abort_idle();
        assert_eq!(correlation.abort_idle_missing(scope), interrupted(true));
    }

    #[test]
    fn an_idle_while_an_interrupt_cancels_the_compaction_leaves_the_turn_to_the_interrupt() {
        let mut correlation = in_turn();
        project(
            &mut correlation,
            "session.compaction_start",
            compaction_started(),
        );
        let scope = correlation.begin_interrupt();
        assert!(
            project(&mut correlation, "session.idle", json!({})).is_empty(),
            "the compaction still holds the Turn until Copilot answers the cancel"
        );
        assert!(
            project(
                &mut correlation,
                "session.compaction_complete",
                json!({ "success": false, "error": "Compaction cancelled" }),
            )
            .is_empty(),
            "the cancel taking settles nothing on its own"
        );
        assert_eq!(
            correlation.finish_interrupt(scope, scope.compacting),
            InterruptRemainder {
                abort: false,
                settled: interrupted(true),
                awaits_idle: false,
            },
            "the loop stopped while the cancel was out, so nothing is left to abort"
        );
    }

    #[test]
    fn an_interrupt_stops_nothing_of_a_turn_after_the_one_it_was_for() {
        let mut correlation = in_turn();
        project(
            &mut correlation,
            "session.compaction_start",
            compaction_started(),
        );
        let scope = correlation.begin_interrupt();
        assert_eq!(
            project(
                &mut correlation,
                "session.compaction_complete",
                compaction_completed(),
            ),
            [compacted()],
            "a compaction that completed before the cancel took did compact"
        );
        assert_eq!(
            project(&mut correlation, "session.idle", json!({})),
            [ProviderEvent::TurnCompleted],
            "with nothing compacting, the loop's idle settles its Turn"
        );
        correlation
            .begin_turn()
            .expect("the next Prompt begins its Turn");

        assert_eq!(
            correlation.finish_interrupt(scope, scope.compacting),
            InterruptRemainder {
                abort: false,
                settled: Vec::new(),
                awaits_idle: false,
            },
            "the Turn the interrupt was for settled, and the one after is not its to stop"
        );
        assert!(correlation.is_turn_running());
    }

    #[test]
    fn a_failure_reported_while_the_cancel_is_out_is_the_cancel_if_it_took() {
        let mut correlation = in_turn();
        project(
            &mut correlation,
            "session.compaction_start",
            compaction_started(),
        );
        let scope = correlation.begin_interrupt();
        assert!(
            project(
                &mut correlation,
                "session.compaction_complete",
                json!({ "success": false, "error": "Compaction cancelled" }),
            )
            .is_empty(),
            "whose failure it was waits on Copilot's answer to the cancel"
        );
        assert!(
            project(&mut correlation, "session.idle", json!({})).is_empty(),
            "and until then it holds the Turn"
        );
        assert_eq!(
            correlation
                .finish_interrupt(scope, scope.compacting)
                .settled,
            interrupted(true),
            "the cancel took, so the failure was it, and the interrupt settles the Turn"
        );
    }

    #[test]
    fn a_failure_reported_while_the_cancel_is_out_is_the_compactions_own_if_it_failed() {
        let mut correlation = held_by_a_compaction();
        correlation.begin_interrupt();
        project(
            &mut correlation,
            "session.compaction_complete",
            json!({ "success": false, "error": "too long" }),
        );
        assert_eq!(
            correlation.cancel_failed(),
            [
                attributed(
                    None,
                    ProviderEvent::CompactionFailed {
                        error: Some("too long".to_owned())
                    }
                ),
                attributed(None, ProviderEvent::TurnCompleted)
            ],
            "the cancel never took, so Copilot's failure stands, and the Turn it held settles"
        );
    }

    #[test]
    fn a_failure_reported_while_the_cancel_is_out_is_the_compactions_own_if_nothing_was_cancelled()
    {
        let mut held = held_by_a_compaction();
        let scope = held.begin_interrupt();
        project(
            &mut held,
            "session.compaction_complete",
            json!({ "success": false, "error": "too long" }),
        );
        let failed = attributed(
            None,
            ProviderEvent::CompactionFailed {
                error: Some("too long".to_owned()),
            },
        );
        assert_eq!(
            held.finish_interrupt(scope, false).settled,
            [
                failed.clone(),
                attributed(None, ProviderEvent::TurnInterrupted)
            ],
            "the failure stands, ahead of the Turn the interrupt settles"
        );

        let mut working = in_turn();
        project(
            &mut working,
            "session.compaction_start",
            compaction_started(),
        );
        let scope = working.begin_interrupt();
        project(
            &mut working,
            "session.compaction_complete",
            json!({ "success": false, "error": "too long" }),
        );
        assert!(working.finish_interrupt(scope, false).abort);
        assert_eq!(
            project_attributed(
                &mut working,
                event("session.idle", json!({ "aborted": true }))
            ),
            [failed, attributed(None, ProviderEvent::TurnInterrupted)],
            "the failure stands ahead of the Turn the abort's idle settles"
        );
    }

    /// A correlation whose held Turn an interrupt settled itself once the abort its Subagent
    /// needed ended in no idle within the wait, and whose next Turn is running.
    fn owing_an_aborted_idle() -> CopilotCorrelation {
        let mut correlation = with_subagent();
        project(
            &mut correlation,
            "session.compaction_start",
            compaction_started(),
        );
        project(&mut correlation, "session.idle", json!({}));
        let scope = correlation.begin_interrupt();
        assert!(correlation.finish_interrupt(scope, true).awaits_idle);
        correlation.expect_abort_idle();
        assert_eq!(correlation.abort_idle_missing(scope), interrupted(true));
        correlation
            .begin_turn()
            .expect("the next Prompt begins its Turn");
        correlation
    }

    #[test]
    fn an_aborted_idle_an_earlier_abort_owes_settles_nothing_of_the_turn_after_it() {
        let mut correlation = owing_an_aborted_idle();
        assert!(
            project(&mut correlation, "session.idle", json!({ "aborted": true })).is_empty(),
            "the late idle belongs to the Turn the interrupt settled"
        );
        assert!(correlation.is_turn_running());
        assert_eq!(
            project(&mut correlation, "session.idle", json!({ "aborted": true })),
            [ProviderEvent::TurnInterrupted],
            "only one is owed"
        );
    }

    #[test]
    fn a_turns_own_idle_is_never_taken_for_the_one_an_earlier_abort_owes() {
        let mut correlation = owing_an_aborted_idle();
        assert_eq!(
            project(&mut correlation, "session.idle", json!({})),
            [ProviderEvent::TurnCompleted],
            "an idle no abort ended in is the Turn's own"
        );

        let mut correlation = owing_an_aborted_idle();
        correlation.begin_interrupt();
        assert_eq!(
            project(&mut correlation, "session.idle", json!({ "aborted": true })),
            [ProviderEvent::TurnInterrupted],
            "an abort sent since owes its own idle, which settles the Turn it stops"
        );
    }

    #[test]
    fn a_compaction_copilot_failed_to_cancel_is_followed_and_may_be_cancelled_again() {
        let mut correlation = held_by_a_compaction();
        correlation.begin_interrupt();
        assert!(correlation.cancel_failed().is_empty());
        let retried = correlation.begin_interrupt();
        assert!(
            retried.compacting,
            "the next interrupt cancels the compaction still running"
        );
        assert!(correlation.cancel_failed().is_empty());
        assert_eq!(
            project(
                &mut correlation,
                "session.compaction_complete",
                compaction_completed(),
            ),
            [compacted(), ProviderEvent::TurnCompleted],
            "its end settles the Turn it held, as its loop's idle said"
        );
    }

    #[test]
    fn a_compaction_that_ended_while_a_failed_cancel_was_out_releases_the_turn_it_held() {
        let mut correlation = held_by_a_compaction();
        correlation.begin_interrupt();
        assert_eq!(
            project(
                &mut correlation,
                "session.compaction_complete",
                compaction_completed(),
            ),
            [compacted()]
        );
        assert_eq!(
            correlation.cancel_failed(),
            [attributed(None, ProviderEvent::TurnCompleted)],
            "the interrupt that would have settled the Turn failed, so its idle does"
        );
    }

    #[test]
    fn a_compaction_starting_after_one_suru_stopped_following_is_recorded_again() {
        let mut correlation = in_turn();
        project(
            &mut correlation,
            "session.compaction_start",
            compaction_started(),
        );
        correlation.begin_interrupt();
        assert_eq!(
            project(
                &mut correlation,
                "session.compaction_start",
                compaction_started(),
            ),
            [ProviderEvent::CompactionStarted],
            "a new start is read as the stopped one's end never coming"
        );
        assert_eq!(
            project(
                &mut correlation,
                "session.compaction_complete",
                compaction_completed(),
            ),
            [compacted()]
        );
    }

    #[test]
    fn a_steer_into_a_turn_a_compaction_holds_hands_the_turn_back_to_the_loop() {
        let mut correlation = held_by_a_compaction();
        assert_eq!(correlation.steer_delivering(), Some(false));
        assert_eq!(
            project(
                &mut correlation,
                "session.compaction_complete",
                compaction_completed(),
            ),
            [compacted()],
            "the compaction ending before the steered loop speaks settles nothing"
        );
        assert!(
            !project(
                &mut correlation,
                "assistant.message",
                json!({ "messageId": "m-steered", "content": "Steered." }),
            )
            .is_empty(),
            "the steered loop answers in the Turn"
        );
        assert_eq!(
            project(&mut correlation, "session.idle", json!({})),
            [ProviderEvent::TurnCompleted]
        );
    }

    #[test]
    fn a_steer_that_never_reached_copilot_leaves_the_turn_to_its_compaction() {
        let mut correlation = held_by_a_compaction();
        let idled = correlation.steer_delivering();
        correlation.steer_undelivered(idled);
        assert_eq!(
            project(
                &mut correlation,
                "session.compaction_complete",
                compaction_completed(),
            ),
            [compacted(), ProviderEvent::TurnCompleted]
        );
    }

    #[test]
    fn a_compaction_in_a_continuation_late_output_began_makes_it_copilots() {
        let mut correlation = with_settled_subagent();
        project(&mut correlation, "session.idle", json!({}));
        project(
            &mut correlation,
            "assistant.message_start",
            json!({ "messageId": "m-late" }),
        );
        assert_eq!(
            project(
                &mut correlation,
                "session.compaction_start",
                compaction_started(),
            ),
            [
                ProviderEvent::ContinuationStarted {
                    selection: selection()
                },
                ProviderEvent::CompactionStarted
            ],
            "Copilot owns the Continuation it compacts in, so a Prompt stops the compaction"
        );
        assert_eq!(
            project(
                &mut correlation,
                "session.compaction_start",
                compaction_started(),
            ),
            [ProviderEvent::CompactionStarted],
            "and says so once"
        );
    }

    #[test]
    fn a_continuation_only_a_compaction_held_owes_the_turn_after_it_no_idle() {
        let mut correlation = after_a_turn();
        project(
            &mut correlation,
            "session.compaction_start",
            compaction_started(),
        );

        correlation.begin_turn().expect("a Prompt begins its Turn");
        assert!(
            project(
                &mut correlation,
                "session.compaction_complete",
                compaction_completed(),
            )
            .is_empty(),
            "the compaction settled with the Continuation the Prompt cut off"
        );
        assert_eq!(
            project(&mut correlation, "session.idle", json!({})),
            [ProviderEvent::TurnCompleted],
            "no loop ran in the Continuation, so the Prompt's own idle is the Prompt's"
        );
    }

    #[test]
    fn a_subagent_compaction_its_stretch_settled_without_ends_nowhere_once_it_resumes() {
        let mut correlation = with_subagent();
        project_attributed(
            &mut correlation,
            agent_event("agent-1", "session.compaction_start", compaction_started()),
        );
        project_attributed(
            &mut correlation,
            agent_event(
                "agent-1",
                "subagent.completed",
                json!({ "toolCallId": "t-spawn", "agentName": "researcher", "agentDisplayName": "Researcher" }),
            ),
        );
        project_attributed(
            &mut correlation,
            delivered_message("agent-1", "idle", None, "Run it again."),
        );
        assert!(
            project_attributed(
                &mut correlation,
                agent_event(
                    "agent-1",
                    "session.compaction_complete",
                    compaction_completed()
                ),
            )
            .is_empty(),
            "the end belongs to the stretch that settled, not the one resuming the Subagent"
        );
        assert_eq!(
            project_attributed(
                &mut correlation,
                agent_event("agent-1", "session.compaction_start", compaction_started()),
            ),
            [AttributedProviderEvent {
                attribution: subagent("agent-1"),
                event: ProviderEvent::CompactionStarted,
            }],
            "the resumed stretch's own compaction stands in it"
        );
    }

    #[test]
    fn a_subagent_compaction_starting_while_it_works_no_stretch_wakes_it_until_the_end() {
        let mut correlation = with_settled_subagent();
        assert_eq!(
            project_attributed(
                &mut correlation,
                agent_event("agent-1", "session.compaction_start", compaction_started()),
            ),
            [
                attributed(
                    None,
                    ProviderEvent::SubagentWoken {
                        subagent_id: ProviderSubagentId::new("agent-1")
                    }
                ),
                AttributedProviderEvent {
                    attribution: subagent("agent-1"),
                    event: ProviderEvent::CompactionStarted,
                }
            ],
            "the compaction is the Subagent's own work, in a Continuation of its Session"
        );
        assert_eq!(
            project_attributed(
                &mut correlation,
                agent_event(
                    "agent-1",
                    "session.compaction_complete",
                    compaction_completed()
                ),
            ),
            [
                AttributedProviderEvent {
                    attribution: subagent("agent-1"),
                    event: compacted(),
                },
                settled("agent-1", ProviderSubagentStatus::Completed)
            ],
            "no loop runs in it, so the compaction ending settles it"
        );
    }

    #[test]
    fn a_resume_while_a_compaction_holds_the_subagent_settles_that_continuation_first() {
        let mut correlation = with_settled_subagent();
        project_attributed(
            &mut correlation,
            agent_event("agent-1", "session.compaction_start", compaction_started()),
        );
        let resumed = project_attributed(
            &mut correlation,
            delivered_message("agent-1", "idle", None, "Run it again."),
        );
        assert_eq!(
            resumed[0],
            settled("agent-1", ProviderSubagentStatus::Completed),
            "the resume settles the Continuation the compaction began before it begins its own"
        );
        assert!(
            project_attributed(
                &mut correlation,
                agent_event(
                    "agent-1",
                    "session.compaction_complete",
                    compaction_completed()
                ),
            )
            .is_empty(),
            "the compaction outlasted its Continuation, so its end records nothing in the resume"
        );
    }

    #[test]
    fn a_subagents_compaction_lands_in_its_own_session_and_an_unknown_ones_nowhere() {
        let mut correlation = with_subagent();
        assert_eq!(
            project_attributed(
                &mut correlation,
                agent_event("agent-1", "session.compaction_start", compaction_started()),
            ),
            [AttributedProviderEvent {
                attribution: subagent("agent-1"),
                event: ProviderEvent::CompactionStarted,
            }]
        );
        assert_eq!(
            project_attributed(
                &mut correlation,
                agent_event(
                    "agent-1",
                    "session.compaction_complete",
                    compaction_completed()
                ),
            ),
            [AttributedProviderEvent {
                attribution: subagent("agent-1"),
                event: compacted(),
            }]
        );
        assert!(
            project_attributed(
                &mut correlation,
                agent_event("agent-9", "session.compaction_start", compaction_started()),
            )
            .is_empty()
        );
        assert_eq!(
            project(&mut correlation, "session.idle", json!({})),
            [ProviderEvent::TurnCompleted],
            "a Subagent's compaction holds nothing of the main conversation's open"
        );
    }

    /// A correlation whose Session sits idle after a Turn, then asked to compact on request in the
    /// Turn it answers.
    fn compacting_on_request() -> (CopilotCorrelation, TurnId) {
        compacting_on_request_after(after_a_turn())
    }

    fn compacting_on_request_after(
        mut correlation: CopilotCorrelation,
    ) -> (CopilotCorrelation, TurnId) {
        let turn = TurnId::new();
        correlation
            .begin_manual_compaction(turn)
            .expect("an idle Session takes a manual compaction");
        correlation.context_prompt_ready(turn, selection());
        (correlation, turn)
    }

    fn answered(
        correlation: &mut CopilotCorrelation,
        turn: TurnId,
        answer: ManualCompactionAnswer,
    ) -> Vec<ProviderEvent> {
        correlation
            .project_manual_compaction_answer(turn, answer)
            .into_iter()
            .map(|attributed| attributed.event)
            .collect()
    }

    fn overdue(correlation: &mut CopilotCorrelation, turn: TurnId) -> Vec<ProviderEvent> {
        correlation
            .project_manual_compaction_overdue(turn)
            .into_iter()
            .map(|attributed| attributed.event)
            .collect()
    }

    fn manual_started() -> serde_json::Value {
        json!({ "trigger": "manual", "currentTokens": 182_000 })
    }

    fn manual_completed() -> serde_json::Value {
        json!({
            "success": true,
            "trigger": "manual",
            "preCompactionTokens": 182_000,
            "postCompactionTokens": 31_000,
            "summaryContent": "<overview>Half done.</overview>",
            "compactionTokensUsed": {
                "inputTokens": 1_500,
                "cacheReadTokens": 500,
                "cacheWriteTokens": 100,
                "outputTokens": 40,
                "model": "claude-fixture",
            },
        })
    }

    /// The compaction `manual_completed` ends, with the counts and summary it reports.
    fn manual_compacted() -> ProviderEvent {
        ProviderEvent::CompactionCompleted {
            before_tokens: Some(182_000),
            after_tokens: Some(31_000),
            summary: Some("<overview>Half done.</overview>".to_owned()),
        }
    }

    /// The counts of the one model call `manual_completed` reports, as a Usage reports them.
    fn manual_usage(event: &ProviderEvent) -> (Option<u64>, Option<u64>, Option<u64>, Option<u64>) {
        let ProviderEvent::Usage { usage, .. } = event else {
            panic!("a Usage is reported: {event:?}");
        };
        (
            usage.fresh_input_tokens,
            usage.cache_read_tokens,
            usage.cache_write_tokens,
            usage.output_tokens,
        )
    }

    #[test]
    fn a_manual_compaction_is_reported_in_its_own_turn_and_settled_by_copilots_answer() {
        let (mut correlation, turn) = compacting_on_request();
        assert!(correlation.is_compacting_on_request());
        assert_eq!(
            project(
                &mut correlation,
                "session.compaction_start",
                manual_started()
            ),
            [ProviderEvent::CompactionStarted],
            "no Continuation begins: the Turn Suru opened holds it"
        );
        assert!(
            project(&mut correlation, "session.idle", json!({})).is_empty(),
            "no loop runs a manual compaction, so no idle settles its Turn"
        );
        assert!(
            project(
                &mut correlation,
                "assistant.usage",
                json!({ "model": "claude-fixture", "inputTokens": 9, "outputTokens": 9 })
            )
            .is_empty(),
            "what the summarising call spent is told once, by the compaction's end"
        );
        let ended = project(
            &mut correlation,
            "session.compaction_complete",
            manual_completed(),
        );
        assert_eq!(ended[0], manual_compacted());
        assert_eq!(
            manual_usage(&ended[1]),
            (Some(900), Some(500), Some(100), Some(40))
        );
        assert_eq!(
            answered(
                &mut correlation,
                turn,
                ManualCompactionAnswer::Compacted { summary: None }
            ),
            [ProviderEvent::TurnCompleted]
        );
        assert!(!correlation.is_turn_running());
    }

    #[test]
    fn an_answer_ahead_of_the_compactions_reports_waits_for_them() {
        let (mut correlation, turn) = compacting_on_request();
        assert!(
            answered(
                &mut correlation,
                turn,
                ManualCompactionAnswer::Compacted { summary: None }
            )
            .is_empty(),
            "a compaction Copilot made has reports still to come"
        );
        assert_eq!(
            project(
                &mut correlation,
                "session.compaction_start",
                manual_started()
            ),
            [ProviderEvent::CompactionStarted],
            "in the Turn it was asked in, not a Continuation"
        );
        let ended = project(
            &mut correlation,
            "session.compaction_complete",
            manual_completed(),
        );
        assert_eq!(ended.len(), 3, "{ended:?}");
        assert_eq!(ended[0], manual_compacted(), "its counts land on it");
        assert_eq!(
            manual_usage(&ended[1]),
            (Some(900), Some(500), Some(100), Some(40)),
            "its Usage lands on its Turn"
        );
        assert_eq!(ended[2], ProviderEvent::TurnCompleted);
        assert!(!correlation.is_turn_running());
        assert!(
            overdue(&mut correlation, turn).is_empty(),
            "a Turn that settled is owed no overdue settle"
        );
        assert!(
            correlation.begin_manual_compaction(TurnId::new()).is_ok(),
            "nothing is owed of a compaction whose reports are in"
        );
    }
    #[test]
    fn only_a_request_copilot_rejected_outright_settles_ahead_of_its_reports() {
        let failure = |rejected| ManualCompactionAnswer::NotCompacted {
            error: Some("No messages to compact".to_owned()),
            rejected,
        };
        let refused = ProviderEvent::TurnFailed {
            message: "No messages to compact".to_owned(),
        };
        let (mut correlation, turn) = compacting_on_request();
        assert_eq!(
            answered(&mut correlation, turn, failure(true)),
            std::slice::from_ref(&refused),
            "Copilot rejected the request before it could compact anything"
        );
        assert!(
            correlation.begin_manual_compaction(TurnId::new()).is_ok(),
            "and owes no report of it"
        );

        // Any other failure may have reports still on their way.
        let (mut correlation, turn) = compacting_on_request();
        assert!(answered(&mut correlation, turn, failure(false)).is_empty());
        assert_eq!(
            project(
                &mut correlation,
                "session.compaction_start",
                manual_started()
            ),
            [ProviderEvent::CompactionStarted]
        );
        assert_eq!(
            project(
                &mut correlation,
                "session.compaction_complete",
                json!({ "success": false, "trigger": "manual", "error": "Too long to summarise" }),
            ),
            [
                ProviderEvent::CompactionFailed {
                    error: Some("Too long to summarise".to_owned()),
                },
                ProviderEvent::TurnFailed {
                    message: "Too long to summarise".to_owned(),
                },
            ],
            "the reports record the Compaction, which says why the Turn failed"
        );

        // Reports that never come leave the Turn to settle on the answer once overdue, owing
        // nothing of a compaction Copilot never reported and says it did not make.
        let (mut correlation, turn) = compacting_on_request();
        assert!(answered(&mut correlation, turn, failure(false)).is_empty());
        assert_eq!(overdue(&mut correlation, turn), [refused]);
        assert!(correlation.begin_manual_compaction(TurnId::new()).is_ok());
    }
    #[test]
    fn an_answered_compaction_whose_end_never_comes_settles_on_the_answer_once_overdue() {
        let (mut correlation, turn) = compacting_on_request();
        project(
            &mut correlation,
            "session.compaction_start",
            manual_started(),
        );
        assert!(
            answered(
                &mut correlation,
                turn,
                ManualCompactionAnswer::Compacted {
                    summary: Some("<overview>Half done.</overview>".to_owned()),
                }
            )
            .is_empty(),
            "the compaction's end is still to come"
        );
        assert!(
            overdue(&mut correlation, TurnId::new()).is_empty(),
            "another compaction's wait settles nothing of this one"
        );
        assert_eq!(
            overdue(&mut correlation, turn),
            [
                ProviderEvent::CompactionCompleted {
                    before_tokens: None,
                    after_tokens: None,
                    summary: Some("<overview>Half done.</overview>".to_owned()),
                },
                ProviderEvent::TurnCompleted,
            ],
            "Copilot answered that it compacted"
        );
        assert!(
            project(
                &mut correlation,
                "session.compaction_complete",
                manual_completed()
            )
            .is_empty(),
            "the end of a manual compaction whose Turn settled records nothing"
        );
        assert!(!correlation.is_turn_running(), "and begins no Continuation");
        assert!(
            correlation.begin_manual_compaction(TurnId::new()).is_ok(),
            "its reports are in, so nothing is owed"
        );
        correlation.abandon_turn();
        assert_eq!(
            project(
                &mut correlation,
                "session.compaction_start",
                compaction_started()
            ),
            [
                ProviderEvent::ContinuationStarted {
                    selection: selection()
                },
                ProviderEvent::CompactionStarted,
            ],
            "a compaction Copilot begins in the background after is its own"
        );
    }

    /// A correlation whose Turn an interrupt settled while Copilot compacted in its background,
    /// cancelling a compaction whose end has yet to come.
    fn after_a_cancelled_compaction() -> CopilotCorrelation {
        let mut correlation = in_turn();
        correlation.context_prompt_ready(TurnId::new(), selection());
        project(
            &mut correlation,
            "session.compaction_start",
            compaction_started(),
        );
        project(&mut correlation, "session.idle", json!({}));
        let scope = correlation.begin_interrupt();
        assert!(scope.compacting);
        let remainder = correlation.finish_interrupt(scope, true);
        assert!(!remainder.abort, "{remainder:?}");
        assert!(!correlation.is_turn_running());
        correlation
    }

    #[test]
    fn a_cancelled_background_compactions_end_never_reaches_the_manual_compaction_after_it() {
        let stale = || {
            json!({
                "success": false,
                "trigger": "threshold",
                "error": "Compaction cancelled",
                "compactionTokensUsed": { "inputTokens": 9_000, "outputTokens": 10 },
            })
        };
        for stale_before_the_start in [true, false] {
            let (mut correlation, turn) =
                compacting_on_request_after(after_a_cancelled_compaction());
            if stale_before_the_start {
                assert!(
                    project(&mut correlation, "session.compaction_complete", stale()).is_empty(),
                    "the cancelled compaction's end is owed nothing"
                );
            }
            assert_eq!(
                project(
                    &mut correlation,
                    "session.compaction_start",
                    manual_started()
                ),
                [ProviderEvent::CompactionStarted]
            );
            if !stale_before_the_start {
                assert!(
                    project(&mut correlation, "session.compaction_complete", stale()).is_empty(),
                    "the cancelled compaction's end settles nothing of the manual one"
                );
            }
            let ended = project(
                &mut correlation,
                "session.compaction_complete",
                manual_completed(),
            );
            assert_eq!(
                ended[0],
                manual_compacted(),
                "the manual compaction ends as it did"
            );
            assert_eq!(
                manual_usage(&ended[1]),
                (Some(900), Some(500), Some(100), Some(40)),
                "with its own Usage alone"
            );
            assert_eq!(
                answered(
                    &mut correlation,
                    turn,
                    ManualCompactionAnswer::Compacted { summary: None }
                ),
                [ProviderEvent::TurnCompleted]
            );
        }
    }

    #[test]
    fn reports_naming_no_trigger_decide_nothing_of_a_manual_compaction() {
        // The cancelled compaction's end, naming no trigger, after the manual one began.
        let (mut correlation, turn) = compacting_on_request_after(after_a_cancelled_compaction());
        assert_eq!(
            project(
                &mut correlation,
                "session.compaction_start",
                manual_started()
            ),
            [ProviderEvent::CompactionStarted]
        );
        assert!(
            project(
                &mut correlation,
                "session.compaction_complete",
                json!({
                    "success": false,
                    "error": "Compaction cancelled",
                    "compactionTokensUsed": { "inputTokens": 9_000, "outputTokens": 10 },
                }),
            )
            .is_empty(),
            "it could be either compaction's end, so it ends neither"
        );
        let ended = project(
            &mut correlation,
            "session.compaction_complete",
            manual_completed(),
        );
        assert_eq!(ended[0], manual_compacted());
        assert_eq!(
            manual_usage(&ended[1]),
            (Some(900), Some(500), Some(100), Some(40))
        );
        assert_eq!(
            answered(
                &mut correlation,
                turn,
                ManualCompactionAnswer::Compacted { summary: None }
            ),
            [ProviderEvent::TurnCompleted]
        );

        // With nothing owed, a report naming no trigger still decides nothing.
        let (mut correlation, _) = compacting_on_request();
        for (report, data) in [
            ("session.compaction_start", json!({})),
            ("session.compaction_complete", compaction_completed()),
        ] {
            assert!(project(&mut correlation, report, data).is_empty());
        }
        assert!(correlation.is_compacting_on_request());
    }
    #[test]
    fn a_manual_compactions_turn_settles_as_copilot_answered_and_as_suru_aborted_it() {
        let failure = "Compaction failed: the model returned an empty summary";
        for (aborted, ending, answer, expected) in [
            // Copilot's own failure, in its words.
            (
                None,
                Some(json!({ "success": false, "trigger": "manual", "error": failure })),
                ManualCompactionAnswer::NotCompacted {
                    error: Some("RPC said otherwise".to_owned()),
                    rejected: false,
                },
                ProviderEvent::TurnFailed {
                    message: failure.to_owned(),
                },
            ),
            // Copilot rejecting the request before it compacted anything.
            (
                None,
                None,
                ManualCompactionAnswer::NotCompacted {
                    error: Some("No messages to compact".to_owned()),
                    rejected: true,
                },
                ProviderEvent::TurnFailed {
                    message: "No messages to compact".to_owned(),
                },
            ),
            (
                None,
                None,
                ManualCompactionAnswer::NotCompacted {
                    error: None,
                    rejected: true,
                },
                ProviderEvent::TurnFailed {
                    message: NOT_COMPACTED.to_owned(),
                },
            ),
            // The cancellation Suru's abort asked for.
            (
                Some(true),
                Some(
                    json!({ "success": false, "trigger": "manual", "error": "Compaction Cancelled" }),
                ),
                ManualCompactionAnswer::NotCompacted {
                    error: Some("Compaction Cancelled".to_owned()),
                    rejected: false,
                },
                ProviderEvent::TurnInterrupted,
            ),
            // An abort that found nothing to abort leaves the compaction's own end standing.
            (
                Some(false),
                Some(json!({ "success": false, "trigger": "manual", "error": failure })),
                ManualCompactionAnswer::NotCompacted {
                    error: None,
                    rejected: false,
                },
                ProviderEvent::TurnFailed {
                    message: failure.to_owned(),
                },
            ),
            (
                Some(false),
                Some(manual_completed()),
                ManualCompactionAnswer::Compacted { summary: None },
                ProviderEvent::TurnCompleted,
            ),
        ] {
            let (mut correlation, turn) = compacting_on_request();
            if let Some(ending) = ending {
                project(
                    &mut correlation,
                    "session.compaction_start",
                    manual_started(),
                );
                if let Some(aborted) = aborted {
                    correlation.manual_compaction_aborted(aborted);
                }
                project(&mut correlation, "session.compaction_complete", ending);
            } else if let Some(aborted) = aborted {
                correlation.manual_compaction_aborted(aborted);
            }
            assert_eq!(answered(&mut correlation, turn, answer), [expected]);
            assert!(!correlation.is_turn_running());
        }
    }

    #[test]
    fn no_manual_compaction_begins_while_copilot_owes_an_earlier_ones_reports() {
        let (mut correlation, turn) = compacting_on_request();
        assert!(
            answered(
                &mut correlation,
                turn,
                ManualCompactionAnswer::Compacted { summary: None }
            )
            .is_empty()
        );
        assert_eq!(
            overdue(&mut correlation, turn),
            [
                ProviderEvent::CompactionCompleted {
                    before_tokens: None,
                    after_tokens: None,
                    summary: None,
                },
                ProviderEvent::TurnCompleted,
            ]
        );
        for _ in 0..2 {
            assert!(
                correlation.begin_manual_compaction(TurnId::new()).is_err(),
                "its reports, under the manual trigger, would be taken for the next one's"
            );
        }
        assert!(!correlation.is_turn_running(), "nothing begins");

        // A stretch of the loop running to its idle has every report Copilot wrote before its
        // answer ahead of it, so none is owed any longer.
        correlation.begin_turn().expect("open the Turn");
        project(&mut correlation, "session.idle", json!({}));
        assert!(correlation.begin_manual_compaction(TurnId::new()).is_ok());
    }

    #[test]
    fn a_manual_compaction_is_refused_while_a_turn_runs() {
        let mut correlation = in_turn();
        assert!(correlation.begin_manual_compaction(TurnId::new()).is_err());
        assert!(
            !correlation.is_compacting_on_request(),
            "the Turn running is left as it was"
        );
    }
}
