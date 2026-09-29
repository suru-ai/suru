//! Projection of Codex's native notifications onto attributed Provider events.
//!
//! [`NativeCorrelation`] is the running state this projection needs: which native Turn is active
//! on the Session's own thread, the Messages, commands, file changes, Tool Calls, and native
//! Reasoning items each followed thread still has open, and the collab child threads the agent
//! has spawned. A spawn item on a followed thread opens its child as a Subagent — the event pump
//! then attaches the child thread so its own items stream over this connection too — and every
//! item projects under the attribution of the thread that produced it, which is how a child's work
//! lands in its Subagent's Session rather than the parent's Transcript, and how a child's own
//! spawns recurse.
//! Subagent lifecycle items are read regardless of the active Turn, because Codex documents a
//! child's completion arriving after the parent turn's own (ADR 0015). A child's stretch of work
//! settles at its own native turn's end, or wherever Codex's lifecycle reports say it did first;
//! a delegation that then starts another native turn on the same thread resumes it (ADR 0031).
//! A Delegation is classified by how Codex delivers it (ADR 0032): a `sendInput` into a child's
//! running turn begins nothing, and steers that turn once the child drains it, which the child's
//! thread reports as a `UserMessage` item. A child thread has no user, so each `UserMessage` on it
//! is a Delegation. The one its stretch began with already opens the stretch's Turn where the
//! collab call that spawned or resumed the child said what it handed over; a V2 agent's activity
//! never says, so there that first `UserMessage` is reported as the Delegation that began the
//! stretch, from the delegating Agent, which heads the stretch's Turn and gives its row the
//! description the activity lacked. Any later one is a steer: it stands in that Turn where it
//! arrived, credited to the Agent whose `sendInput` carried that text. A steer changes nothing in
//! the parent's Transcript, not even the Subagent row's description. Input Suru hands a child
//! itself — a Subagent Report to a native Subagent that delegated through the Broker (ADR 0035) —
//! steers a working child's turn, or begins a native turn on the child's thread that wakes it into
//! a stretch no Delegation began: a Continuation of its own Session, as a Watch's wake is, with no
//! resume and no row. The input stands nowhere either.
//! Notifications that belong to no thread Suru follows are dropped, notifications that contradict
//! the recorded state fail the Session, and everything else becomes the Provider events a Session
//! consumes.

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex},
};

use futures_util::stream;
use serde_json::Value;
use tokio::sync::mpsc;

use super::super::command_presentation::strip_launcher_wrapper;
use super::{
    DEFAULT_SERVICE_TIER_CHOICE_ID, REASONING_EFFORT_OPTION_ID, SERVICE_TIER_OPTION_ID,
    approval::{CodexApprovals, NativeApprovalIdentity, NativeApprovalKind},
    codex_error, codex_error_context,
    tools::{PresentedToolCall, ToolCallOutcome},
    transport::JsonRpcTransport,
    wire::{
        CodexPosture, NativeCollabAgentState, NativeCollabAgentStatus, NativeCollabCallStatus,
        NativeCollabTool, NativeCommandStatus, NativeCumulativeUsage, NativeField,
        NativeFileChange, NativeFileChangeStatus, NativeNotification, NativeSubagentActivityKind,
        NativeToolUse, NativeTurnFailureKind, NativeTurnOutcome, ThreadConnectionResult,
        ThreadResumeParams,
    },
};
use crate::{
    pricing::{ModelsDevModel, PricingSource},
    protocol::{
        AgentSelection, Approval, ApprovalSubject, CommandAction, FileChange, ModelId,
        ModelOptionChoiceId, ModelOptionId, ModelOptionSelection, ModelOptionValue, Usage,
    },
    provider::{
        AttributedProviderEvent, MeteredCost, ProviderActivityId, ProviderCommandStatus,
        ProviderError, ProviderEvent, ProviderEventAttribution, ProviderEventStream,
        ProviderFileChangeStatus, ProviderSubagentId, ProviderSubagentStatus, first_line,
        harness::ProcessGuard,
        reasoning::{ReasoningSegment, ReasoningSummarySplitter},
    },
};

/// What a Subagent's row calls the agent when Codex's wire names no kind for
/// it, as on the collab shape whose spawn calls carry only a prompt.
const GENERIC_SUBAGENT_NAME: &str = "Agent";

/// The one measurement a spawned child thread's first stretch is taken under.
/// Naming every reading of that stretch the same fixes the child's baseline at
/// nothing the first time and never rebases it, so the stretch counts whatever
/// the thread ran before Suru attached, however many native turns it read.
/// A resumed stretch is measured under the native turn that resumed it
/// instead, so it rebases onto wherever the thread's total stood.
const CHILD_THREAD_TURN: &str = "";

/// Everything the projection must remember between notifications for one Codex connection.
pub(super) struct NativeCorrelation {
    questionnaires: super::questionnaire::CodexQuestionnaires,
    approvals: CodexApprovals,
    thread_id: String,
    turn_starting: bool,
    active_turn_id: Option<String>,
    // Retained across settlement: Codex-initiated Turns inherit the last selection.
    effective_selection: Option<AgentSelection>,
    settled_turns: HashSet<String>,
    /// The Model this connection's Usage is priced at. Codex states no dollar
    /// figure of its own, so the Model is what the rate table is asked about;
    /// it outlives any one Turn because a spawned child thread meters under
    /// the Session's Model without a Turn of its own to read it from.
    metered_model: ModelId,
    /// Codex's running total for the Session's own thread.
    root_metering: ThreadMetering,
    context_fill_sequence: u64,
    pub(super) context_fill_turn: Option<crate::protocol::TurnId>,
    context_native_turn: Option<String>,
    /// The streaming items open on the Session's own thread.
    root: ThreadInFlight,
    /// The spawned child threads whose current stretch of work is open, by
    /// thread id — the identity each one's Subagent is known by.
    children: HashMap<String, AttachedChild>,
    /// Child threads with no stretch open: those whose Subagents settled, and
    /// those a delegation names that this connection never saw work — a
    /// child spawned before a restart. Each keeps what a resume measures and
    /// prices its stretch from. A spawn item repeating one of them re-opens
    /// nothing; only a resume begins another stretch.
    settled_children: HashMap<String, AttachedChild>,
    /// Context ordering survives child settlement, independently of output
    /// routing and the cumulative Usage baseline.
    child_context_turns: HashMap<String, ChildContextTurns>,
    /// Child threads spawned but not yet attached; the event pump drains this
    /// and requests each thread's stream.
    pending_attaches: Vec<String>,
}

/// Native Turn IDs have no sortable order. Once a newly observed Turn
/// supersedes one, later echoes of that old ID cannot refresh occupancy.
#[derive(Default)]
struct ChildContextTurns {
    current: Option<String>,
    superseded: HashSet<String>,
}

impl ChildContextTurns {
    fn observe(&mut self, turn_id: &str) -> bool {
        if turn_id.is_empty() || self.superseded.contains(turn_id) {
            return false;
        }
        if self.current.as_deref() != Some(turn_id)
            && let Some(previous) = self.current.replace(turn_id.to_owned())
        {
            self.superseded.insert(previous);
        }
        true
    }
}

/// The streaming items one followed thread has open.
#[derive(Default)]
struct ThreadInFlight {
    active_agent_message: Option<ActiveNativeAgentMessage>,
    active_commands: HashMap<String, ActiveNativeCommand>,
    active_file_changes: HashMap<String, ActiveNativeFileChange>,
    active_tool_calls: HashMap<String, ActiveNativeToolCall>,
    active_reasoning: HashMap<String, ActiveNativeReasoning>,
}

/// One child thread Suru knows: the items it has open, and the latest native
/// turn its items have ridden under, which is what a stop must name to
/// `turn/interrupt` the child.
///
/// A child works in stretches, each one a Turn of its Subagent's Session: the
/// spawn's, then one per resume. A stretch settles when the native turn it
/// runs in ends, or when Codex's lifecycle reports say so first; the child
/// then waits in [`NativeCorrelation::settled_children`] for a delegation to
/// start another native turn on its thread.
#[derive(Default)]
struct AttachedChild {
    in_flight: ThreadInFlight,
    latest_turn_id: Option<String>,
    /// The conversation that delegated the open stretch — the spawn's, or the
    /// resume's — which is who a steer no known send carried is credited to.
    delegator: Option<ProviderEventAttribution>,
    /// Whether the Delegation the open stretch began with may still arrive
    /// as the child's own item.
    opening: StretchOpening,
    /// The sends handing the open stretch more to do, until each one's steer
    /// arrives or the stretch settles.
    sends: Vec<SendInFlight>,
    /// Codex's running total for this child's own thread, kept across every
    /// stretch so a resume measures its own Turn from where the thread's total
    /// stood rather than from nothing.
    metering: ThreadMetering,
    model: Option<ModelId>,
    pricing_baseline: NativeCumulativeUsage,
    unpriced_prefix: bool,
    estimate_blocked: bool,
    /// The native turn a resume began the open stretch with, which is what
    /// the stretch is measured under; `None` for the spawn's stretch.
    resumed_turn: Option<String>,
    /// The native turns the open stretch has run in.
    stretch_turns: HashSet<String>,
    /// The native turns earlier stretches ran in. Anything naming one of them
    /// is the tail of work already settled, never the start of a resume.
    past_turns: HashSet<String>,
    /// Whether a stretch of this child's opened on this connection, which is
    /// what gives Model evidence for it a row to land on.
    worked_here: bool,
    /// A native turn this settled child began that no delegation has claimed.
    unclaimed_turn: Option<String>,
    /// The delegation waiting to resume this settled child once it starts a
    /// native turn.
    claim: Option<ResumeClaim>,
    /// Input Suru handed this child itself, with a `turn/start` of its own on
    /// the child's thread — Subagent Reports (ADR 0035) — as the `UserMessage`
    /// it arrives as will read, until it arrives. A Report stands in no
    /// Transcript, so that item is passed over rather than read as a
    /// Delegation.
    handed_input: Vec<String>,
    /// Where Suru's own `turn/start` on this settled child stands, while that
    /// input wakes it.
    handed_wake: Option<HandedWake>,
}

/// Suru's own `turn/start` waking a child with input it hands the child
/// itself (ADR 0035). The native turn that input runs in is a stretch of the
/// child's no Delegation began, so it opens with no resume: it is a
/// Continuation of the child's Session, as a Watch's wake is. A settled
/// child's is opened by Suru as Codex takes the input; one Codex begins with
/// the input after a working child's turn ended — Suru having found the child
/// working — waits for that stretch to settle, and opens with the wake.
#[derive(Clone, Debug, Eq, PartialEq)]
enum HandedWake {
    /// Sent to a settled child and not yet answered: the next native turn the
    /// child begins that no delegation claims is the one the input runs in.
    Requested,
    /// Answered, naming the native turn the input runs in — a new one, or one
    /// still running that Suru had already seen settle, which Codex steered
    /// the input into instead.
    Took(String),
}

impl AttachedChild {
    /// The measurement the open stretch's readings are recorded under.
    fn stretch_key(&self) -> String {
        self.resumed_turn
            .clone()
            .unwrap_or_else(|| CHILD_THREAD_TURN.to_owned())
    }

    /// What the open stretch has consumed so far, if it has been measured at
    /// all. A resumed stretch rebases only at its first reading, so until then
    /// the baseline still stands where an earlier stretch left it.
    fn stretch_usage(&self) -> Option<Usage> {
        (self.resumed_turn.is_none()
            || self.metering.baseline_turn.as_deref() == self.resumed_turn.as_deref())
        .then(|| self.metering.latest.since(self.metering.baseline))
    }

    /// Notes a `sendInput` handing this working child `text` from `sender`,
    /// once however many of the call's reports name it.
    fn expect_send(&mut self, call_id: &str, sender: &ProviderEventAttribution, text: &str) {
        if !self.sends.iter().any(|send| send.call_id == call_id) {
            self.sends.push(SendInFlight {
                call_id: call_id.to_owned(),
                sender: sender.clone(),
                text: text.to_owned(),
                delivered: false,
            });
        }
    }

    /// A `sendInput` to this working child completing. Codex has queued its
    /// input by then, but the running turn may not have drained it yet: one
    /// whose steer already arrived is done with, one still to arrive is kept
    /// for it, and one whose start went unseen is noted now.
    fn finish_send(&mut self, call_id: &str, sender: &ProviderEventAttribution, text: &str) {
        match self.sends.iter().position(|send| send.call_id == call_id) {
            Some(index) if self.sends[index].delivered => {
                self.sends.remove(index);
            }
            Some(_) => {}
            None => self.expect_send(call_id, sender, text),
        }
    }

    /// Forgets a `sendInput` that failed, whose input Codex never delivered.
    fn forget_send(&mut self, call_id: &str) {
        self.sends.retain(|send| send.call_id != call_id);
    }

    /// The Agent that sent the steer `text`: the earliest send carrying that
    /// text whose steer has not yet arrived. A steer no known send carried —
    /// one drained before either of its call's reports reached Suru — is
    /// credited to the conversation that delegated the stretch, which is the
    /// one that sends to it most, rather than dropped: the Subagent did
    /// receive it, and a Delegation it received stands.
    fn steer_sender(&mut self, text: &str) -> ProviderEventAttribution {
        match self
            .sends
            .iter_mut()
            .find(|send| !send.delivered && send.text.trim() == text.trim())
        {
            Some(send) => {
                send.delivered = true;
                send.sender.clone()
            }
            None => self.stretch_delegator(),
        }
    }

    /// The delegating Agent of the open stretch, which is who the Delegation
    /// it began with came from.
    fn stretch_delegator(&self) -> ProviderEventAttribution {
        self.delegator
            .clone()
            .unwrap_or(ProviderEventAttribution::OwningSession)
    }

    /// Admits `turn_id` to the open stretch, unless an earlier stretch ran in
    /// it. Returns whether the turn belongs to the open stretch.
    fn admit_turn(&mut self, turn_id: &str) -> bool {
        if self.past_turns.contains(turn_id) {
            return false;
        }
        if !turn_id.is_empty() {
            self.stretch_turns.insert(turn_id.to_owned());
        }
        true
    }
}

/// Where a child's open stretch stands with the Delegation that began it.
/// Codex puts the input a native turn begins with on the thread as a
/// `UserMessage` item, ahead of anything else the turn does. Where the collab
/// call that spawned or resumed the child said what it handed over, the
/// stretch's Turn already opens with that Delegation, so the item must not
/// stand again; where the item that began the stretch said nothing of it — a
/// V2 agent's activity — the item is the only word of it there is.
#[derive(Default)]
enum StretchOpening {
    /// The opening item may still arrive, because nothing else of the
    /// stretch has. Carries what the Delegation said, where the wire said.
    Awaited(Option<String>),
    /// The opening item has arrived, or can no longer: the stretch has done
    /// other work first, so the item was streamed before Suru followed the
    /// thread.
    #[default]
    Passed,
}

impl StretchOpening {
    /// Takes one `UserMessage` of the stretch, answering what it is: the
    /// opening one — the first to arrive before any other work, where it says
    /// what the opening Delegation said, if the wire said — or a steer.
    /// Whichever it is, every later one is a steer, since the opening item
    /// precedes everything else.
    fn receive(&mut self, text: &str) -> StretchInput {
        let input = match self {
            Self::Awaited(None) => StretchInput::UnrecordedOpening,
            Self::Awaited(Some(opening)) if opening.trim() == text.trim() => {
                StretchInput::RecordedOpening
            }
            Self::Awaited(Some(_)) | Self::Passed => StretchInput::Steer,
        };
        *self = Self::Passed;
        input
    }
}

/// What one `UserMessage` of a child's open stretch is to the Delegation that
/// began the stretch.
enum StretchInput {
    /// The opening item, whose Delegation the collab call that began the
    /// stretch already reported: it opens the stretch's Turn already.
    RecordedOpening,
    /// The opening item, carrying the Delegation the item that began the
    /// stretch said nothing of: it is reported now, to open the Turn.
    UnrecordedOpening,
    /// Anything after the opening item: a steer.
    Steer,
}

/// A `sendInput` handing a working child more to do, which the child's
/// running turn drains as a steer (ADR 0032). It is kept against its receiver
/// so the steer can name the Agent whose call carried it: the text a send
/// hands over is what the drained `UserMessage` reads, which is the only
/// thing the two share on the wire.
struct SendInFlight {
    call_id: String,
    sender: ProviderEventAttribution,
    text: String,
    /// Whether the receiver has drained it. The call's completion, which may
    /// trail the steer, then forgets it.
    delivered: bool,
}

/// A delegation to a settled child — Codex's `sendInput`, or the V2
/// `followupTask` it reports as an interaction — waiting for the native turn
/// it starts. Only that turn makes it a resume: a delegation that starts
/// none, such as V2's `sendMessage` to an idle agent, which reports the same
/// interaction, begins nothing. `delegator` is the conversation that ran the
/// call, which is where the resume's row stands.
struct ResumeClaim {
    delegator: ProviderEventAttribution,
    name: String,
    description: String,
    delegation: Option<String>,
}

/// Codex's latest running total for one followed thread, and the point the
/// Turn in progress measures itself from. Codex meters a thread rather than a
/// Turn and restates the whole total every time, so a Turn's Usage is the
/// distance travelled since it opened. Summing the per-call figures Codex
/// sends beside the total would double-count every call it re-announces
/// unchanged.
#[derive(Default)]
struct ThreadMetering {
    latest: NativeCumulativeUsage,
    baseline: NativeCumulativeUsage,
    /// The native Turn `baseline` was taken for, so the Turn is measured from
    /// one place however many readings it takes.
    baseline_turn: Option<String>,
}

impl ThreadMetering {
    /// Notes where the thread's total stands without attributing it to a Turn.
    /// This is what a reattach's replayed total does, and what a reading
    /// trailing its settled Turn does: neither is anyone's to claim, but both
    /// say where the next Turn starts measuring from. A reading that arrived
    /// late leaves the total it already reached standing.
    fn observe(&mut self, total: NativeCumulativeUsage) {
        self.latest = self.latest.furthest_of(total);
    }

    /// Admits a reading belonging to `turn_id` and answers with what that Turn
    /// has consumed. The first reading of a Turn fixes where it is measured
    /// from; the rest measure from that same place, so a total Codex restates
    /// unchanged restates the Turn's Usage rather than adding to it.
    fn record(&mut self, total: NativeCumulativeUsage, turn_id: &str) -> Usage {
        if self.baseline_turn.as_deref() != Some(turn_id) {
            self.baseline = self.latest;
            self.baseline_turn = Some(turn_id.to_owned());
        }
        self.observe(total);
        self.latest.since(self.baseline)
    }
}

struct ActiveNativeAgentMessage {
    item_id: String,
    streamed_text: String,
}

struct ActiveNativeCommand {
    streamed_output: String,
}

struct ActiveNativeFileChange {
    changes: Vec<FileChange>,
}

/// A Tool Call whose use Codex has yet to complete, with the input it opened with — none, where
/// the item did not yet say — which the completed item's input replaces only if it differs.
struct ActiveNativeToolCall {
    input: Option<String>,
}

/// The Reasoning item Codex is still streaming. Each of its summary sections
/// becomes a Reasoning Activity of its own, so the item is tracked one section
/// at a time: `open_section` is where the streaming section sits in the item's
/// summary — the index the wire names outright, rather than one counted from
/// the breaks — which is what names its Activity apart from its neighbours';
/// `streamed` is the raw text that section has carried, against which the
/// completed item's repeat of it is reconciled; and `splitter` holds that
/// section's title block back from its content until it resolves. A break
/// leaves the section behind and starts the next one on its own text and its
/// own splitter, so every section resolves a title and a body of its own.
struct ActiveNativeReasoning {
    open_section: usize,
    streamed: String,
    splitter: ReasoningSummarySplitter,
}

impl ActiveNativeReasoning {
    fn new() -> Self {
        Self {
            open_section: 0,
            streamed: String::new(),
            splitter: ReasoningSummarySplitter::default(),
        }
    }

    /// Admits the next chunk of the open section, yielding whatever of it the
    /// splitter resolved.
    fn push_delta(&mut self, delta: &str) -> ReasoningSegment {
        self.streamed.push_str(delta);
        self.splitter.push(delta)
    }

    /// Leaves the settled section behind for the one Codex has moved on to.
    fn begin_section(&mut self, section: usize) {
        self.open_section = section;
        self.streamed = String::new();
        self.splitter = ReasoningSummarySplitter::default();
    }
}

impl NativeCorrelation {
    pub(super) fn new(
        thread_id: String,
        metered_model: ModelId,
        questionnaires: super::questionnaire::CodexQuestionnaires,
        approvals: CodexApprovals,
    ) -> Self {
        Self {
            thread_id,
            questionnaires,
            approvals,
            turn_starting: false,
            active_turn_id: None,
            effective_selection: None,
            settled_turns: HashSet::new(),
            metered_model,
            root_metering: ThreadMetering::default(),
            context_fill_sequence: 0,
            context_fill_turn: None,
            context_native_turn: None,
            root: ThreadInFlight::default(),
            children: HashMap::new(),
            settled_children: HashMap::new(),
            child_context_turns: HashMap::new(),
            pending_attaches: Vec::new(),
        }
    }

    /// Takes Model evidence for one child thread. A working child's Subagent
    /// takes it at once, restating the stretch's Usage where a change of Model
    /// leaves it unpriceable. A settled child keeps it for the Turn its next
    /// resume begins; only an attach reply, which trails the stretch it was
    /// requested for, still reaches the row that stretch left behind — and
    /// only where that stretch ran on this connection.
    fn observe_child_model(
        &mut self,
        thread_id: &str,
        model: ModelId,
        from_attach: bool,
    ) -> Vec<AttributedProviderEvent> {
        let model_changed = |model: ModelId| AttributedProviderEvent {
            attribution: ProviderEventAttribution::OwningSession,
            event: ProviderEvent::SubagentModelChanged {
                subagent_id: ProviderSubagentId::new(thread_id),
                model,
            },
        };
        if let Some(child) = self.settled_children.get_mut(thread_id) {
            child.model = Some(model.clone());
            return if from_attach && child.worked_here {
                vec![model_changed(model)]
            } else {
                Vec::new()
            };
        }
        let Some(child) = self.children.get_mut(thread_id) else {
            return Vec::new();
        };
        let restated = match child.model.as_ref() {
            None => {
                child.pricing_baseline = child.metering.latest;
                child.unpriced_prefix = child
                    .stretch_usage()
                    .is_some_and(|usage| has_billable_usage(&usage));
                child.model = Some(model.clone());
                None
            }
            Some(current) if current == &model => None,
            Some(_) => {
                child.model = Some(model.clone());
                child.estimate_blocked = true;
                child.stretch_usage()
            }
        };
        let mut observed = vec![model_changed(model)];
        observed.extend(restated.map(|usage| AttributedProviderEvent {
            attribution: ProviderEventAttribution::Subagent(ProviderSubagentId::new(thread_id)),
            event: ProviderEvent::Usage { usage, cost: None },
        }));
        observed
    }

    /// The native Turn whose notifications are currently being projected.
    pub(super) fn active_turn_id(&self) -> Option<String> {
        self.active_turn_id.clone()
    }

    /// Whether a `turn/start` request has been issued but not yet answered.
    pub(super) fn is_turn_pending(&self) -> bool {
        self.turn_starting
    }

    /// Claims the single native Turn slot ahead of issuing a `turn/start` request.
    pub(super) fn begin_turn_start(&mut self) -> Result<(), ProviderError> {
        if self.turn_starting || self.active_turn_id.is_some() {
            return Err(codex_error(
                "Codex started a Turn while another native Turn was active",
            ));
        }
        self.context_native_turn = None;
        self.turn_starting = true;
        Ok(())
    }

    /// Releases the claim taken by [`Self::begin_turn_start`], installing the Turn if it started.
    pub(super) fn finish_turn_start(
        &mut self,
        started: Result<String, ProviderError>,
        selection: AgentSelection,
    ) -> Result<(), ProviderError> {
        self.turn_starting = false;
        match started {
            Ok(turn_id) if self.active_turn_id.is_none() => {
                self.context_native_turn = Some(turn_id.clone());
                self.active_turn_id = Some(turn_id);
                self.metered_model = selection.model.clone();
                self.effective_selection = Some(selection);
                self.root = ThreadInFlight::default();
                Ok(())
            }
            Ok(_) => Err(codex_error(
                "Codex started a Turn while another native Turn was active",
            )),
            Err(error) => Err(error),
        }
    }

    fn is_active_turn(&self, thread_id: &str, turn_id: &str) -> bool {
        self.thread_id == thread_id && self.active_turn_id.as_deref() == Some(turn_id)
    }

    /// Settles the native Turn on the Session's own thread. The children
    /// deliberately survive: a Subagent may outlive the Turn that spawned it
    /// (ADR 0015), and its thread keeps streaming until its own settle.
    fn settle_turn(&mut self) {
        if let Some(turn_id) = self.active_turn_id.take() {
            self.settled_turns.insert(turn_id);
        }
        self.root = ThreadInFlight::default();
    }

    /// The child threads spawned since last drained, for the pump to attach.
    pub(super) fn take_pending_attaches(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_attaches)
    }

    /// The thread one streamed item's step lands in, with the attribution its
    /// events carry: the Session's own thread while the turn the step names is
    /// the active one, or a child thread with a stretch open under its
    /// Subagent's identity. A child's steps are not gated on the turn its
    /// stretch began in, because a stretch is one Subagent Turn however many
    /// native turns Codex runs it as — but a step naming a turn an earlier
    /// stretch ran in is that stretch's tail, and lands nowhere. Anything else
    /// has nowhere to land and is dropped. A child's step is work its stretch
    /// did, so the item opening the stretch can no longer follow it.
    fn item_thread(
        &mut self,
        thread_id: &str,
        turn_id: &str,
    ) -> Option<(&mut ThreadInFlight, ProviderEventAttribution)> {
        if self.thread_id == thread_id {
            if self.active_turn_id.as_deref() == Some(turn_id) {
                return Some((&mut self.root, ProviderEventAttribution::OwningSession));
            }
            return None;
        }
        let child = self.child_step(thread_id, turn_id)?;
        child.opening = StretchOpening::Passed;
        Some((
            &mut child.in_flight,
            ProviderEventAttribution::Subagent(ProviderSubagentId::new(thread_id)),
        ))
    }

    /// The working child one streamed step of `thread_id` lands in, on the
    /// terms [`Self::item_thread`] states, noting the turn it names as the
    /// latest the child's work has ridden under.
    fn child_step(&mut self, thread_id: &str, turn_id: &str) -> Option<&mut AttachedChild> {
        if let Some(turns) = self.child_context_turns.get_mut(thread_id) {
            turns.observe(turn_id);
        }
        let child = self.children.get_mut(thread_id)?;
        if !child.admit_turn(turn_id) {
            return None;
        }
        child.latest_turn_id = Some(turn_id.to_owned());
        Some(child)
    }

    /// The `turn/interrupt` targets stopping every followed child takes: each
    /// child thread beside the latest turn its items have named. A child none
    /// have yet leaves `None` — there is no turn a stop could address, and
    /// skipping it is the caller's call to make.
    pub(super) fn child_interrupt_targets(&self) -> Vec<(String, Option<String>)> {
        self.children
            .iter()
            .map(|(thread_id, child)| (thread_id.clone(), child.latest_turn_id.clone()))
            .collect()
    }

    /// The one child's `turn/interrupt` target, on the same terms as
    /// [`Self::child_interrupt_targets`]; `None` for a thread not followed —
    /// already settled, or never spawned.
    pub(super) fn child_interrupt_target(
        &self,
        thread_id: &str,
    ) -> Option<(String, Option<String>)> {
        self.children
            .get(thread_id)
            .map(|child| (thread_id.to_owned(), child.latest_turn_id.clone()))
    }

    /// The attribution a Subagent lifecycle item on `thread_id` rides under,
    /// if that is a thread Suru follows and it is working — which is also the
    /// conversation a delegation it sends is attributed to. Deliberately not
    /// gated on the active Turn: the lifecycle speaks for threads rather than
    /// turns, and a child's completion may arrive after the turn that spawned
    /// it completed.
    fn spawner_attribution(&self, thread_id: &str) -> Option<ProviderEventAttribution> {
        if self.thread_id == thread_id {
            Some(ProviderEventAttribution::OwningSession)
        } else if self.children.contains_key(thread_id) {
            Some(ProviderEventAttribution::Subagent(ProviderSubagentId::new(
                thread_id,
            )))
        } else {
            None
        }
    }

    /// Opens one spawned child thread as a Subagent: follows the thread,
    /// queues its attach, and announces the spawn in the conversation that ran
    /// it — which is what decides the Session its child hangs under, and how a
    /// child's own spawns recurse one level down — carrying the spawn's
    /// Delegation where the wire reported what the child was handed. A spawn
    /// repeating a thread already followed or already settled opens nothing.
    fn spawn_child(
        &mut self,
        spawner: ProviderEventAttribution,
        child_thread_id: String,
        name: String,
        description: String,
        delegation: Option<String>,
    ) -> Vec<AttributedProviderEvent> {
        if child_thread_id == self.thread_id
            || self.children.contains_key(&child_thread_id)
            || self.settled_children.contains_key(&child_thread_id)
        {
            return Vec::new();
        }
        self.children.insert(
            child_thread_id.clone(),
            AttachedChild {
                delegator: Some(spawner.clone()),
                opening: StretchOpening::Awaited(delegation.clone()),
                worked_here: true,
                ..AttachedChild::default()
            },
        );
        self.child_context_turns
            .insert(child_thread_id.clone(), ChildContextTurns::default());
        self.pending_attaches.push(child_thread_id.clone());
        vec![AttributedProviderEvent {
            attribution: spawner,
            event: ProviderEvent::SubagentStarted {
                subagent_id: ProviderSubagentId::new(child_thread_id),
                name,
                description,
                delegation,
            },
        }]
    }

    /// Settles one Subagent's open stretch, moving its thread to the settled
    /// set: the settle closes the row and the child Session's Turn together.
    /// A send the stretch never drained was never delivered into it — Codex
    /// runs a turn on while input waits for it — so it is forgotten, and
    /// later output is discarded until a resume opens another stretch, while
    /// Context Fill retains its attribution and the thread's running total is
    /// still followed, for that resume to measure from. Like the spawn's
    /// counterparts on the other Providers, the settle addresses the row by
    /// the Subagent's own identity and rides the owning conversation, so a
    /// nested Subagent's settle still lands after its spawner's own — order
    /// the wire does not promise.
    fn settle_child(
        &mut self,
        child_thread_id: &str,
        status: ProviderSubagentStatus,
    ) -> Vec<AttributedProviderEvent> {
        let Some(mut child) = self.children.remove(child_thread_id) else {
            return Vec::new();
        };
        self.questionnaires.end_thread(child_thread_id);
        self.approvals.end_thread(child_thread_id);
        child.in_flight = ThreadInFlight::default();
        child.opening = StretchOpening::Passed;
        child.sends.clear();
        let stretch_turns = std::mem::take(&mut child.stretch_turns);
        // Input Suru handed the child that Codex took into a turn of its own,
        // begun after the one the stretch worked in ended, wakes the child
        // there once the stretch has settled (see [`HandedWake`]). Input one
        // of the stretch's own turns took is done with.
        let taken_later = matches!(
            &child.handed_wake,
            Some(HandedWake::Took(turn_id)) if !stretch_turns.contains(turn_id)
        );
        if !taken_later {
            child.handed_input.clear();
            child.handed_wake = None;
        }
        child.past_turns.extend(stretch_turns);
        self.settled_children
            .insert(child_thread_id.to_owned(), child);
        vec![AttributedProviderEvent {
            attribution: ProviderEventAttribution::OwningSession,
            event: ProviderEvent::SubagentCompleted {
                subagent_id: ProviderSubagentId::new(child_thread_id),
                status,
            },
        }]
    }

    /// A child thread's native turn ending. For a child with a stretch open,
    /// that is the stretch's own boundary — the one signal no stale lifecycle
    /// report can pre-empt — so it settles the stretch on the turn's outcome.
    /// A turn a settled child began unclaimed ends as nothing but the tail of
    /// work no resume took.
    fn finish_child_turn(
        &mut self,
        child_thread_id: &str,
        turn_id: &str,
        outcome: &NativeTurnOutcome,
    ) -> Vec<AttributedProviderEvent> {
        // A turn Suru's own input was taken into, ending before anything of
        // it streamed here, still ends the stretch that input woke the child
        // into: Suru opened it, and nothing else would settle it.
        if self
            .settled_children
            .get(child_thread_id)
            .is_some_and(|child| child.handed_wake == Some(HandedWake::Took(turn_id.to_owned())))
        {
            let mut finished = self.wake_for_handed_input(child_thread_id, turn_id.to_owned());
            finished.extend(self.finish_child_turn(child_thread_id, turn_id, outcome));
            return finished;
        }
        if let Some(child) = self.children.get_mut(child_thread_id) {
            if !child.admit_turn(turn_id) {
                return Vec::new();
            }
            return self.settle_child(
                child_thread_id,
                match outcome {
                    NativeTurnOutcome::Completed => ProviderSubagentStatus::Completed,
                    NativeTurnOutcome::Interrupted => ProviderSubagentStatus::Interrupted,
                    NativeTurnOutcome::Failed { .. } => ProviderSubagentStatus::Failed,
                },
            );
        }
        if let Some(child) = self.settled_children.get_mut(child_thread_id)
            && !turn_id.is_empty()
        {
            if child.unclaimed_turn.as_deref() == Some(turn_id) {
                child.unclaimed_turn = None;
            }
            child.past_turns.insert(turn_id.to_owned());
        }
        Vec::new()
    }

    /// Records a delegation to a child with no stretch open, as the resume it
    /// becomes once the child starts a native turn — at once, if the child's
    /// turn already began. Codex reports the delegation and the turn it starts
    /// on different threads, in no promised order, so either may come first.
    /// The child's thread is attached again on the first claim, so the turn it
    /// runs streams here even when the thread was reloaded since — reopened by
    /// `resumeAgent`, or spawned before a restart. A delegation to a working
    /// child resumes nothing.
    fn claim_resume(
        &mut self,
        child_thread_id: &str,
        claim: ResumeClaim,
    ) -> Vec<AttributedProviderEvent> {
        if child_thread_id == self.thread_id || self.children.contains_key(child_thread_id) {
            return Vec::new();
        }
        let child = self
            .settled_children
            .entry(child_thread_id.to_owned())
            .or_default();
        if child.claim.replace(claim).is_none() {
            self.pending_attaches.push(child_thread_id.to_owned());
        }
        let child = &self.settled_children[child_thread_id];
        match child.unclaimed_turn.clone() {
            Some(turn_id) => self.resume_child(child_thread_id, turn_id),
            None => Vec::new(),
        }
    }

    /// A settled child naming a native turn — its `turn/started`, or any item
    /// the turn streams. A turn no earlier stretch ran in is a new one: where
    /// a delegation claims it, the child resumes into it; otherwise it waits
    /// for the claim, and what it streams meanwhile lands nowhere. The turn
    /// input Suru handed the child itself runs in wakes it instead, whether
    /// new or one Suru saw settle that Codex steered the input into.
    fn wake_settled_child(
        &mut self,
        child_thread_id: &str,
        turn_id: &str,
    ) -> Vec<AttributedProviderEvent> {
        let Some(child) = self.settled_children.get_mut(child_thread_id) else {
            return Vec::new();
        };
        if turn_id.is_empty() {
            return Vec::new();
        }
        let handed = match &child.handed_wake {
            Some(HandedWake::Took(taken)) => taken == turn_id,
            Some(HandedWake::Requested) => {
                child.claim.is_none() && !child.past_turns.contains(turn_id)
            }
            None => false,
        };
        if handed {
            return self.wake_for_handed_input(child_thread_id, turn_id.to_owned());
        }
        if child.past_turns.contains(turn_id) {
            return Vec::new();
        }
        if child.claim.is_none() {
            child.unclaimed_turn = Some(turn_id.to_owned());
            return Vec::new();
        }
        self.resume_child(child_thread_id, turn_id.to_owned())
    }

    /// Resumes a settled child into the native turn its claim started: the
    /// thread is followed again, its next stretch measured and priced from
    /// where its total stands, and the resume announced in the conversation
    /// that delegated it, carrying what that delegation said. The Model the
    /// child last ran on follows it into the new Turn until fresher evidence
    /// arrives.
    ///
    /// A resume the Session's own thread delegated can begin after that
    /// thread's turn completed, since the child's turn is reported apart from
    /// it. The resume then lands in a Continuation, and Codex runs no turn of
    /// the parent's to end it, so its end is reported with the resume: the
    /// Continuation holds the row and settles at once, and the child keeps
    /// the parent Working until it settles too (ADR 0015).
    fn resume_child(
        &mut self,
        child_thread_id: &str,
        turn_id: String,
    ) -> Vec<AttributedProviderEvent> {
        let Some(mut child) = self.settled_children.remove(child_thread_id) else {
            return Vec::new();
        };
        let Some(claim) = child.claim.take() else {
            self.settled_children
                .insert(child_thread_id.to_owned(), child);
            return Vec::new();
        };
        child.delegator = Some(claim.delegator.clone());
        child.opening = StretchOpening::Awaited(claim.delegation.clone());
        let model = self.reopen_child(child_thread_id, child, turn_id);
        let delegated_while_idle = claim.delegator == ProviderEventAttribution::OwningSession
            && self.active_turn_id.is_none()
            && !self.turn_starting;
        let subagent_id = ProviderSubagentId::new(child_thread_id);
        let mut resumed = vec![AttributedProviderEvent {
            attribution: claim.delegator,
            event: ProviderEvent::SubagentResumed {
                subagent_id: subagent_id.clone(),
                name: claim.name,
                description: claim.description,
                delegation: claim.delegation,
            },
        }];
        resumed.extend(model.map(|model| AttributedProviderEvent {
            attribution: ProviderEventAttribution::OwningSession,
            event: ProviderEvent::SubagentModelChanged { subagent_id, model },
        }));
        if delegated_while_idle {
            resumed.push(ProviderEvent::TurnCompleted.into());
        }
        resumed
    }

    /// Wakes a settled child into native turn `turn_id`, which input Suru
    /// handed the child itself runs in (see [`HandedWake`]). No Delegation
    /// began the stretch, so it is no resume and gains no row: it is woken,
    /// as a Watch wakes a Subagent, into a Continuation of its own Session —
    /// which Suru has already opened where it found the child settled as
    /// Codex took the input, and which the wake opens where it found the
    /// child still working. The input stands nowhere. The thread is followed
    /// again, and the Model the child last ran on follows it into the new
    /// Turn until fresher evidence arrives.
    fn wake_for_handed_input(
        &mut self,
        child_thread_id: &str,
        turn_id: String,
    ) -> Vec<AttributedProviderEvent> {
        let Some(mut child) = self.settled_children.remove(child_thread_id) else {
            return Vec::new();
        };
        child.past_turns.remove(&turn_id);
        child.delegator = None;
        child.opening = StretchOpening::Passed;
        let model = self.reopen_child(child_thread_id, child, turn_id);
        let subagent_id = ProviderSubagentId::new(child_thread_id);
        let mut woken = vec![AttributedProviderEvent {
            attribution: ProviderEventAttribution::OwningSession,
            event: ProviderEvent::SubagentWoken {
                subagent_id: subagent_id.clone(),
            },
        }];
        woken.extend(model.map(|model| AttributedProviderEvent {
            attribution: ProviderEventAttribution::OwningSession,
            event: ProviderEvent::SubagentModelChanged { subagent_id, model },
        }));
        woken
    }

    /// Follows a settled child's thread again for the stretch native turn
    /// `turn_id` begins — measured and priced from where the thread's total
    /// stands — answering the Model it last ran on.
    fn reopen_child(
        &mut self,
        child_thread_id: &str,
        mut child: AttachedChild,
        turn_id: String,
    ) -> Option<ModelId> {
        child.unclaimed_turn = None;
        child.handed_wake = None;
        child.stretch_turns = HashSet::from([turn_id.clone()]);
        child.latest_turn_id = Some(turn_id.clone());
        child.resumed_turn = Some(turn_id.clone());
        child.pricing_baseline = NativeCumulativeUsage::default();
        child.unpriced_prefix = false;
        child.estimate_blocked = false;
        child.worked_here = true;
        let model = child.model.clone();
        self.children.insert(child_thread_id.to_owned(), child);
        self.child_context_turns
            .entry(child_thread_id.to_owned())
            .or_default()
            .observe(&turn_id);
        model
    }

    /// Readies this connection for input Suru is about to hand the child on
    /// `child_thread_id` itself, with a `turn/start` of its own on the child's
    /// thread — Subagent Reports (ADR 0035) — which reads `text` as the
    /// `UserMessage` it arrives as. A working child's running turn takes it
    /// as a steer; a settled child — or one this connection never followed,
    /// spawned before a restart — begins a native turn with it, which wakes
    /// the child into a stretch of its own (see [`HandedWake`]). Either way the
    /// input stands nowhere. Answers whether the child's thread has to be
    /// attached first, so the turn streams here: one with no stretch open may
    /// have been reloaded since, as a delegation's resume allows for.
    pub(super) fn expect_handed_input(
        &mut self,
        child_thread_id: &str,
        text: String,
    ) -> Result<bool, ProviderError> {
        if child_thread_id == self.thread_id {
            return Err(codex_error(
                "Codex runs no Subagent on the Session's own thread",
            ));
        }
        if let Some(child) = self.children.get_mut(child_thread_id) {
            child.handed_input.push(text);
            return Ok(false);
        }
        let child = self
            .settled_children
            .entry(child_thread_id.to_owned())
            .or_default();
        child.handed_input.push(text);
        // A delegation already waiting on the child's next turn resumes it
        // there, row and all, and the input rides that turn. Known race, left
        // for a later slice: with such a claim pending no wake is marked here,
        // yet the orchestrator still wakes the child as Codex takes the input,
        // a Continuation the resume begun milliseconds later then settles.
        if child.claim.is_none() {
            child.handed_wake = Some(HandedWake::Requested);
        }
        Ok(true)
    }

    /// Codex took the input Suru handed the child on `child_thread_id` into
    /// native turn `turn_id`. For a settled child that is the stretch it
    /// wakes into, even one Suru saw settle before Codex steered the input
    /// into it. For a working child it is the turn the stretch works in,
    /// which the input steered — unless that turn had ended before the input
    /// arrived and Codex began another with it, which wakes the child into a
    /// stretch of its own once the one it worked in settles.
    pub(super) fn handed_input_taken(&mut self, child_thread_id: &str, turn_id: String) {
        if let Some(child) = self.children.get_mut(child_thread_id) {
            if child.latest_turn_id.as_deref() != Some(turn_id.as_str()) {
                child.handed_wake = Some(HandedWake::Took(turn_id));
            }
            return;
        }
        let Some(child) = self.settled_children.get_mut(child_thread_id) else {
            return;
        };
        if child.handed_wake.is_none() {
            return;
        }
        if child.unclaimed_turn.as_deref() == Some(turn_id.as_str()) {
            child.unclaimed_turn = None;
        }
        child.handed_wake = Some(HandedWake::Took(turn_id));
    }

    /// Forgets input reading `text` that Suru meant to hand the child on
    /// `child_thread_id`, which Codex never took.
    pub(super) fn forget_handed_input(&mut self, child_thread_id: &str, text: &str) {
        let settled = self.settled_children.get_mut(child_thread_id);
        let is_settled = settled.is_some();
        let Some(child) = settled.or_else(|| self.children.get_mut(child_thread_id)) else {
            return;
        };
        if let Some(handed) = child.handed_input.iter().position(|handed| handed == text) {
            child.handed_input.remove(handed);
        }
        if is_settled {
            child.handed_wake = None;
        }
    }

    /// Keeps the Model a settled child's thread runs on, as the attach made
    /// before handing it input answered, for the stretch that input wakes it
    /// into.
    pub(super) fn note_settled_child_model(&mut self, child_thread_id: &str, model: ModelId) {
        if let Some(child) = self.settled_children.get_mut(child_thread_id) {
            child.model = Some(model);
        }
    }
}

/// One thread's item-step events, each attributed to that thread's Session.
fn attributed(
    attribution: &ProviderEventAttribution,
    events: Vec<ProviderEvent>,
) -> Vec<AttributedProviderEvent> {
    events
        .into_iter()
        .map(|event| AttributedProviderEvent {
            attribution: attribution.clone(),
            event,
        })
        .collect()
}

/// The events of the Session's own conversation, which every non-item
/// notification speaks for.
fn owning(events: Vec<ProviderEvent>) -> Vec<AttributedProviderEvent> {
    events
        .into_iter()
        .map(AttributedProviderEvent::from)
        .collect()
}

/// Streams the Provider events projected from `notifications`, holding the process open meanwhile.
pub(super) fn provider_events(
    notifications: mpsc::UnboundedReceiver<Result<NativeNotification, ProviderError>>,
    process: Arc<ProcessGuard>,
    correlation: Arc<StdMutex<NativeCorrelation>>,
    skill_catalog_invalidations: tokio::sync::watch::Sender<u64>,
    attachment: ChildThreadAttachment,
    pricing: Option<Arc<PricingSource>>,
) -> ProviderEventStream {
    let (attached_models, attached_model_results) = mpsc::unbounded_channel();
    Box::pin(stream::unfold(
        GuardedEventReceiver {
            receiver: notifications,
            _process: process,
            correlation,
            skill_catalog_invalidations,
            attachment,
            attached_models,
            attached_model_results,
            pricing,
            pending: VecDeque::new(),
        },
        next_provider_event,
    ))
}

/// What attaching a child thread takes: the transport the Session speaks
/// over, and the working directory its threads run under. The event pump
/// attaches each spawned child with it, and the Session each settled child it
/// hands input to itself.
#[derive(Clone)]
pub(super) struct ChildThreadAttachment {
    pub(super) transport: JsonRpcTransport,
    pub(super) cwd: String,
    pub(super) posture: Arc<std::sync::Mutex<CodexPosture>>,
}

impl ChildThreadAttachment {
    /// Requests the child thread's stream, so its items reach this connection.
    /// The resumed thread's own lineage — the parent it declares — is not
    /// re-checked: the spawn item on the parent's stream already named the
    /// relationship, and it is the spawner's account Suru follows. The attach
    /// carries no `config`: a child thread inherits its parent's per-thread MCP
    /// servers, the Broker's included, and Codex ignores a `config` on a
    /// running thread and may rebuild an idle one from it.
    /// Fire-and-forget: a child Codex will not hand over leaves its Subagent's
    /// Session sparse — settled by the lifecycle items the spawner's thread
    /// still carries — rather than failing the parent's.
    fn attach(&self, thread_id: String, results: mpsc::UnboundedSender<(String, ModelId)>) {
        let attachment = self.clone();
        tokio::spawn(async move {
            if let Ok(Some(model)) = attachment.request_stream(&thread_id).await {
                let _ = results.send((thread_id, model));
            }
        });
    }

    /// Requests the child thread's stream, as [`Self::attach`] does, and waits
    /// for Codex to hand it over, answering the Model the thread runs on where
    /// Codex names one.
    pub(super) async fn request_stream(
        &self,
        thread_id: &str,
    ) -> Result<Option<ModelId>, ProviderError> {
        let posture = *self
            .posture
            .lock()
            .expect("Codex Session posture lock is not poisoned");
        let result = self
            .transport
            .request(
                "thread/resume",
                &ThreadResumeParams {
                    thread_id,
                    cwd: &self.cwd,
                    approval_policy: posture.approval_policy(),
                    sandbox: posture.sandbox(),
                    config: None,
                    developer_instructions: None,
                },
            )
            .await
            .map_err(|error| {
                codex_error_context("Codex did not attach the Subagent's thread", error)
            })?;
        let attached = serde_json::from_value::<ThreadConnectionResult>(result)
            .map_err(|_| codex_error("Codex returned an invalid thread/resume response"))?;
        if attached.thread.id != thread_id {
            return Err(codex_error(
                "Codex returned an invalid thread/resume response: thread ID did not match",
            ));
        }
        Ok((!attached.model.is_empty()).then(|| ModelId::new(attached.model)))
    }
}

struct GuardedEventReceiver {
    receiver: mpsc::UnboundedReceiver<Result<NativeNotification, ProviderError>>,
    _process: Arc<ProcessGuard>,
    correlation: Arc<StdMutex<NativeCorrelation>>,
    skill_catalog_invalidations: tokio::sync::watch::Sender<u64>,
    attachment: ChildThreadAttachment,
    attached_models: mpsc::UnboundedSender<(String, ModelId)>,
    attached_model_results: mpsc::UnboundedReceiver<(String, ModelId)>,
    /// The rate table a Codex Cost is estimated from. Absent where the Session
    /// was started without one, which leaves Costs absent and tokens intact —
    /// the same reading as a rate table that has never been fetched.
    pricing: Option<Arc<PricingSource>>,
    pending: VecDeque<Result<AttributedProviderEvent, ProviderError>>,
}

async fn next_provider_event(
    mut events: GuardedEventReceiver,
) -> Option<(
    Result<AttributedProviderEvent, ProviderError>,
    GuardedEventReceiver,
)> {
    loop {
        if let Some(event) = events.pending.pop_front() {
            return Some((event, events));
        }
        let native = tokio::select! {
            native = events.receiver.recv() => native?,
            attached = events.attached_model_results.recv() => {
                let (thread_id, model) = attached?;
                let observed = events
                    .correlation
                    .lock()
                    .expect("Codex native correlation lock is not poisoned")
                    .observe_child_model(&thread_id, model, true);
                events.pending.extend(observed.into_iter().map(Ok));
                continue;
            }
        };
        match native {
            Err(error) => return Some((Err(error), events)),
            Ok(native) => {
                if matches!(native, NativeNotification::SkillsChanged) {
                    events
                        .skill_catalog_invalidations
                        .send_modify(|generation| {
                            *generation = generation.saturating_add(1);
                        });
                    continue;
                }
                let (projected, attaches) = {
                    let mut correlation = events
                        .correlation
                        .lock()
                        .expect("Codex native correlation lock is not poisoned");
                    let projected = project_native_notification(&mut correlation, native);
                    let projected = projected.map(|projected| {
                        projected
                            .into_iter()
                            .map(|event| {
                                estimate_cost(&mut correlation, events.pricing.as_deref(), event)
                            })
                            .collect::<Vec<_>>()
                    });
                    (projected, correlation.take_pending_attaches())
                };
                for thread_id in attaches {
                    events
                        .attachment
                        .attach(thread_id, events.attached_models.clone());
                }
                match projected {
                    Ok(projected) => {
                        events.pending.extend(projected.into_iter().map(Ok));
                    }
                    Err(error) => events.pending.push_back(Err(error)),
                }
            }
        }
    }
}

/// Prices a Usage event from the rate table, which is the only Cost a Codex
/// Turn can carry: Codex reports tokens and never dollars, so the Basis is
/// always Estimated. A Model the table does not price, a table no fetch has
/// ever filled, and an event that is not a Usage all pass through unchanged,
/// leaving the Cost absent rather than zero.
///
/// A Subagent is priced only after its own attach/settings evidence establishes
/// a Model. Usage already observed before that evidence becomes the pricing
/// baseline, and an ambiguous later Model change stops further estimates until
/// a resume begins another stretch.
fn estimate_cost(
    correlation: &mut NativeCorrelation,
    pricing: Option<&PricingSource>,
    mut event: AttributedProviderEvent,
) -> AttributedProviderEvent {
    let (Some(pricing), ProviderEvent::Usage { usage, cost }) = (pricing, &mut event.event) else {
        return event;
    };
    let (model, priceable_usage, partial) = match &event.attribution {
        ProviderEventAttribution::OwningSession => {
            (correlation.metered_model.clone(), usage.clone(), false)
        }
        ProviderEventAttribution::Subagent(subagent) => {
            let Some(child) = correlation.children.get(subagent.as_str()) else {
                return event;
            };
            let Some(model) = child.model.clone() else {
                return event;
            };
            if child.estimate_blocked {
                return event;
            }
            // Priced from whichever came later: the Model becoming known, or
            // the stretch beginning — a resume's Turn prices only its own.
            (
                model,
                child
                    .metering
                    .latest
                    .since(child.pricing_baseline.furthest_of(child.metering.baseline)),
                child.unpriced_prefix,
            )
        }
    };
    let estimated = pricing.estimate_cached(
        &ModelsDevModel::new(super::MODELS_DEV_PROVIDER, model),
        &priceable_usage,
    );
    *cost = estimated.map(|estimated| {
        let metered = MeteredCost::from(estimated);
        if partial { metered.partial() } else { metered }
    });
    event
}

fn has_billable_usage(usage: &Usage) -> bool {
    [
        usage.fresh_input_tokens,
        usage.cache_read_tokens,
        usage.cache_write_tokens,
        usage.output_tokens,
        usage.reasoning_tokens,
    ]
    .into_iter()
    .flatten()
    .any(|count| count > 0)
}

/// The thread and native turn a streamed step names, for the notifications
/// that carry the work a turn does. These are what can show a settled child
/// working again; a running total can not, because attaching a thread replays
/// the total its last turn reached.
fn streamed_step_turn(notification: &NativeNotification) -> Option<(&str, &str)> {
    match notification {
        NativeNotification::QuestionnaireRequested { params, .. } => {
            Some((&params.thread_id, &params.turn_id))
        }
        NativeNotification::CommandApprovalRequested { params, .. } => {
            Some((&params.thread_id, &params.turn_id))
        }
        NativeNotification::FileChangeApprovalRequested { params, .. } => {
            Some((&params.thread_id, &params.turn_id))
        }
        NativeNotification::PermissionsApprovalRequested { params, .. } => {
            Some((&params.thread_id, &params.turn_id))
        }
        NativeNotification::AgentMessageStarted {
            thread_id, turn_id, ..
        }
        | NativeNotification::AgentMessageDelta {
            thread_id, turn_id, ..
        }
        | NativeNotification::AgentMessageCompleted {
            thread_id, turn_id, ..
        }
        | NativeNotification::CommandStarted {
            thread_id, turn_id, ..
        }
        | NativeNotification::CommandOutputDelta {
            thread_id, turn_id, ..
        }
        | NativeNotification::CommandCompleted {
            thread_id, turn_id, ..
        }
        | NativeNotification::FileChangeStarted {
            thread_id, turn_id, ..
        }
        | NativeNotification::FileChangeUpdated {
            thread_id, turn_id, ..
        }
        | NativeNotification::FileChangeCompleted {
            thread_id, turn_id, ..
        }
        | NativeNotification::ReasoningStarted {
            thread_id, turn_id, ..
        }
        | NativeNotification::ReasoningDelta {
            thread_id, turn_id, ..
        }
        | NativeNotification::ReasoningSectionBreak {
            thread_id, turn_id, ..
        }
        | NativeNotification::ReasoningCompleted {
            thread_id, turn_id, ..
        }
        | NativeNotification::ToolUseStarted {
            thread_id, turn_id, ..
        }
        | NativeNotification::ToolUseCompleted {
            thread_id, turn_id, ..
        }
        | NativeNotification::UserMessage {
            thread_id, turn_id, ..
        } => Some((thread_id, turn_id)),
        NativeNotification::TurnStarted { .. }
        | NativeNotification::QuestionnaireResolved { .. }
        | NativeNotification::SkillsChanged
        | NativeNotification::AgentSelectionChanged { .. }
        | NativeNotification::TurnCompleted { .. }
        | NativeNotification::TokenUsage { .. }
        | NativeNotification::CollabCallStarted { .. }
        | NativeNotification::CollabCallCompleted { .. }
        | NativeNotification::SubagentActivity { .. } => None,
    }
}

fn project_native_notification(
    correlation: &mut NativeCorrelation,
    notification: NativeNotification,
) -> Result<Vec<AttributedProviderEvent>, ProviderError> {
    // A step of a settled child's new native turn resumes it first, when a
    // delegation claims the turn, so the step lands in the Turn that resume
    // begins.
    let mut projected = match streamed_step_turn(&notification) {
        Some((thread_id, turn_id)) if thread_id != correlation.thread_id => {
            correlation.wake_settled_child(thread_id, turn_id)
        }
        _ => Vec::new(),
    };
    projected.extend(project_notification(correlation, notification)?);
    Ok(projected)
}

fn project_notification(
    correlation: &mut NativeCorrelation,
    notification: NativeNotification,
) -> Result<Vec<AttributedProviderEvent>, ProviderError> {
    match notification {
        NativeNotification::TurnStarted { thread_id, turn_id } => {
            if let Some(turns) = correlation.child_context_turns.get_mut(&thread_id) {
                turns.observe(&turn_id);
            }
            if thread_id != correlation.thread_id {
                return Ok(project_child_turn_started(
                    correlation,
                    &thread_id,
                    &turn_id,
                ));
            }
            if correlation.settled_turns.contains(&turn_id)
                || correlation.active_turn_id.as_deref() == Some(&turn_id)
            {
                return Ok(Vec::new());
            }
            if turn_id.is_empty() {
                return Err(codex_error("Codex reported an empty Turn ID"));
            }
            if correlation.turn_starting || correlation.active_turn_id.is_some() {
                return Err(codex_error(
                    "Codex started a Turn while another native Turn was active",
                ));
            }
            let Some(selection) = correlation.effective_selection.clone() else {
                return Ok(Vec::new());
            };
            correlation.context_fill_turn = None;
            correlation.context_native_turn = Some(turn_id.clone());
            correlation.active_turn_id = Some(turn_id);
            Ok(owning(vec![ProviderEvent::ContinuationStarted {
                selection,
            }]))
        }
        NativeNotification::QuestionnaireRequested { id, params } => {
            let Some((_, attribution)) =
                correlation.item_thread(&params.thread_id, &params.turn_id)
            else {
                return Ok(Vec::new());
            };
            let questionnaire = correlation.questionnaires.register(id, params)?;
            Ok(attributed(
                &attribution,
                vec![ProviderEvent::QuestionnaireRequested { questionnaire }],
            ))
        }
        NativeNotification::CommandApprovalRequested { id, params } => {
            project_command_approval(correlation, id, params)
        }
        NativeNotification::FileChangeApprovalRequested { id, params } => {
            project_file_change_approval(correlation, id, params)
        }
        NativeNotification::PermissionsApprovalRequested {
            id,
            params,
            native_permissions,
        } => project_permissions_approval(correlation, id, params, native_permissions),
        NativeNotification::QuestionnaireResolved {
            thread_id,
            request_id,
        } => {
            let id = correlation.questionnaires.resolve(&thread_id, &request_id);
            let approval_id = correlation.approvals.resolve(&request_id);
            Ok(match correlation.spawner_attribution(&thread_id) {
                Some(attribution) if id.is_some() || approval_id.is_some() => attributed(
                    &attribution,
                    id.map(|id| ProviderEvent::QuestionnaireWithdrawn { id })
                        .into_iter()
                        .chain(approval_id.map(|id| ProviderEvent::ApprovalWithdrawn { id }))
                        .collect(),
                ),
                _ => Vec::new(),
            })
        }
        NativeNotification::SkillsChanged => Ok(Vec::new()),
        NativeNotification::AgentSelectionChanged {
            thread_id,
            model,
            effort,
            service_tier,
        } => {
            if correlation.thread_id == thread_id {
                project_agent_selection_changed(
                    correlation,
                    &thread_id,
                    model,
                    effort,
                    service_tier,
                )
                .map(owning)
            } else if model.is_empty() {
                Ok(Vec::new())
            } else {
                Ok(correlation.observe_child_model(&thread_id, ModelId::new(model), false))
            }
        }
        NativeNotification::AgentMessageStarted {
            thread_id,
            turn_id,
            item_id,
        } => project_agent_message_started(correlation, &thread_id, &turn_id, item_id),
        NativeNotification::AgentMessageDelta {
            thread_id,
            turn_id,
            item_id,
            delta,
        } => project_agent_message_delta(correlation, &thread_id, &turn_id, &item_id, delta),
        NativeNotification::AgentMessageCompleted {
            thread_id,
            turn_id,
            item_id,
            text,
        } => project_agent_message_completed(correlation, &thread_id, &turn_id, &item_id, &text),
        NativeNotification::CommandStarted {
            thread_id,
            turn_id,
            item_id,
            command,
            cwd,
            status,
        } => project_command_started(
            correlation,
            &thread_id,
            &turn_id,
            item_id,
            command,
            cwd,
            status,
        ),
        NativeNotification::CommandOutputDelta {
            thread_id,
            turn_id,
            item_id,
            delta,
        } => project_command_output_delta(correlation, &thread_id, &turn_id, item_id, delta),
        NativeNotification::CommandCompleted {
            thread_id,
            turn_id,
            item_id,
            aggregated_output,
            exit_status,
            status,
        } => project_command_completed(
            correlation,
            &thread_id,
            &turn_id,
            item_id,
            aggregated_output,
            exit_status,
            status,
        ),
        NativeNotification::FileChangeStarted {
            thread_id,
            turn_id,
            item_id,
            changes,
            status,
        } => {
            project_file_change_started(correlation, &thread_id, &turn_id, item_id, changes, status)
        }
        NativeNotification::FileChangeUpdated {
            thread_id,
            turn_id,
            item_id,
            changes,
        } => project_file_change_updated(correlation, &thread_id, &turn_id, item_id, changes),
        NativeNotification::FileChangeCompleted {
            thread_id,
            turn_id,
            item_id,
            changes,
            status,
        } => project_file_change_completed(
            correlation,
            &thread_id,
            &turn_id,
            item_id,
            changes,
            status,
        ),
        NativeNotification::ReasoningStarted {
            thread_id,
            turn_id,
            item_id,
        } => project_reasoning_started(correlation, &thread_id, &turn_id, item_id),
        NativeNotification::ReasoningDelta {
            thread_id,
            turn_id,
            item_id,
            delta,
            summary_index,
        } => project_reasoning_delta(
            correlation,
            &thread_id,
            &turn_id,
            item_id,
            &delta,
            summary_index,
        ),
        NativeNotification::ReasoningSectionBreak {
            thread_id,
            turn_id,
            item_id,
            summary_index,
        } => project_reasoning_section_break(
            correlation,
            &thread_id,
            &turn_id,
            item_id,
            summary_index,
        ),
        NativeNotification::ReasoningCompleted {
            thread_id,
            turn_id,
            item_id,
            summary,
        } => project_reasoning_completed(correlation, &thread_id, &turn_id, item_id, summary),
        NativeNotification::ToolUseStarted {
            thread_id,
            turn_id,
            tool,
        } => Ok(project_tool_use_started(
            correlation,
            &thread_id,
            &turn_id,
            &tool,
        )),
        NativeNotification::ToolUseCompleted {
            thread_id,
            turn_id,
            tool,
        } => Ok(project_tool_use_completed(
            correlation,
            &thread_id,
            &turn_id,
            &tool,
        )),
        NativeNotification::TokenUsage {
            thread_id,
            turn_id,
            total,
            context_fill,
        } => Ok(project_token_usage(
            correlation,
            &thread_id,
            &turn_id,
            total,
            context_fill,
        )),
        NativeNotification::TurnCompleted {
            thread_id,
            turn_id,
            outcome,
            final_agent_message,
        } => {
            if let Some(turns) = correlation.child_context_turns.get_mut(&thread_id) {
                turns.observe(&turn_id);
            }
            let withdrawn = correlation.questionnaires.end_turn(&thread_id, &turn_id);
            let withdrawn_approvals = correlation.approvals.end_turn(&thread_id, &turn_id);
            if thread_id == correlation.thread_id {
                project_turn_completed(
                    correlation,
                    &thread_id,
                    &turn_id,
                    outcome,
                    final_agent_message,
                )
                .map(owning)
            } else {
                let mut projected = correlation
                    .spawner_attribution(&thread_id)
                    .map(|attribution| {
                        attributed(
                            &attribution,
                            withdrawn
                                .into_iter()
                                .map(|id| ProviderEvent::QuestionnaireWithdrawn { id })
                                .chain(
                                    withdrawn_approvals
                                        .into_iter()
                                        .map(|id| ProviderEvent::ApprovalWithdrawn { id }),
                                )
                                .collect(),
                        )
                    })
                    .unwrap_or_default();
                projected.extend(correlation.finish_child_turn(&thread_id, &turn_id, &outcome));
                Ok(projected)
            }
        }
        NativeNotification::UserMessage {
            thread_id,
            turn_id,
            text,
        } => Ok(project_user_message(
            correlation,
            &thread_id,
            &turn_id,
            text,
        )),
        NativeNotification::CollabCallStarted {
            thread_id,
            call_id,
            tool,
            receiver_thread_ids,
            prompt,
        } => {
            project_collab_call_started(
                correlation,
                &thread_id,
                &call_id,
                tool,
                &receiver_thread_ids,
                prompt.as_deref(),
            );
            Ok(Vec::new())
        }
        NativeNotification::CollabCallCompleted {
            thread_id,
            call_id,
            tool,
            status,
            receiver_thread_ids,
            prompt,
            agents_states,
        } => Ok(project_collab_call_completed(
            correlation,
            &thread_id,
            CollabCall {
                call_id,
                tool,
                status,
                receiver_thread_ids,
                prompt,
                agents_states,
            },
        )),
        NativeNotification::SubagentActivity {
            thread_id,
            kind,
            agent_thread_id,
            agent_path,
        } => Ok(project_subagent_activity(
            correlation,
            &thread_id,
            kind,
            agent_thread_id,
            &agent_path,
        )),
    }
}

fn project_command_approval(
    correlation: &mut NativeCorrelation,
    request_id: super::wire::RequestId,
    params: super::wire::CommandApprovalParams,
) -> Result<Vec<AttributedProviderEvent>, ProviderError> {
    let Some((_, attribution)) = correlation.item_thread(&params.thread_id, &params.turn_id) else {
        return Ok(Vec::new());
    };
    let subject = if let Some(command) = params.command.clone() {
        ApprovalSubject::Command {
            command,
            cwd: params.cwd.clone(),
            actions: params
                .command_actions
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(lower_command_action)
                .collect(),
        }
    } else if let Some(network) = &params.network_approval_context {
        ApprovalSubject::Network {
            host_or_url: format!("{}://{}", network.protocol, network.host),
        }
    } else {
        ApprovalSubject::OtherTool {
            name: "Codex command execution".to_owned(),
            input: serde_json::json!({ "itemId": params.item_id }),
        }
    };
    let id = correlation.approvals.register(NativeApprovalIdentity {
        request_id,
        thread_id: params.thread_id,
        turn_id: params.turn_id,
        kind: NativeApprovalKind::Command,
    })?;
    Ok(attributed(
        &attribution,
        vec![ProviderEvent::ApprovalRequested {
            approval: Approval {
                id,
                subject,
                reason: params.reason,
            },
            tool_activity_id: Some(ProviderActivityId::new(params.item_id)),
        }],
    ))
}

fn lower_command_action(action: &Value) -> CommandAction {
    let command = action
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    match action.get("type").and_then(Value::as_str) {
        Some("read") => match (
            action.get("name").and_then(Value::as_str),
            action.get("path").and_then(Value::as_str),
        ) {
            (Some(name), Some(path)) => CommandAction::Read {
                command,
                name: name.to_owned(),
                path: PathBuf::from(path),
            },
            _ => CommandAction::Unknown { command },
        },
        Some("listFiles") => CommandAction::ListFiles {
            command,
            path: action
                .get("path")
                .and_then(Value::as_str)
                .map(PathBuf::from),
        },
        Some("search") => CommandAction::Search {
            command,
            query: action
                .get("query")
                .and_then(Value::as_str)
                .map(str::to_owned),
            path: action
                .get("path")
                .and_then(Value::as_str)
                .map(PathBuf::from),
        },
        _ => CommandAction::Unknown { command },
    }
}

fn project_file_change_approval(
    correlation: &mut NativeCorrelation,
    request_id: super::wire::RequestId,
    params: super::wire::FileChangeApprovalParams,
) -> Result<Vec<AttributedProviderEvent>, ProviderError> {
    let Some((thread, attribution)) = correlation.item_thread(&params.thread_id, &params.turn_id)
    else {
        return Ok(Vec::new());
    };
    let Some(file_change) = thread.active_file_changes.get(&params.item_id) else {
        return Err(codex_error(
            "Codex requested file-change Approval before starting its item",
        ));
    };
    let paths = file_change
        .changes
        .iter()
        .flat_map(|change| match change {
            FileChange::Add { path } | FileChange::Delete { path } => vec![path.clone()],
            FileChange::Update { path, moved_to } => {
                let mut paths = vec![path.clone()];
                paths.extend(moved_to.clone());
                paths
            }
        })
        .collect();
    let id = correlation.approvals.register(NativeApprovalIdentity {
        request_id,
        thread_id: params.thread_id,
        turn_id: params.turn_id,
        kind: NativeApprovalKind::FileChange,
    })?;
    Ok(attributed(
        &attribution,
        vec![ProviderEvent::ApprovalRequested {
            approval: Approval {
                id,
                subject: ApprovalSubject::FileChange {
                    paths,
                    grant_root: params.grant_root,
                },
                reason: params.reason,
            },
            tool_activity_id: Some(ProviderActivityId::new(params.item_id)),
        }],
    ))
}

fn project_permissions_approval(
    correlation: &mut NativeCorrelation,
    request_id: super::wire::RequestId,
    params: super::wire::PermissionsApprovalParams,
    native_permissions: Value,
) -> Result<Vec<AttributedProviderEvent>, ProviderError> {
    let Some((_, attribution)) = correlation.item_thread(&params.thread_id, &params.turn_id) else {
        return Ok(Vec::new());
    };
    let id = correlation.approvals.register(NativeApprovalIdentity {
        request_id,
        thread_id: params.thread_id,
        turn_id: params.turn_id,
        kind: NativeApprovalKind::Permissions {
            requested: native_permissions,
        },
    })?;
    Ok(attributed(
        &attribution,
        vec![ProviderEvent::ApprovalRequested {
            approval: Approval {
                id,
                subject: ApprovalSubject::PermissionGrant {
                    profile: params.permissions,
                },
                reason: params.reason,
            },
            tool_activity_id: Some(ProviderActivityId::new(params.item_id)),
        }],
    ))
}

/// A child thread's native turn starting. A working child's stretch takes the
/// turn as its own, so a stop can address it before any item names it; a
/// settled child's new turn is the one a delegation's resume runs in.
fn project_child_turn_started(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
) -> Vec<AttributedProviderEvent> {
    if let Some(child) = correlation.children.get_mut(thread_id) {
        if !turn_id.is_empty() && child.admit_turn(turn_id) {
            child.latest_turn_id = Some(turn_id.to_owned());
        }
        return Vec::new();
    }
    correlation.wake_settled_child(thread_id, turn_id)
}

/// One step of a spawned agent's lifecycle, as the spawner's thread reports
/// it: a start opens the agent's thread as a Subagent, and the terminal kinds
/// settle its stretch. An interaction with a settled agent — the V2
/// `followupTask`, and `sendMessage` too, which reports the same — claims the
/// native turn it may start as a resume; with a working agent it revises
/// nothing the row shows. The activity names only the agent, never what it
/// was handed, so a spawn or resume reported this way opens its child Turn
/// with no Delegation and its row with no description: both wait for the
/// agent's own first input, which says what it was handed.
fn project_subagent_activity(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    kind: NativeSubagentActivityKind,
    agent_thread_id: String,
    agent_path: &str,
) -> Vec<AttributedProviderEvent> {
    let Some(spawner) = correlation.spawner_attribution(thread_id) else {
        return Vec::new();
    };
    match kind {
        NativeSubagentActivityKind::Started => correlation.spawn_child(
            spawner,
            agent_thread_id,
            subagent_name_from_path(agent_path),
            String::new(),
            None,
        ),
        NativeSubagentActivityKind::Completed => {
            correlation.settle_child(&agent_thread_id, ProviderSubagentStatus::Completed)
        }
        NativeSubagentActivityKind::Interrupted => {
            correlation.settle_child(&agent_thread_id, ProviderSubagentStatus::Interrupted)
        }
        NativeSubagentActivityKind::Interacted if agent_thread_id != thread_id => correlation
            .claim_resume(
                &agent_thread_id,
                ResumeClaim {
                    delegator: spawner,
                    name: subagent_name_from_path(agent_path),
                    description: String::new(),
                    delegation: None,
                },
            ),
        NativeSubagentActivityKind::Interacted | NativeSubagentActivityKind::Other => Vec::new(),
    }
}

/// What a completed collab tool call reports.
struct CollabCall {
    call_id: String,
    tool: NativeCollabTool,
    status: NativeCollabCallStatus,
    receiver_thread_ids: Vec<String>,
    prompt: Option<String>,
    agents_states: BTreeMap<String, NativeCollabAgentState>,
}

/// The receivers a `sendInput` on `thread_id` hands input to: never the
/// sender itself.
fn send_receivers<'a>(
    thread_id: &'a str,
    receiver_thread_ids: &'a [String],
) -> impl Iterator<Item = &'a String> {
    receiver_thread_ids
        .iter()
        .filter(move |receiver| receiver.as_str() != thread_id)
}

/// A collab tool call starting on a followed thread. A `sendInput` to a
/// working child is about to hand its running turn more to do, so it is noted
/// against the child for the steer that turn will drain, which may reach Suru
/// before the call's own completion does. A send to a child with no stretch
/// open is left to its completion, which claims the turn it starts as a
/// resume; every other call's start projects nothing.
fn project_collab_call_started(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    call_id: &str,
    tool: NativeCollabTool,
    receiver_thread_ids: &[String],
    prompt: Option<&str>,
) {
    let (NativeCollabTool::SendInput, Some(sender), Some(prompt)) = (
        tool,
        correlation.spawner_attribution(thread_id),
        prompt.filter(|prompt| !prompt.trim().is_empty()),
    ) else {
        return;
    };
    for receiver in send_receivers(thread_id, receiver_thread_ids) {
        if let Some(child) = correlation.children.get_mut(receiver) {
            child.expect_send(call_id, &sender, prompt);
        }
    }
}

/// A collab tool call completing on a followed thread. A completed spawn opens
/// its receiver threads as Subagents, described by the prompt the call handed
/// them, which is also the Delegation each child Turn opens with. Whatever
/// the call was, the terminal lifecycle states it observed then settle the
/// stretches of the Subagents they name — which is how a wait learns of a
/// child finishing. A completed send comes last: to a receiver with no stretch
/// open — settled, just settled by the call's own report, or spawned before a
/// restart — it claims the native turn it starts as a resume, described by
/// the prompt's first line and opened by the prompt as its Delegation. To a
/// working receiver it is a steer, which stands only once the receiver
/// drains it (ADR 0032), so the completion changes nothing the parent shows
/// and only keeps the send noted for the steer to name its sender by. A send
/// that failed delivered nothing, and is forgotten. Because the settles come
/// first, a send whose report still carries its receiver's previous
/// `completed` can never settle the stretch that send resumes. A completed
/// `resumeAgent` only reloads a closed child, and begins nothing.
fn project_collab_call_completed(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    call: CollabCall,
) -> Vec<AttributedProviderEvent> {
    let CollabCall {
        call_id,
        tool,
        status,
        receiver_thread_ids,
        prompt,
        agents_states,
    } = call;
    let Some(spawner) = correlation.spawner_attribution(thread_id) else {
        return Vec::new();
    };
    let completed = status == NativeCollabCallStatus::Completed;
    let mut projected = Vec::new();
    if completed && tool == NativeCollabTool::SpawnAgent {
        let description = prompt.clone().unwrap_or_default();
        for receiver in &receiver_thread_ids {
            projected.extend(correlation.spawn_child(
                spawner.clone(),
                receiver.clone(),
                GENERIC_SUBAGENT_NAME.to_owned(),
                description.clone(),
                prompt.clone(),
            ));
        }
    }
    for (child_thread_id, state) in &agents_states {
        let settled = match state.status {
            NativeCollabAgentStatus::Completed | NativeCollabAgentStatus::Shutdown => {
                ProviderSubagentStatus::Completed
            }
            NativeCollabAgentStatus::Interrupted => ProviderSubagentStatus::Interrupted,
            NativeCollabAgentStatus::Errored | NativeCollabAgentStatus::NotFound => {
                ProviderSubagentStatus::Failed
            }
            NativeCollabAgentStatus::PendingInit
            | NativeCollabAgentStatus::Running
            | NativeCollabAgentStatus::Other => continue,
        };
        projected.extend(correlation.settle_child(child_thread_id, settled));
    }
    if !completed && tool == NativeCollabTool::SendInput {
        for receiver in send_receivers(thread_id, &receiver_thread_ids) {
            if let Some(child) = correlation.children.get_mut(receiver) {
                child.forget_send(&call_id);
            }
        }
    }
    if completed && tool == NativeCollabTool::SendInput {
        let prompt = prompt.filter(|prompt| !prompt.trim().is_empty());
        for receiver in send_receivers(thread_id, &receiver_thread_ids) {
            if let Some(child) = correlation.children.get_mut(receiver) {
                if let Some(prompt) = &prompt {
                    child.finish_send(&call_id, &spawner, prompt);
                }
                continue;
            }
            projected.extend(correlation.claim_resume(
                receiver,
                ResumeClaim {
                    delegator: spawner.clone(),
                    name: GENERIC_SUBAGENT_NAME.to_owned(),
                    description: prompt.as_deref().and_then(first_line).unwrap_or_default(),
                    delegation: prompt.clone(),
                },
            ));
        }
    }
    projected
}

/// Input a thread's agent received. On the Session's own thread that is the
/// user's Prompt, which Suru recorded when it sent it, so it projects nothing.
/// A child thread has no user, so there it is a Delegation: either the one its
/// stretch began with, or — arriving after that — a steer (ADR 0032). The one
/// the stretch began with already opens the stretch's Turn where the collab
/// call that began it said what it handed over; where that call was a V2
/// agent's activity, which never says, this item is the first word of it, and
/// is reported as the Delegation that began the stretch, from the delegating
/// Agent. A steer stands in the Turn the child is working in, at the point its
/// item arrived, as a Delegation from the Agent whose `sendInput` carried it;
/// it begins no Turn and adds nothing to any row. Input Suru handed the child
/// itself — a Subagent Report — is neither, and stands nowhere (ADR 0035).
/// Input on a child with no stretch open lands nowhere, as its work does.
fn project_user_message(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
    text: String,
) -> Vec<AttributedProviderEvent> {
    if thread_id == correlation.thread_id {
        return Vec::new();
    }
    let Some(child) = correlation.child_step(thread_id, turn_id) else {
        return Vec::new();
    };
    if let Some(handed) = child
        .handed_input
        .iter()
        .position(|handed| handed.trim() == text.trim())
    {
        // Input Suru handed the child itself — a Report — which stands in no
        // Transcript. Arriving, it has passed whatever the stretch opened
        // with, as any other work would.
        child.handed_input.remove(handed);
        child.opening = StretchOpening::Passed;
        return Vec::new();
    }
    let input = child.opening.receive(&text);
    if text.trim().is_empty() {
        return Vec::new();
    }
    let subagent_id = ProviderSubagentId::new(thread_id);
    let projected = match input {
        StretchInput::RecordedOpening => return Vec::new(),
        StretchInput::UnrecordedOpening => AttributedProviderEvent {
            attribution: child.stretch_delegator(),
            event: ProviderEvent::SubagentDelegated {
                subagent_id,
                delegation: text,
            },
        },
        StretchInput::Steer => AttributedProviderEvent {
            attribution: child.steer_sender(&text),
            event: ProviderEvent::SubagentSteered {
                subagent_id,
                delegation: text,
            },
        },
    };
    vec![projected]
}

/// The Subagent's name off Codex's agent path — the path's last segment, the
/// way `/root/auditor` names an auditor. The path is Codex's own logical
/// agent-tree address with a wire-defined `/` separator, not a filesystem
/// path, so splitting on `/` holds on every platform.
fn subagent_name_from_path(agent_path: &str) -> String {
    agent_path
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or(GENERIC_SUBAGENT_NAME)
        .to_owned()
}

fn project_reasoning_started(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
    item_id: String,
) -> Result<Vec<AttributedProviderEvent>, ProviderError> {
    let Some((thread, attribution)) = correlation.item_thread(thread_id, turn_id) else {
        return Ok(Vec::new());
    };
    if thread.active_reasoning.contains_key(&item_id) {
        return Err(codex_error(
            "Codex reused an active Reasoning item identity",
        ));
    }
    thread
        .active_reasoning
        .insert(item_id.clone(), ActiveNativeReasoning::new());
    Ok(attributed(
        &attribution,
        vec![ProviderEvent::ReasoningStarted {
            activity_id: reasoning_section_activity_id(&item_id, 0),
        }],
    ))
}

fn project_reasoning_delta(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
    item_id: String,
    delta: &str,
    summary_index: usize,
) -> Result<Vec<AttributedProviderEvent>, ProviderError> {
    let Some((thread, attribution)) = correlation.item_thread(thread_id, turn_id) else {
        return Ok(Vec::new());
    };
    let Some(reasoning) = thread.active_reasoning.get_mut(&item_id) else {
        return Ok(Vec::new());
    };
    let mut projected = open_reasoning_section(&item_id, reasoning, summary_index);
    if summary_index < reasoning.open_section {
        // The block this section streamed into settled when Codex moved past
        // it, and a settled block takes no more content.
        return Ok(attributed(&attribution, projected));
    }
    let segment = reasoning.push_delta(delta);
    projected.extend(reasoning_segment_events(&item_id, summary_index, segment));
    Ok(attributed(&attribution, projected))
}

/// Projects the break between two Reasoning summary sections as the settling of
/// the block the section streamed into and the start of the next one's, because
/// each section is a Reasoning Activity of its own, carrying at most the one
/// title its section led with and the duration the orchestrator stamps between
/// these two events. Codex announces the break that opens the first section
/// too, and that section is the block Codex already started, so a break naming
/// a section no later than the open one opens nothing.
fn project_reasoning_section_break(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
    item_id: String,
    summary_index: usize,
) -> Result<Vec<AttributedProviderEvent>, ProviderError> {
    let Some((thread, attribution)) = correlation.item_thread(thread_id, turn_id) else {
        return Ok(Vec::new());
    };
    let Some(reasoning) = thread.active_reasoning.get_mut(&item_id) else {
        return Ok(Vec::new());
    };
    Ok(attributed(
        &attribution,
        open_reasoning_section(&item_id, reasoning, summary_index),
    ))
}

/// Moves the item on to the summary section Codex is reporting on, settling the
/// block the section before it streamed into and starting the section's own.
/// Codex names the section on every break and every delta, so following the
/// name rather than counting breaks keeps Suru's blocks aligned with the
/// sections the completed item repeats, however many breaks Codex sends.
fn open_reasoning_section(
    item_id: &str,
    reasoning: &mut ActiveNativeReasoning,
    summary_index: usize,
) -> Vec<ProviderEvent> {
    if summary_index <= reasoning.open_section {
        return Vec::new();
    }
    let mut projected =
        settle_reasoning_section(item_id, reasoning.open_section, &mut reasoning.splitter);
    reasoning.begin_section(summary_index);
    projected.push(ProviderEvent::ReasoningStarted {
        activity_id: reasoning_section_activity_id(item_id, summary_index),
    });
    projected
}

fn project_reasoning_completed(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
    item_id: String,
    summary: Vec<String>,
) -> Result<Vec<AttributedProviderEvent>, ProviderError> {
    let Some((thread, attribution)) = correlation.item_thread(thread_id, turn_id) else {
        return Ok(Vec::new());
    };
    // Reasoning is the account of the work rather than the work, so nothing
    // about it fails a Turn: losing the Turn over a Reasoning summary would cost
    // the reader the answer it led to. A completion for a block Suru never saw
    // start has nowhere to land, and is dropped the way a stray delta is.
    let Some(mut reasoning) = thread.active_reasoning.remove(&item_id) else {
        return Ok(Vec::new());
    };
    // Codex repeats every section of the summary here, and only streams them
    // when the block was streaming to a client at all. The open section takes
    // whatever the stream had not already carried of it; when the two disagree,
    // the stream already showed the reader a coherent section, so repeating the
    // completed one on top of it would double the text rather than correct it.
    // Sections the stream settled earlier are left as the reader saw them.
    let open = reasoning.open_section;
    let mut projected = Vec::new();
    let remaining = summary
        .get(open)
        .and_then(|section| section.strip_prefix(reasoning.streamed.as_str()))
        .unwrap_or_default();
    if !remaining.is_empty() {
        projected.extend(reasoning_segment_events(
            &item_id,
            open,
            reasoning.splitter.push(remaining),
        ));
    }
    projected.extend(settle_reasoning_section(
        &item_id,
        open,
        &mut reasoning.splitter,
    ));
    // Sections the stream never reached open and settle here, in arrival order,
    // so a summary Codex only sent whole still splits the way a streamed one
    // does.
    for (section, text) in summary.iter().enumerate().skip(open + 1) {
        let mut splitter = ReasoningSummarySplitter::default();
        projected.push(ProviderEvent::ReasoningStarted {
            activity_id: reasoning_section_activity_id(&item_id, section),
        });
        projected.extend(reasoning_segment_events(
            &item_id,
            section,
            splitter.push(text),
        ));
        projected.extend(settle_reasoning_section(&item_id, section, &mut splitter));
    }
    Ok(attributed(&attribution, projected))
}

/// Names the Activity one summary section of a Reasoning item projects onto.
/// Codex identifies the item as a whole, so the section's place within it is
/// what tells one section's block from the next's.
fn reasoning_section_activity_id(item_id: &str, section: usize) -> ProviderActivityId {
    ProviderActivityId::new(format!("{item_id}#{section}"))
}

/// Settles the block one summary section streamed into, releasing whatever the
/// splitter still withholds before completing it, so no section's block is left
/// open for a Turn to settle around.
fn settle_reasoning_section(
    item_id: &str,
    section: usize,
    splitter: &mut ReasoningSummarySplitter,
) -> Vec<ProviderEvent> {
    let mut events = reasoning_segment_events(item_id, section, splitter.finish());
    events.push(ProviderEvent::ReasoningCompleted {
        activity_id: reasoning_section_activity_id(item_id, section),
    });
    events
}

/// Lowers one split step of a Reasoning summary section onto the Provider
/// events that carry it, dropping the step that resolved nothing because the
/// splitter is still withholding the head.
fn reasoning_segment_events(
    item_id: &str,
    section: usize,
    segment: ReasoningSegment,
) -> Vec<ProviderEvent> {
    let mut events = Vec::new();
    if let Some(title) = segment.title {
        events.push(ProviderEvent::ReasoningTitleChanged {
            activity_id: reasoning_section_activity_id(item_id, section),
            title,
        });
    }
    if !segment.content.is_empty() {
        events.push(ProviderEvent::ReasoningDelta {
            activity_id: reasoning_section_activity_id(item_id, section),
            content: segment.content,
        });
    }
    events
}

/// One cumulative reading landing on the Turn it belongs to, as the distance
/// that Turn has travelled since it opened.
///
/// On the Session's own thread the reading has to name the Turn Suru is
/// measuring. Any other reading — the total a reattach replays, or one
/// trailing a Turn that has already settled — belongs to nobody: it moves the
/// thread's running total, which is where the next Turn will start measuring
/// from, but never the baseline the Turn in progress is being measured
/// against. Turn identity decides that rather than arrival order, because a
/// reading queued before a Turn opened is drained after it.
///
/// A child thread's readings land on its open stretch — one Subagent Turn: the
/// spawn's counts the thread from its start, however many native turns it
/// takes, and a resume's counts from where the thread's total stood when it
/// first read. A settled child's readings belong to nobody, but they still
/// move the total the next resume measures from. The Cost is left for the
/// Session's rate lookup to fill in, because Codex states no dollar figure of
/// its own.
fn project_token_usage(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
    total: NativeCumulativeUsage,
    context_fill: Option<crate::protocol::ContextFill>,
) -> Vec<AttributedProviderEvent> {
    let context_fill = if correlation.thread_id != thread_id {
        let current = correlation
            .child_context_turns
            .get_mut(thread_id)
            .is_some_and(|turns| turns.observe(turn_id));
        context_fill.filter(|_| current)
    } else {
        context_fill
    };
    correlation.context_fill_sequence += 1;
    let context_event = context_fill.map(|fill| ProviderEvent::ContextFill {
        report: crate::provider::ContextFillReport {
            turn_id: if correlation.thread_id == thread_id {
                correlation.context_fill_turn
            } else {
                None
            },
            sequence: correlation.context_fill_sequence,
            fill,
        },
    });
    if correlation.thread_id == thread_id {
        if correlation.active_turn_id.as_deref() != Some(turn_id) {
            correlation.root_metering.observe(total);
            return if correlation.context_native_turn.as_deref() == Some(turn_id)
                && !correlation.turn_starting
            {
                owning(context_event.into_iter().collect())
            } else {
                Vec::new()
            };
        }
        let usage = correlation.root_metering.record(total, turn_id);
        return owning(
            std::iter::once(ProviderEvent::Usage { usage, cost: None })
                .chain(context_event)
                .collect(),
        );
    }
    let attribution = ProviderEventAttribution::Subagent(ProviderSubagentId::new(thread_id));
    let Some(child) = correlation.children.get_mut(thread_id) else {
        return match correlation.settled_children.get_mut(thread_id) {
            Some(child) => {
                child.metering.observe(total);
                attributed(&attribution, context_event.into_iter().collect())
            }
            None => Vec::new(),
        };
    };
    // A resumed stretch claims only readings of the native turns it runs in.
    // Any other — the total an attach replays against the turn it was reached
    // in, a settled stretch's straggler — says where the thread stands, which
    // is where the stretch measures from until its own first reading.
    if child.resumed_turn.is_some() && !child.stretch_turns.contains(turn_id) {
        child.metering.observe(total);
        return attributed(&attribution, context_event.into_iter().collect());
    }
    let usage = child.metering.record(total, &child.stretch_key());
    attributed(
        &attribution,
        std::iter::once(ProviderEvent::Usage { usage, cost: None })
            .chain(context_event)
            .collect(),
    )
}

fn project_agent_selection_changed(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    model: String,
    effort: NativeField<Option<String>>,
    service_tier: NativeField<Option<String>>,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    if correlation.thread_id != thread_id || correlation.active_turn_id.is_none() {
        return Ok(Vec::new());
    }
    if model.is_empty() {
        return Err(codex_error(
            "Codex reported an empty effective Model for the active Turn",
        ));
    }
    let Some(requested) = correlation.effective_selection.as_ref() else {
        return Err(codex_error(
            "Codex reported effective settings before accepting the active Turn",
        ));
    };
    let mut effective = requested.clone();
    effective.model = ModelId::new(model);
    apply_effective_select_option(
        &mut effective,
        REASONING_EFFORT_OPTION_ID,
        effort,
        NativeClearMapping::RemoveOption,
    )?;
    apply_effective_select_option(
        &mut effective,
        SERVICE_TIER_OPTION_ID,
        service_tier,
        NativeClearMapping::Select(DEFAULT_SERVICE_TIER_CHOICE_ID),
    )?;
    if effective == *requested {
        return Ok(Vec::new());
    }
    correlation.metered_model = effective.model.clone();
    correlation.effective_selection = Some(effective.clone());
    Ok(vec![ProviderEvent::AgentSelectionChanged {
        selection: effective,
    }])
}

fn project_agent_message_started(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
    item_id: String,
) -> Result<Vec<AttributedProviderEvent>, ProviderError> {
    let Some((thread, attribution)) = correlation.item_thread(thread_id, turn_id) else {
        return Ok(Vec::new());
    };
    if thread.active_agent_message.is_some() {
        return Err(codex_error(
            "Codex started a second Agent Message before completing the first",
        ));
    }
    thread.active_agent_message = Some(ActiveNativeAgentMessage {
        item_id,
        streamed_text: String::new(),
    });
    Ok(attributed(
        &attribution,
        vec![ProviderEvent::AgentMessageStarted],
    ))
}

fn project_agent_message_delta(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
    item_id: &str,
    delta: String,
) -> Result<Vec<AttributedProviderEvent>, ProviderError> {
    let Some((thread, attribution)) = correlation.item_thread(thread_id, turn_id) else {
        return Ok(Vec::new());
    };
    let Some(message) = thread.active_agent_message.as_mut() else {
        return Err(codex_error(
            "Codex sent Agent Message content before starting the Message",
        ));
    };
    if message.item_id != item_id {
        return Ok(Vec::new());
    }
    message.streamed_text.push_str(&delta);
    Ok(attributed(
        &attribution,
        vec![ProviderEvent::AgentMessageDelta { content: delta }],
    ))
}

fn project_agent_message_completed(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
    item_id: &str,
    text: &str,
) -> Result<Vec<AttributedProviderEvent>, ProviderError> {
    let Some((thread, attribution)) = correlation.item_thread(thread_id, turn_id) else {
        return Ok(Vec::new());
    };
    let Some(message) = thread.active_agent_message.as_ref() else {
        return Err(codex_error(
            "Codex completed an Agent Message before starting it",
        ));
    };
    if message.item_id != item_id {
        return Ok(Vec::new());
    }
    let Some(remaining) = text.strip_prefix(&message.streamed_text) else {
        return Err(codex_error(
            "Codex completed an Agent Message with content that did not match its stream",
        ));
    };
    let mut projected = Vec::with_capacity(if remaining.is_empty() { 1 } else { 2 });
    if !remaining.is_empty() {
        projected.push(ProviderEvent::AgentMessageDelta {
            content: remaining.to_owned(),
        });
    }
    projected.push(ProviderEvent::AgentMessageCompleted);
    thread.active_agent_message = None;
    Ok(attributed(&attribution, projected))
}

fn project_command_started(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
    item_id: String,
    command: String,
    cwd: Option<PathBuf>,
    status: NativeCommandStatus,
) -> Result<Vec<AttributedProviderEvent>, ProviderError> {
    let Some((thread, attribution)) = correlation.item_thread(thread_id, turn_id) else {
        return Ok(Vec::new());
    };
    if !matches!(status, NativeCommandStatus::InProgress) {
        return Err(codex_error(
            "Codex started a command outside its active state",
        ));
    }
    if thread.active_commands.contains_key(&item_id) {
        return Err(codex_error("Codex reused an active command item identity"));
    }
    thread.active_commands.insert(
        item_id.clone(),
        ActiveNativeCommand {
            streamed_output: String::new(),
        },
    );
    Ok(attributed(
        &attribution,
        vec![ProviderEvent::CommandStarted {
            activity_id: ProviderActivityId::new(item_id),
            command: strip_launcher_wrapper(command),
            cwd,
        }],
    ))
}

fn project_command_output_delta(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
    item_id: String,
    delta: String,
) -> Result<Vec<AttributedProviderEvent>, ProviderError> {
    let Some((thread, attribution)) = correlation.item_thread(thread_id, turn_id) else {
        return Ok(Vec::new());
    };
    let Some(command) = thread.active_commands.get_mut(&item_id) else {
        return Ok(Vec::new());
    };
    command.streamed_output.push_str(&delta);
    Ok(attributed(
        &attribution,
        vec![ProviderEvent::CommandOutputDelta {
            activity_id: ProviderActivityId::new(item_id),
            content: delta,
        }],
    ))
}

fn project_command_completed(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
    item_id: String,
    aggregated_output: Option<String>,
    exit_status: Option<i32>,
    status: NativeCommandStatus,
) -> Result<Vec<AttributedProviderEvent>, ProviderError> {
    let Some((thread, attribution)) = correlation.item_thread(thread_id, turn_id) else {
        return Ok(Vec::new());
    };
    let Some(command) = thread.active_commands.get(&item_id) else {
        return Err(codex_error(
            "Codex completed a command before starting the Activity",
        ));
    };
    // The live deltas are the account of record: Codex emits them chronologically
    // across stdout and stderr, while the completed item's aggregate concatenates
    // the two streams and caps each independently. The aggregate extends the
    // stream only when deltas stopped carrying output, so it settles what the
    // stream never said and otherwise leaves the stream alone.
    let remaining = aggregated_output
        .as_deref()
        .and_then(|output| output.strip_prefix(command.streamed_output.as_str()))
        .unwrap_or_default()
        .to_owned();
    let status = match status {
        NativeCommandStatus::Completed => ProviderCommandStatus::Completed,
        NativeCommandStatus::Failed | NativeCommandStatus::Declined => {
            ProviderCommandStatus::Failed
        }
        NativeCommandStatus::InProgress => {
            return Err(codex_error(
                "Codex completed a command while it was still active",
            ));
        }
    };
    thread.active_commands.remove(&item_id);
    let activity_id = ProviderActivityId::new(item_id);
    let mut projected = Vec::with_capacity(if remaining.is_empty() { 1 } else { 2 });
    if !remaining.is_empty() {
        projected.push(ProviderEvent::CommandOutputDelta {
            activity_id: activity_id.clone(),
            content: remaining,
        });
    }
    projected.push(ProviderEvent::CommandCompleted {
        activity_id,
        status,
        exit_status,
    });
    Ok(attributed(&attribution, projected))
}

fn project_file_change_started(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
    item_id: String,
    changes: Vec<NativeFileChange>,
    status: NativeFileChangeStatus,
) -> Result<Vec<AttributedProviderEvent>, ProviderError> {
    let Some((thread, attribution)) = correlation.item_thread(thread_id, turn_id) else {
        return Ok(Vec::new());
    };
    if !matches!(status, NativeFileChangeStatus::InProgress) {
        return Err(codex_error(
            "Codex started file changes outside their active state",
        ));
    }
    if thread.active_commands.contains_key(&item_id)
        || thread.active_file_changes.contains_key(&item_id)
    {
        return Err(codex_error(
            "Codex reused an active file-change item identity",
        ));
    }
    let changes = changes
        .into_iter()
        .map(FileChange::from)
        .collect::<Vec<_>>();
    thread.active_file_changes.insert(
        item_id.clone(),
        ActiveNativeFileChange {
            changes: changes.clone(),
        },
    );
    Ok(attributed(
        &attribution,
        vec![ProviderEvent::FileChangeStarted {
            activity_id: ProviderActivityId::new(item_id),
            changes,
        }],
    ))
}

fn project_file_change_updated(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
    item_id: String,
    changes: Vec<NativeFileChange>,
) -> Result<Vec<AttributedProviderEvent>, ProviderError> {
    let Some((thread, attribution)) = correlation.item_thread(thread_id, turn_id) else {
        return Ok(Vec::new());
    };
    let Some(file_change) = thread.active_file_changes.get_mut(&item_id) else {
        return Ok(Vec::new());
    };
    let changes = changes
        .into_iter()
        .map(FileChange::from)
        .collect::<Vec<_>>();
    file_change.changes.clone_from(&changes);
    Ok(attributed(
        &attribution,
        vec![ProviderEvent::FileChangeUpdated {
            activity_id: ProviderActivityId::new(item_id),
            changes,
        }],
    ))
}

fn project_file_change_completed(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
    item_id: String,
    changes: Vec<NativeFileChange>,
    status: NativeFileChangeStatus,
) -> Result<Vec<AttributedProviderEvent>, ProviderError> {
    let Some((thread, attribution)) = correlation.item_thread(thread_id, turn_id) else {
        return Ok(Vec::new());
    };
    let Some(file_change) = thread.active_file_changes.get(&item_id) else {
        return Err(codex_error(
            "Codex completed file changes before starting the Activity",
        ));
    };
    let changes = changes
        .into_iter()
        .map(FileChange::from)
        .collect::<Vec<_>>();
    let changes_changed = file_change.changes != changes;
    let status = match status {
        NativeFileChangeStatus::Completed => ProviderFileChangeStatus::Completed,
        NativeFileChangeStatus::Failed | NativeFileChangeStatus::Declined => {
            ProviderFileChangeStatus::Failed
        }
        NativeFileChangeStatus::InProgress => {
            return Err(codex_error(
                "Codex completed file changes while they were still active",
            ));
        }
    };
    thread.active_file_changes.remove(&item_id);
    let activity_id = ProviderActivityId::new(item_id);
    let mut projected = Vec::with_capacity(if changes_changed { 2 } else { 1 });
    if changes_changed {
        projected.push(ProviderEvent::FileChangeUpdated {
            activity_id: activity_id.clone(),
            changes,
        });
    }
    projected.push(ProviderEvent::FileChangeCompleted {
        activity_id,
        status,
    });
    Ok(attributed(&attribution, projected))
}

/// The Tool Call a Tool use is recorded as, its input redacted once rendered. The redactor has
/// already read the item's own text, but rendering spells the arguments anew — a number, a key,
/// the joins between them — so a secret an argument spells in any of those ways is caught only
/// in the rendered input, as it is in a Command's text.
fn presented_tool_call(
    questionnaires: &super::questionnaire::CodexQuestionnaires,
    tool: &NativeToolUse,
) -> PresentedToolCall {
    let mut tool_call = tool.tool_call();
    tool_call.input = tool_call
        .input
        .map(|input| questionnaires.redact_text(&input));
    tool_call
}

/// How a Tool Call settles, its output redacted once rendered for the same reason its input is:
/// the output joins what the item carried — a result's text beside a call's error, a failure's
/// reason beside its message — anew.
fn settled_tool_call(
    questionnaires: &super::questionnaire::CodexQuestionnaires,
    tool: &NativeToolUse,
) -> ToolCallOutcome {
    let mut outcome = tool.outcome();
    outcome.output = questionnaires.redact_text(&outcome.output);
    outcome
}

/// Opens the Tool Call a Tool use is recorded as, with whatever input its item already carries.
/// A start repeating one still open opens nothing further.
fn project_tool_use_started(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
    tool: &NativeToolUse,
) -> Vec<AttributedProviderEvent> {
    let tool_call = presented_tool_call(&correlation.questionnaires, tool);
    let Some((thread, attribution)) = correlation.item_thread(thread_id, turn_id) else {
        return Vec::new();
    };
    if thread.active_tool_calls.contains_key(tool.item_id()) {
        return Vec::new();
    }
    thread.active_tool_calls.insert(
        tool.item_id().to_owned(),
        ActiveNativeToolCall {
            input: tool_call.input.clone(),
        },
    );
    attributed(
        &attribution,
        vec![ProviderEvent::ToolCallStarted {
            activity_id: ProviderActivityId::new(tool.item_id()),
            name: tool_call.name,
            server: tool_call.server,
            input: tool_call.input,
        }],
    )
}

/// Settles a Tool Call from the completed item of its use: the input, where it differs from what
/// the Tool Call opened with — as a web search's does, which Codex starts before knowing what it
/// searches — then the whole output, which Codex never streams, then the outcome. A use Suru never
/// saw start opens and settles at once, rather than going unseen.
fn project_tool_use_completed(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
    tool: &NativeToolUse,
) -> Vec<AttributedProviderEvent> {
    let tool_call = presented_tool_call(&correlation.questionnaires, tool);
    let outcome = settled_tool_call(&correlation.questionnaires, tool);
    let Some((thread, attribution)) = correlation.item_thread(thread_id, turn_id) else {
        return Vec::new();
    };
    let activity_id = ProviderActivityId::new(tool.item_id());
    let mut projected = Vec::with_capacity(3);
    match thread.active_tool_calls.remove(tool.item_id()) {
        Some(opened) => {
            if let Some(input) = tool_call
                .input
                .filter(|input| opened.input.as_ref() != Some(input))
            {
                projected.push(ProviderEvent::ToolCallInputKnown {
                    activity_id: activity_id.clone(),
                    input,
                });
            }
        }
        None => projected.push(ProviderEvent::ToolCallStarted {
            activity_id: activity_id.clone(),
            name: tool_call.name,
            server: tool_call.server,
            input: tool_call.input,
        }),
    }
    if !outcome.output.is_empty() {
        projected.push(ProviderEvent::ToolCallOutputDelta {
            activity_id: activity_id.clone(),
            content: outcome.output,
        });
    }
    projected.push(ProviderEvent::ToolCallCompleted {
        activity_id,
        status: outcome.status,
        omitted_parts: outcome.omitted_parts,
    });
    attributed(&attribution, projected)
}

fn project_turn_completed(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    turn_id: &str,
    outcome: NativeTurnOutcome,
    final_agent_message: Option<super::wire::CompletedNativeAgentMessage>,
) -> Result<Vec<ProviderEvent>, ProviderError> {
    if !correlation.is_active_turn(thread_id, turn_id) {
        return Ok(Vec::new());
    }
    let mut events = Vec::new();
    if matches!(&outcome, NativeTurnOutcome::Completed)
        && correlation.root.active_agent_message.is_some()
        && let Some(final_agent_message) = final_agent_message
    {
        events.extend(
            project_agent_message_completed(
                correlation,
                thread_id,
                turn_id,
                &final_agent_message.item_id,
                &final_agent_message.text,
            )?
            .into_iter()
            .map(|attributed| attributed.event),
        );
    }
    let selection_rejected = match (&outcome, &correlation.effective_selection) {
        (NativeTurnOutcome::Failed { message, kind }, Some(selection)) => {
            is_native_selection_rejection(message, kind, selection)
        }
        _ => false,
    };
    correlation.settle_turn();
    events.push(match outcome {
        NativeTurnOutcome::Completed => ProviderEvent::TurnCompleted,
        NativeTurnOutcome::Interrupted => ProviderEvent::TurnInterrupted,
        NativeTurnOutcome::Failed { message, .. } if selection_rejected => {
            ProviderEvent::AgentSelectionRejected { message }
        }
        NativeTurnOutcome::Failed { message, .. } => ProviderEvent::TurnFailed { message },
    });
    Ok(events)
}

/// Decides whether a failed native Turn was Codex refusing the Agent Selection it was given.
fn is_native_selection_rejection(
    message: &str,
    kind: &NativeTurnFailureKind,
    selection: &AgentSelection,
) -> bool {
    match kind {
        NativeTurnFailureKind::BadRequest { additional_details } => additional_details
            .as_deref()
            .and_then(|details| serde_json::from_str::<Value>(details).ok())
            .is_some_and(|details| json_identifies_agent_selection_parameter(&details)),
        NativeTurnFailureKind::Other => {
            let message = message.to_ascii_lowercase();
            let selected_model = selection.model.as_str().to_ascii_lowercase();
            let rejected = [
                "unavailable",
                "unsupported",
                "not available",
                "not found",
                "does not exist",
                "unknown",
                "invalid",
                "access",
                "denied",
                "retired",
            ]
            .iter()
            .any(|reason| message.contains(reason));
            let identifies_model = message.contains("model") && message.contains(&selected_model);
            let identifies_option = selection.options.iter().any(|option| {
                let option_id = option.id.as_str().to_ascii_lowercase();
                let option_label = option_id.replace('_', " ");
                message.contains(&option_id) || message.contains(&option_label)
            });
            rejected && (identifies_model || identifies_option)
        }
    }
}

fn json_identifies_agent_selection_parameter(value: &Value) -> bool {
    match value {
        Value::Object(fields) => {
            fields
                .get("param")
                .and_then(Value::as_str)
                .is_some_and(|parameter| {
                    matches!(
                        parameter,
                        "model"
                            | "effort"
                            | "reasoningEffort"
                            | "reasoning_effort"
                            | "serviceTier"
                            | "service_tier"
                    )
                })
                || fields
                    .values()
                    .any(json_identifies_agent_selection_parameter)
        }
        Value::Array(values) => values.iter().any(json_identifies_agent_selection_parameter),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => false,
    }
}

fn apply_effective_select_option(
    selection: &mut AgentSelection,
    option_id: &str,
    field: NativeField<Option<String>>,
    clear: NativeClearMapping,
) -> Result<(), ProviderError> {
    let NativeField::Present(value) = field else {
        return Ok(());
    };
    let choice = match (value, clear) {
        (Some(choice), _) => choice,
        (None, NativeClearMapping::Select(choice)) => choice.to_owned(),
        (None, NativeClearMapping::RemoveOption) => {
            selection
                .options
                .retain(|option| option.id.as_str() != option_id);
            return Ok(());
        }
    };
    if choice.is_empty() {
        return Err(codex_error(format!(
            "Codex reported an empty effective value for Model Option `{option_id}`"
        )));
    }
    let value = ModelOptionValue::Select {
        choice: ModelOptionChoiceId::new(choice),
    };
    if let Some(option) = selection
        .options
        .iter_mut()
        .find(|option| option.id.as_str() == option_id)
    {
        option.value = value;
    } else {
        selection.options.push(ModelOptionSelection {
            id: ModelOptionId::new(option_id),
            value,
        });
    }
    Ok(())
}

/// How a cleared native Model Option maps back onto a Suru Agent Selection.
enum NativeClearMapping {
    RemoveOption,
    Select(&'static str),
}

#[cfg(test)]
mod tests {
    use crate::protocol::{
        AgentSelection, ModelId, ModelOptionChoiceId, ModelOptionId, ModelOptionSelection,
        ModelOptionValue, ProviderId,
    };

    use crate::provider::{
        AttributedProviderEvent, ProviderCommandStatus, ProviderSubagentId, ProviderSubagentStatus,
        ProviderToolCallStatus,
    };
    use serde_json::json;

    use super::super::wire::tool_use_item;

    use super::{
        NativeCollabAgentState, NativeCollabAgentStatus, NativeCollabCallStatus, NativeCollabTool,
        NativeCommandStatus, NativeCorrelation, NativeNotification, NativeSubagentActivityKind,
        NativeTurnFailureKind, NativeTurnOutcome, ProviderActivityId, ProviderEvent,
        ProviderEventAttribution, is_native_selection_rejection, project_native_notification,
    };

    const THREAD: &str = "thread-fixture";
    const TURN: &str = "turn-fixture";
    const ITEM: &str = "item-fixture";

    fn reasoning_turn() -> NativeCorrelation {
        let mut correlation = NativeCorrelation::new(
            THREAD.to_owned(),
            ModelId::new("gpt-fixture"),
            Default::default(),
            Default::default(),
        );
        correlation.begin_turn_start().expect("claim the Turn slot");
        correlation
            .finish_turn_start(
                Ok(TURN.to_owned()),
                AgentSelection {
                    provider: ProviderId::new("codex"),
                    model: ModelId::new("gpt-fixture"),
                    options: Vec::new(),
                },
            )
            .expect("install the native Turn");
        correlation
    }

    fn project(
        correlation: &mut NativeCorrelation,
        notification: NativeNotification,
    ) -> Vec<ProviderEvent> {
        project_native_notification(correlation, notification)
            .expect("project the notification")
            .into_iter()
            .map(|attributed| {
                assert_eq!(
                    attributed.attribution,
                    ProviderEventAttribution::OwningSession,
                    "the root turn's events ride the owning Session"
                );
                attributed.event
            })
            .collect()
    }

    fn started() -> NativeNotification {
        NativeNotification::ReasoningStarted {
            thread_id: THREAD.to_owned(),
            turn_id: TURN.to_owned(),
            item_id: ITEM.to_owned(),
        }
    }

    fn delta(section: usize, delta: &str) -> NativeNotification {
        NativeNotification::ReasoningDelta {
            thread_id: THREAD.to_owned(),
            turn_id: TURN.to_owned(),
            item_id: ITEM.to_owned(),
            delta: delta.to_owned(),
            summary_index: section,
        }
    }

    fn section_break(section: usize) -> NativeNotification {
        NativeNotification::ReasoningSectionBreak {
            thread_id: THREAD.to_owned(),
            turn_id: TURN.to_owned(),
            item_id: ITEM.to_owned(),
            summary_index: section,
        }
    }

    fn completed(summary: &[&str]) -> NativeNotification {
        NativeNotification::ReasoningCompleted {
            thread_id: THREAD.to_owned(),
            turn_id: TURN.to_owned(),
            item_id: ITEM.to_owned(),
            summary: summary
                .iter()
                .map(|section| (*section).to_owned())
                .collect(),
        }
    }

    fn section_activity_id(section: usize) -> ProviderActivityId {
        ProviderActivityId::new(format!("{ITEM}#{section}"))
    }

    fn started_section(section: usize) -> ProviderEvent {
        ProviderEvent::ReasoningStarted {
            activity_id: section_activity_id(section),
        }
    }

    fn titled_section(section: usize, title: &str) -> ProviderEvent {
        ProviderEvent::ReasoningTitleChanged {
            activity_id: section_activity_id(section),
            title: title.to_owned(),
        }
    }

    fn section_content(section: usize, content: &str) -> ProviderEvent {
        ProviderEvent::ReasoningDelta {
            activity_id: section_activity_id(section),
            content: content.to_owned(),
        }
    }

    fn completed_section(section: usize) -> ProviderEvent {
        ProviderEvent::ReasoningCompleted {
            activity_id: section_activity_id(section),
        }
    }

    fn command_started(command: &str) -> NativeNotification {
        NativeNotification::CommandStarted {
            thread_id: THREAD.to_owned(),
            turn_id: TURN.to_owned(),
            item_id: ITEM.to_owned(),
            command: command.to_owned(),
            cwd: None,
            status: NativeCommandStatus::InProgress,
        }
    }

    fn recorded_command(events: Vec<ProviderEvent>) -> String {
        match events.into_iter().next() {
            Some(ProviderEvent::CommandStarted { command, .. }) => command,
            other => panic!("expected a started command, got {other:?}"),
        }
    }

    fn tool_use_started(item: serde_json::Value) -> NativeNotification {
        NativeNotification::ToolUseStarted {
            thread_id: THREAD.to_owned(),
            turn_id: TURN.to_owned(),
            tool: tool_use_item(item),
        }
    }

    fn tool_use_completed(item: serde_json::Value) -> NativeNotification {
        NativeNotification::ToolUseCompleted {
            thread_id: THREAD.to_owned(),
            turn_id: TURN.to_owned(),
            tool: tool_use_item(item),
        }
    }

    fn web_search(query: &str) -> serde_json::Value {
        json!({
            "type": "webSearch",
            "id": ITEM,
            "query": query,
            "action": (!query.is_empty()).then(|| json!({"type": "search", "query": query})),
        })
    }

    fn mcp_call(status: &str, result: serde_json::Value) -> serde_json::Value {
        json!({
            "type": "mcpToolCall", "id": ITEM, "server": "github", "tool": "create_issue",
            "status": status, "arguments": {"title": "Fix"}, "result": result, "error": null,
        })
    }

    #[test]
    fn a_web_search_opens_with_no_input_and_its_completion_names_the_search() {
        let mut correlation = reasoning_turn();
        assert_eq!(
            project(&mut correlation, tool_use_started(web_search(""))),
            [ProviderEvent::ToolCallStarted {
                activity_id: ProviderActivityId::new(ITEM),
                name: "web_search".to_owned(),
                server: None,
                input: None,
            }]
        );
        assert_eq!(
            project(&mut correlation, tool_use_completed(web_search("rust"))),
            [
                ProviderEvent::ToolCallInputKnown {
                    activity_id: ProviderActivityId::new(ITEM),
                    input: "query=rust".to_owned(),
                },
                ProviderEvent::ToolCallCompleted {
                    activity_id: ProviderActivityId::new(ITEM),
                    status: ProviderToolCallStatus::Completed,
                    omitted_parts: 0,
                },
            ],
            "the search settles with no output"
        );
    }

    #[test]
    fn an_mcp_call_settles_with_its_whole_output_and_the_input_it_opened_with_stands() {
        let mut correlation = reasoning_turn();
        assert_eq!(
            project(
                &mut correlation,
                tool_use_started(mcp_call("inProgress", serde_json::Value::Null))
            ),
            [ProviderEvent::ToolCallStarted {
                activity_id: ProviderActivityId::new(ITEM),
                name: "create_issue".to_owned(),
                server: Some("github".to_owned()),
                input: Some("title=Fix".to_owned()),
            }]
        );
        assert_eq!(
            project(
                &mut correlation,
                tool_use_completed(mcp_call(
                    "completed",
                    json!({"content": [
                        {"type": "text", "text": "Created #7"},
                        {"type": "image", "data": "AA=="},
                    ]})
                ))
            ),
            [
                ProviderEvent::ToolCallOutputDelta {
                    activity_id: ProviderActivityId::new(ITEM),
                    content: "Created #7".to_owned(),
                },
                ProviderEvent::ToolCallCompleted {
                    activity_id: ProviderActivityId::new(ITEM),
                    status: ProviderToolCallStatus::Completed,
                    omitted_parts: 1,
                },
            ]
        );
    }

    #[test]
    fn a_tool_use_never_seen_to_start_opens_and_settles_at_once() {
        let mut correlation = reasoning_turn();
        assert_eq!(
            project(
                &mut correlation,
                tool_use_completed(json!({"type": "imageView", "id": ITEM, "path": "chart.png"}))
            ),
            [
                ProviderEvent::ToolCallStarted {
                    activity_id: ProviderActivityId::new(ITEM),
                    name: "view_image".to_owned(),
                    server: None,
                    input: Some("path=chart.png".to_owned()),
                },
                ProviderEvent::ToolCallCompleted {
                    activity_id: ProviderActivityId::new(ITEM),
                    status: ProviderToolCallStatus::Completed,
                    omitted_parts: 0,
                },
            ]
        );
    }

    #[test]
    fn a_start_repeating_an_open_tool_call_opens_nothing_further() {
        let mut correlation = reasoning_turn();
        assert_eq!(
            project(&mut correlation, tool_use_started(web_search(""))).len(),
            1
        );
        assert!(project(&mut correlation, tool_use_started(web_search(""))).is_empty());
    }

    #[test]
    fn a_tool_use_from_a_turn_suru_no_longer_tracks_is_dropped() {
        let mut correlation = reasoning_turn();
        for notification in [
            NativeNotification::ToolUseStarted {
                thread_id: THREAD.to_owned(),
                turn_id: "stale-turn".to_owned(),
                tool: tool_use_item(web_search("")),
            },
            NativeNotification::ToolUseCompleted {
                thread_id: "unfollowed-thread".to_owned(),
                turn_id: TURN.to_owned(),
                tool: tool_use_item(web_search("rust")),
            },
        ] {
            assert!(project(&mut correlation, notification).is_empty());
        }
    }

    #[test]
    fn a_capped_final_command_aggregate_settles_the_fuller_live_stream() {
        let mut correlation = reasoning_turn();
        project(&mut correlation, command_started("find the answer"));
        project(
            &mut correlation,
            NativeNotification::CommandOutputDelta {
                thread_id: THREAD.to_owned(),
                turn_id: TURN.to_owned(),
                item_id: ITEM.to_owned(),
                delta: "retained prefix\nstreamed beyond the final cap\n".to_owned(),
            },
        );

        assert_eq!(
            project(
                &mut correlation,
                NativeNotification::CommandCompleted {
                    thread_id: THREAD.to_owned(),
                    turn_id: TURN.to_owned(),
                    item_id: ITEM.to_owned(),
                    aggregated_output: Some("retained prefix\n".to_owned()),
                    exit_status: Some(0),
                    status: NativeCommandStatus::Completed,
                },
            ),
            vec![ProviderEvent::CommandCompleted {
                activity_id: ProviderActivityId::new(ITEM),
                status: ProviderCommandStatus::Completed,
                exit_status: Some(0),
            }]
        );
    }

    /// Codex streams a command's deltas chronologically across stdout and
    /// stderr but aggregates the completed item as stdout followed by stderr.
    /// A command that interleaves the two ends with an aggregate the stream
    /// can neither extend nor be extended by, and the Turn must survive it.
    #[test]
    fn an_out_of_order_final_command_aggregate_settles_the_live_stream() {
        let mut correlation = reasoning_turn();
        project(&mut correlation, command_started("cargo test"));
        for delta in [
            "   Compiling suru\n",
            "running 1 test\n",
            "error: test failed\n",
        ] {
            project(
                &mut correlation,
                NativeNotification::CommandOutputDelta {
                    thread_id: THREAD.to_owned(),
                    turn_id: TURN.to_owned(),
                    item_id: ITEM.to_owned(),
                    delta: delta.to_owned(),
                },
            );
        }

        assert_eq!(
            project(
                &mut correlation,
                NativeNotification::CommandCompleted {
                    thread_id: THREAD.to_owned(),
                    turn_id: TURN.to_owned(),
                    item_id: ITEM.to_owned(),
                    aggregated_output: Some(
                        "running 1 test\n   Compiling suru\nerror: test failed\n".to_owned(),
                    ),
                    exit_status: Some(101),
                    status: NativeCommandStatus::Failed,
                },
            ),
            vec![ProviderEvent::CommandCompleted {
                activity_id: ProviderActivityId::new(ITEM),
                status: ProviderCommandStatus::Failed,
                exit_status: Some(101),
            }]
        );
    }

    /// Nothing streamed leaves the completed item's aggregate as the only
    /// account of the command's output, so it must still reach the transcript.
    #[test]
    fn an_unstreamed_command_takes_its_output_from_the_final_aggregate() {
        let mut correlation = reasoning_turn();
        project(&mut correlation, command_started("find the answer"));

        assert_eq!(
            project(
                &mut correlation,
                NativeNotification::CommandCompleted {
                    thread_id: THREAD.to_owned(),
                    turn_id: TURN.to_owned(),
                    item_id: ITEM.to_owned(),
                    aggregated_output: Some("the whole answer\n".to_owned()),
                    exit_status: Some(0),
                    status: NativeCommandStatus::Completed,
                },
            ),
            vec![
                ProviderEvent::CommandOutputDelta {
                    activity_id: ProviderActivityId::new(ITEM),
                    content: "the whole answer\n".to_owned(),
                },
                ProviderEvent::CommandCompleted {
                    activity_id: ProviderActivityId::new(ITEM),
                    status: ProviderCommandStatus::Completed,
                    exit_status: Some(0),
                },
            ]
        );
    }

    #[test]
    fn a_powershell_launcher_wrapper_is_stripped_from_the_recorded_command() {
        let mut correlation = reasoning_turn();
        let events = project(
            &mut correlation,
            command_started("pwsh -NoProfile -Command 'Get-ChildItem -Recurse'"),
        );
        assert_eq!(recorded_command(events), "Get-ChildItem -Recurse");
    }

    #[test]
    fn a_powershell_executable_path_and_lowercased_flags_still_strip() {
        let mut correlation = reasoning_turn();
        let events = project(
            &mut correlation,
            command_started("powershell.exe -nologo -command 'Write-Host hi'"),
        );
        assert_eq!(recorded_command(events), "Write-Host hi");
    }

    #[test]
    fn a_bare_sh_dash_c_wrapper_is_stripped_from_the_recorded_command() {
        let mut correlation = reasoning_turn();
        let events = project(&mut correlation, command_started("sh -c ls"));
        assert_eq!(recorded_command(events), "ls");
    }

    #[test]
    fn a_command_that_is_not_launcher_plumbing_is_recorded_verbatim() {
        let mut correlation = reasoning_turn();
        let events = project(
            &mut correlation,
            command_started("git -c core.pager=cat log -1"),
        );
        assert_eq!(recorded_command(events), "git -c core.pager=cat log -1");
    }

    #[test]
    fn a_shell_run_with_more_than_a_wrapped_script_is_recorded_verbatim() {
        let mut correlation = reasoning_turn();
        let events = project(
            &mut correlation,
            command_started("bash -lc 'echo hi' trailing"),
        );
        assert_eq!(recorded_command(events), "bash -lc 'echo hi' trailing");
    }

    #[test]
    fn a_powershell_run_with_a_flag_codex_never_passes_is_recorded_verbatim() {
        let mut correlation = reasoning_turn();
        let events = project(
            &mut correlation,
            command_started("pwsh -ExecutionPolicy Bypass -Command 'Write-Host hi'"),
        );
        assert_eq!(
            recorded_command(events),
            "pwsh -ExecutionPolicy Bypass -Command 'Write-Host hi'"
        );
    }

    #[test]
    fn quoting_that_does_not_split_back_to_an_argv_is_recorded_verbatim() {
        let mut correlation = reasoning_turn();
        let events = project(&mut correlation, command_started("zsh -lc 'unbalanced"));
        assert_eq!(recorded_command(events), "zsh -lc 'unbalanced");
    }

    #[test]
    fn a_posix_shell_launcher_wrapper_is_stripped_from_the_recorded_command() {
        let mut correlation = reasoning_turn();
        let events = project(
            &mut correlation,
            command_started("/usr/bin/zsh -lc 'cargo test'"),
        );
        assert_eq!(recorded_command(events), "cargo test");
    }

    #[test]
    fn a_streamed_reasoning_summary_becomes_a_titled_block_and_its_body() {
        let mut correlation = reasoning_turn();

        assert_eq!(
            project(&mut correlation, started()),
            vec![started_section(0)]
        );
        assert_eq!(
            project(&mut correlation, delta(0, "**Inspecting the")),
            Vec::new()
        );
        assert_eq!(
            project(&mut correlation, delta(0, " seam**\n\nReading it.")),
            vec![
                titled_section(0, "Inspecting the seam"),
                section_content(0, "Reading it."),
            ]
        );
        assert_eq!(
            project(
                &mut correlation,
                completed(&["**Inspecting the seam**\n\nReading it."]),
            ),
            vec![completed_section(0)]
        );
    }

    #[test]
    fn each_summary_section_becomes_its_own_titled_block() {
        let mut correlation = reasoning_turn();

        project(&mut correlation, started());
        project(&mut correlation, delta(0, "**Seam**\n\nReading it."));
        assert_eq!(
            project(&mut correlation, section_break(1)),
            vec![completed_section(0), started_section(1)]
        );
        assert_eq!(
            project(&mut correlation, delta(1, "**Store**\n\nNow the store.")),
            vec![
                titled_section(1, "Store"),
                section_content(1, "Now the store."),
            ]
        );

        assert_eq!(
            project(
                &mut correlation,
                completed(&["**Seam**\n\nReading it.", "**Store**\n\nNow the store."]),
            ),
            vec![completed_section(1)]
        );
    }

    #[test]
    fn a_section_that_leads_with_no_heading_becomes_an_untitled_block() {
        let mut correlation = reasoning_turn();

        project(&mut correlation, started());
        project(&mut correlation, delta(0, "**Seam**\n\nReading it."));
        project(&mut correlation, section_break(1));

        assert_eq!(
            project(&mut correlation, delta(1, "Now the store.")),
            vec![section_content(1, "Now the store.")]
        );
        assert_eq!(
            project(
                &mut correlation,
                completed(&["**Seam**\n\nReading it.", "Now the store."]),
            ),
            vec![completed_section(1)]
        );
    }

    #[test]
    fn the_completed_item_carries_the_rest_of_the_section_the_stream_stopped_short_of() {
        let mut correlation = reasoning_turn();

        project(&mut correlation, started());
        project(&mut correlation, delta(0, "**Seam**\n\nReading it."));
        project(&mut correlation, section_break(1));
        project(&mut correlation, delta(1, "Now the"));

        assert_eq!(
            project(
                &mut correlation,
                completed(&["**Seam**\n\nReading it.", "Now the store."]),
            ),
            vec![section_content(1, " store."), completed_section(1)]
        );
    }

    #[test]
    fn the_break_codex_announces_before_the_first_section_opens_no_further_block() {
        let mut correlation = reasoning_turn();

        project(&mut correlation, started());
        assert_eq!(project(&mut correlation, section_break(0)), Vec::new());

        // The break the started block already stands for opens nothing, so the
        // section streams into that block rather than into an empty one after it.
        assert_eq!(
            project(&mut correlation, delta(0, "Reading it.")),
            vec![section_content(0, "Reading it.")]
        );
        assert_eq!(
            project(&mut correlation, completed(&["Reading it."])),
            vec![completed_section(0)]
        );
    }

    #[test]
    fn a_section_streams_into_the_block_codex_names_rather_than_the_next_one_along() {
        let mut correlation = reasoning_turn();

        project(&mut correlation, started());
        // Codex opens a section it then says nothing in. Counting the breaks
        // would leave the section that follows streaming into the block its
        // predecessor opened, and the completed item would repeat it into a
        // second block beside it.
        project(&mut correlation, section_break(0));
        assert_eq!(
            project(&mut correlation, section_break(1)),
            vec![completed_section(0), started_section(1)]
        );
        assert_eq!(
            project(&mut correlation, delta(1, "Reading it.")),
            vec![section_content(1, "Reading it.")]
        );
        assert_eq!(
            project(&mut correlation, completed(&["", "Reading it."])),
            vec![completed_section(1)]
        );
    }

    #[test]
    fn a_section_whose_break_never_arrived_still_opens_a_block_of_its_own() {
        let mut correlation = reasoning_turn();

        project(&mut correlation, started());
        project(&mut correlation, delta(0, "**Seam**\n\nReading it."));

        assert_eq!(
            project(&mut correlation, delta(1, "Now the store.")),
            vec![
                completed_section(0),
                started_section(1),
                section_content(1, "Now the store."),
            ]
        );
    }

    #[test]
    fn a_completion_for_reasoning_suru_never_saw_start_is_dropped_rather_than_failing_the_turn() {
        let mut correlation = reasoning_turn();

        assert_eq!(
            project(&mut correlation, completed(&["**Seam**\n\nReading it."])),
            Vec::new()
        );
    }

    #[test]
    fn a_summary_codex_never_streamed_arrives_whole_with_the_completed_item() {
        let mut correlation = reasoning_turn();

        project(&mut correlation, started());

        assert_eq!(
            project(
                &mut correlation,
                completed(&["**Seam**\n\nReading it.", "**Store**\n\nNow the store."]),
            ),
            vec![
                titled_section(0, "Seam"),
                section_content(0, "Reading it."),
                completed_section(0),
                started_section(1),
                titled_section(1, "Store"),
                section_content(1, "Now the store."),
                completed_section(1),
            ]
        );
    }

    #[test]
    fn a_completed_summary_that_contradicts_its_stream_still_settles_the_block() {
        let mut correlation = reasoning_turn();

        project(&mut correlation, started());
        project(&mut correlation, delta(0, "**Seam**\n\nReading it."));

        assert_eq!(
            project(&mut correlation, completed(&["Something else entirely."])),
            vec![completed_section(0)]
        );
    }

    #[test]
    fn reasoning_from_a_turn_suru_no_longer_tracks_is_dropped() {
        let mut correlation = reasoning_turn();

        assert_eq!(
            project(
                &mut correlation,
                NativeNotification::ReasoningStarted {
                    thread_id: THREAD.to_owned(),
                    turn_id: "turn-elsewhere".to_owned(),
                    item_id: ITEM.to_owned(),
                },
            ),
            Vec::new()
        );
    }

    const CHILD_THREAD: &str = "child-thread-fixture";

    fn child_activity(kind: NativeSubagentActivityKind) -> NativeNotification {
        NativeNotification::SubagentActivity {
            thread_id: THREAD.to_owned(),
            kind,
            agent_thread_id: CHILD_THREAD.to_owned(),
            agent_path: "/root/scout".to_owned(),
        }
    }

    fn project_attributed(
        correlation: &mut NativeCorrelation,
        notification: NativeNotification,
    ) -> Vec<AttributedProviderEvent> {
        project_native_notification(correlation, notification).expect("project the notification")
    }

    #[test]
    fn a_spawned_childs_items_ride_its_subagent_attribution_whatever_turn_they_name() {
        let mut correlation = reasoning_turn();

        let spawned = project_attributed(
            &mut correlation,
            child_activity(NativeSubagentActivityKind::Started),
        );
        assert_eq!(
            spawned,
            vec![AttributedProviderEvent {
                attribution: ProviderEventAttribution::OwningSession,
                event: ProviderEvent::SubagentStarted {
                    subagent_id: ProviderSubagentId::new(CHILD_THREAD),
                    name: "scout".to_owned(),
                    description: String::new(),
                    delegation: None,
                },
            }],
            "the spawn rides the spawning conversation, named off the agent path, and carries \
             no Delegation because the activity never says what the child was handed"
        );
        assert_eq!(
            correlation.take_pending_attaches(),
            [CHILD_THREAD],
            "the spawn queues the child thread for attachment"
        );

        // The child's items land under its Subagent whatever turn ids the
        // child's own native turns carry.
        let child_message = project_attributed(
            &mut correlation,
            NativeNotification::AgentMessageStarted {
                thread_id: CHILD_THREAD.to_owned(),
                turn_id: "a-turn-suru-never-heard-of".to_owned(),
                item_id: ITEM.to_owned(),
            },
        );
        assert_eq!(
            child_message,
            vec![AttributedProviderEvent {
                attribution: ProviderEventAttribution::Subagent(ProviderSubagentId::new(
                    CHILD_THREAD
                )),
                event: ProviderEvent::AgentMessageStarted,
            }]
        );
    }

    #[test]
    fn a_spawn_repeating_a_settled_child_reopens_nothing() {
        let mut correlation = reasoning_turn();
        project_attributed(
            &mut correlation,
            child_activity(NativeSubagentActivityKind::Started),
        );

        let settled = project_attributed(
            &mut correlation,
            child_activity(NativeSubagentActivityKind::Completed),
        );
        assert_eq!(
            settled,
            vec![AttributedProviderEvent {
                attribution: ProviderEventAttribution::OwningSession,
                event: ProviderEvent::SubagentCompleted {
                    subagent_id: ProviderSubagentId::new(CHILD_THREAD),
                    status: ProviderSubagentStatus::Completed,
                },
            }]
        );

        correlation.take_pending_attaches();
        assert_eq!(
            project_attributed(
                &mut correlation,
                child_activity(NativeSubagentActivityKind::Started),
            ),
            Vec::new(),
            "the settle already closed the child's Session"
        );
        assert_eq!(
            correlation.take_pending_attaches(),
            Vec::<String>::new(),
            "a spawn that opens nothing attaches nothing"
        );
    }

    #[test]
    fn lifecycle_items_on_a_thread_suru_does_not_follow_are_dropped() {
        let mut correlation = reasoning_turn();

        assert_eq!(
            project_attributed(
                &mut correlation,
                NativeNotification::SubagentActivity {
                    thread_id: "thread-elsewhere".to_owned(),
                    kind: NativeSubagentActivityKind::Started,
                    agent_thread_id: CHILD_THREAD.to_owned(),
                    agent_path: "/root/scout".to_owned(),
                },
            ),
            Vec::new()
        );
        assert_eq!(correlation.take_pending_attaches(), Vec::<String>::new());
    }

    const CHILD_TURN: &str = "child-turn-fixture";
    const RESUMED_TURN: &str = "resumed-turn-fixture";

    /// A collab call the root thread completes, naming the child as its one
    /// receiver and reporting `state` for it where given.
    fn collab_call(
        tool: NativeCollabTool,
        prompt: Option<&str>,
        state: Option<NativeCollabAgentStatus>,
    ) -> NativeNotification {
        NativeNotification::CollabCallCompleted {
            thread_id: THREAD.to_owned(),
            call_id: format!("call-{prompt:?}"),
            tool,
            status: NativeCollabCallStatus::Completed,
            receiver_thread_ids: vec![CHILD_THREAD.to_owned()],
            prompt: prompt.map(str::to_owned),
            agents_states: state
                .map(|status| (CHILD_THREAD.to_owned(), NativeCollabAgentState { status }))
                .into_iter()
                .collect(),
        }
    }

    fn child_turn_started(turn: &str) -> NativeNotification {
        NativeNotification::TurnStarted {
            thread_id: CHILD_THREAD.to_owned(),
            turn_id: turn.to_owned(),
        }
    }

    fn child_turn_completed(turn: &str, outcome: NativeTurnOutcome) -> NativeNotification {
        NativeNotification::TurnCompleted {
            thread_id: CHILD_THREAD.to_owned(),
            turn_id: turn.to_owned(),
            outcome,
            final_agent_message: None,
        }
    }

    fn child_message_started(turn: &str) -> NativeNotification {
        NativeNotification::AgentMessageStarted {
            thread_id: CHILD_THREAD.to_owned(),
            turn_id: turn.to_owned(),
            item_id: ITEM.to_owned(),
        }
    }

    /// The child thread's running total, read under `turn`, standing at
    /// `input` fresh input tokens.
    fn child_reading(turn: &str, input: u64) -> NativeNotification {
        let usage: super::super::wire::NativeThreadTokenUsage =
            serde_json::from_value(serde_json::json!({ "total": { "inputTokens": input } }))
                .expect("decode the fixture reading");
        NativeNotification::TokenUsage {
            thread_id: CHILD_THREAD.to_owned(),
            turn_id: turn.to_owned(),
            total: usage.into_cumulative(),
            context_fill: None,
        }
    }

    fn on_child(event: ProviderEvent) -> AttributedProviderEvent {
        AttributedProviderEvent {
            attribution: ProviderEventAttribution::Subagent(ProviderSubagentId::new(CHILD_THREAD)),
            event,
        }
    }

    fn child_settled(status: ProviderSubagentStatus) -> AttributedProviderEvent {
        AttributedProviderEvent {
            attribution: ProviderEventAttribution::OwningSession,
            event: ProviderEvent::SubagentCompleted {
                subagent_id: ProviderSubagentId::new(CHILD_THREAD),
                status,
            },
        }
    }

    fn child_resumed(
        name: &str,
        description: &str,
        delegation: Option<&str>,
    ) -> AttributedProviderEvent {
        AttributedProviderEvent {
            attribution: ProviderEventAttribution::OwningSession,
            event: ProviderEvent::SubagentResumed {
                subagent_id: ProviderSubagentId::new(CHILD_THREAD),
                name: name.to_owned(),
                description: description.to_owned(),
                delegation: delegation.map(str::to_owned),
            },
        }
    }

    /// The fresh input a projected Usage event carries.
    fn fresh_input(events: &[AttributedProviderEvent]) -> Option<u64> {
        match events {
            [
                AttributedProviderEvent {
                    event: ProviderEvent::Usage { usage, .. },
                    ..
                },
            ] => usage.fresh_input_tokens,
            other => panic!("expected one Usage event, got {other:?}"),
        }
    }

    /// A root Turn that spawned the child through a collab call and saw its
    /// first stretch settle at the end of the child's own native turn.
    fn settled_child() -> NativeCorrelation {
        let mut correlation = reasoning_turn();
        project_attributed(
            &mut correlation,
            collab_call(
                NativeCollabTool::SpawnAgent,
                Some("Map the crate layout"),
                Some(NativeCollabAgentStatus::Running),
            ),
        );
        assert_eq!(
            project_attributed(
                &mut correlation,
                child_turn_completed(CHILD_TURN, NativeTurnOutcome::Completed),
            ),
            vec![child_settled(ProviderSubagentStatus::Completed)],
            "the child's own turn ending settles its stretch"
        );
        correlation.take_pending_attaches();
        correlation
    }

    #[test]
    fn a_send_to_a_settled_child_resumes_it_into_the_turn_the_send_starts() {
        let mut correlation = settled_child();

        assert_eq!(
            project_attributed(
                &mut correlation,
                collab_call(
                    NativeCollabTool::SendInput,
                    Some("\n  List the binaries.  \nKeep it short."),
                    Some(NativeCollabAgentStatus::Completed),
                ),
            ),
            Vec::new(),
            "the send resumes nothing until the child starts the turn it asked for, and the \
             child's previous completion it still reports settles nothing"
        );
        assert_eq!(
            correlation.take_pending_attaches(),
            [CHILD_THREAD],
            "the send attaches the child's thread again, so the turn it starts streams here"
        );
        assert_eq!(
            project_attributed(&mut correlation, child_turn_started(RESUMED_TURN)),
            vec![child_resumed(
                "Agent",
                "List the binaries.",
                Some("\n  List the binaries.  \nKeep it short."),
            )],
            "the turn the send started resumes the child in the conversation that sent it, \
             described by the prompt's first line and opened by the whole prompt"
        );
        assert_eq!(
            project_attributed(&mut correlation, child_message_started(RESUMED_TURN)),
            vec![on_child(ProviderEvent::AgentMessageStarted)],
            "the resumed turn's work lands in the Subagent's Session again"
        );
        assert_eq!(
            project_attributed(&mut correlation, child_message_started(CHILD_TURN)),
            Vec::new(),
            "a step of the settled stretch's turn is its tail, not the resumed stretch's work"
        );
        assert_eq!(
            correlation.child_interrupt_target(CHILD_THREAD),
            Some((CHILD_THREAD.to_owned(), Some(RESUMED_TURN.to_owned()))),
            "a stop addresses the turn the resume runs in"
        );
        assert_eq!(
            project_attributed(
                &mut correlation,
                child_turn_completed(RESUMED_TURN, NativeTurnOutcome::Interrupted),
            ),
            vec![child_settled(ProviderSubagentStatus::Interrupted)],
            "the resumed turn's own end settles the resumed stretch on its outcome"
        );
    }

    #[test]
    fn a_send_whose_turn_already_began_resumes_at_once_and_its_stale_completion_settles_nothing() {
        let mut correlation = settled_child();

        assert_eq!(
            project_attributed(&mut correlation, child_turn_started(RESUMED_TURN)),
            Vec::new(),
            "a turn no delegation has claimed begins nothing yet"
        );
        assert_eq!(
            project_attributed(&mut correlation, child_message_started(RESUMED_TURN)),
            Vec::new(),
            "nor does its work land anywhere before the claim"
        );
        assert_eq!(
            project_attributed(
                &mut correlation,
                collab_call(
                    NativeCollabTool::SendInput,
                    Some("Carry on"),
                    Some(NativeCollabAgentStatus::Completed),
                ),
            ),
            vec![child_resumed("Agent", "Carry on", Some("Carry on"))],
            "the send claims the turn already running, and the previous completion its report \
             still carries does not settle the stretch it just began"
        );
        assert_eq!(
            project_attributed(
                &mut correlation,
                NativeNotification::ReasoningStarted {
                    thread_id: CHILD_THREAD.to_owned(),
                    turn_id: RESUMED_TURN.to_owned(),
                    item_id: ITEM.to_owned(),
                },
            ),
            vec![on_child(ProviderEvent::ReasoningStarted {
                activity_id: section_activity_id(0),
            })],
            "the stretch works on in the Subagent's Session"
        );
    }

    #[test]
    fn resume_agent_alone_begins_no_turn_until_a_send_follows_it() {
        let mut correlation = settled_child();

        // `resumeAgent` decodes as a collab tool Suru reads nothing from.
        assert_eq!(
            project_attributed(
                &mut correlation,
                collab_call(
                    NativeCollabTool::Other,
                    None,
                    Some(NativeCollabAgentStatus::PendingInit),
                ),
            ),
            Vec::new(),
            "reloading a closed child resumes nothing"
        );
        assert_eq!(
            correlation.take_pending_attaches(),
            Vec::<String>::new(),
            "and attaches nothing"
        );
        assert_eq!(
            project_attributed(
                &mut correlation,
                collab_call(
                    NativeCollabTool::SendInput,
                    Some("Pick the audit back up"),
                    Some(NativeCollabAgentStatus::PendingInit),
                ),
            ),
            Vec::new()
        );
        assert_eq!(
            project_attributed(&mut correlation, child_message_started(RESUMED_TURN)),
            vec![
                child_resumed(
                    "Agent",
                    "Pick the audit back up",
                    Some("Pick the audit back up")
                ),
                on_child(ProviderEvent::AgentMessageStarted),
            ],
            "the send after the reload resumes the child at the first step of the turn it \
             started, even with that turn's start unseen"
        );
    }

    #[test]
    fn a_followup_waking_an_idle_v2_agent_resumes_it_with_no_delegation_to_open_with() {
        let mut correlation = reasoning_turn();
        project_attributed(
            &mut correlation,
            child_activity(NativeSubagentActivityKind::Started),
        );
        project_attributed(
            &mut correlation,
            child_turn_completed(CHILD_TURN, NativeTurnOutcome::Completed),
        );
        assert_eq!(
            project_attributed(
                &mut correlation,
                child_activity(NativeSubagentActivityKind::Completed),
            ),
            Vec::new(),
            "the completion report trailing the child's own turn end settles nothing twice"
        );
        correlation.take_pending_attaches();

        assert_eq!(
            project_attributed(
                &mut correlation,
                child_activity(NativeSubagentActivityKind::Interacted),
            ),
            Vec::new(),
            "an interaction starts no turn by itself: a queued message reports the same"
        );
        assert_eq!(correlation.take_pending_attaches(), [CHILD_THREAD]);
        assert_eq!(
            project_attributed(&mut correlation, child_turn_started(RESUMED_TURN)),
            vec![child_resumed("scout", "", None)],
            "the followup's turn resumes the agent under the name its path gives it, with \
             nothing the activity says it was handed"
        );
        assert_eq!(
            project_attributed(
                &mut correlation,
                child_activity(NativeSubagentActivityKind::Completed),
            ),
            vec![child_settled(ProviderSubagentStatus::Completed)],
            "the followup's completion settles the resumed stretch"
        );
    }

    #[test]
    fn a_resumed_stretch_is_metered_from_where_the_threads_total_stood() {
        let mut correlation = reasoning_turn();
        project_attributed(
            &mut correlation,
            child_activity(NativeSubagentActivityKind::Started),
        );
        assert_eq!(
            fresh_input(&project_attributed(
                &mut correlation,
                child_reading(CHILD_TURN, 100)
            )),
            Some(100)
        );
        project_attributed(
            &mut correlation,
            child_turn_completed(CHILD_TURN, NativeTurnOutcome::Completed),
        );
        assert_eq!(
            project_attributed(&mut correlation, child_reading(CHILD_TURN, 120)),
            Vec::new(),
            "a reading trailing the settled stretch belongs to nobody"
        );
        project_attributed(
            &mut correlation,
            child_activity(NativeSubagentActivityKind::Interacted),
        );
        project_attributed(&mut correlation, child_turn_started(RESUMED_TURN));

        assert_eq!(
            project_attributed(&mut correlation, child_reading(CHILD_TURN, 120)),
            Vec::new(),
            "the total the reattach replays against the settled turn is nobody's either"
        );
        assert_eq!(
            fresh_input(&project_attributed(
                &mut correlation,
                child_reading(RESUMED_TURN, 170)
            )),
            Some(50),
            "the resumed Turn counts from where the thread stood, not from its start"
        );
        assert_eq!(
            fresh_input(&project_attributed(
                &mut correlation,
                child_reading(RESUMED_TURN, 200)
            )),
            Some(80),
            "and keeps measuring from that one place"
        );
    }

    #[test]
    fn a_resume_after_the_parents_turn_completed_ends_the_continuation_it_lands_in() {
        let mut correlation = settled_child();
        project_attributed(
            &mut correlation,
            collab_call(
                NativeCollabTool::SendInput,
                Some("Carry on"),
                Some(NativeCollabAgentStatus::Completed),
            ),
        );
        assert_eq!(
            project(
                &mut correlation,
                NativeNotification::TurnCompleted {
                    thread_id: THREAD.to_owned(),
                    turn_id: TURN.to_owned(),
                    outcome: NativeTurnOutcome::Completed,
                    final_agent_message: None,
                },
            ),
            vec![ProviderEvent::TurnCompleted],
            "the parent's turn completes before the child's new one starts"
        );

        assert_eq!(
            project_attributed(&mut correlation, child_turn_started(RESUMED_TURN)),
            vec![
                child_resumed("Agent", "Carry on", Some("Carry on")),
                ProviderEvent::TurnCompleted.into(),
            ],
            "the resume lands in a Continuation no Codex turn of the parent's holds open, so \
             its end comes with it"
        );
        assert_eq!(
            project_attributed(&mut correlation, child_message_started(RESUMED_TURN)),
            vec![on_child(ProviderEvent::AgentMessageStarted)],
            "the child works on in its own Session"
        );
    }

    #[test]
    fn a_resume_during_the_parents_turn_leaves_that_turn_to_codex() {
        let mut correlation = settled_child();
        project_attributed(
            &mut correlation,
            collab_call(NativeCollabTool::SendInput, Some("Carry on"), None),
        );

        assert_eq!(
            project_attributed(&mut correlation, child_turn_started(RESUMED_TURN)),
            vec![child_resumed("Agent", "Carry on", Some("Carry on"))],
            "the parent's own turn is still running, and ends at Codex's boundary"
        );
    }

    #[test]
    fn a_send_to_a_child_spawned_before_a_restart_resumes_it_under_its_thread_id() {
        let mut correlation = reasoning_turn();

        assert_eq!(
            project_attributed(
                &mut correlation,
                collab_call(
                    NativeCollabTool::SendInput,
                    Some("Pick up where you left off"),
                    Some(NativeCollabAgentStatus::Completed),
                ),
            ),
            Vec::new()
        );
        assert_eq!(
            correlation.take_pending_attaches(),
            [CHILD_THREAD],
            "a thread this connection never followed is attached for the turn the send starts"
        );
        assert_eq!(
            correlation.observe_child_model(CHILD_THREAD, ModelId::new("gpt-child"), true),
            Vec::new(),
            "its Model has no stretch here to land in yet"
        );
        assert_eq!(
            project_attributed(&mut correlation, child_message_started(RESUMED_TURN)),
            vec![
                child_resumed(
                    "Agent",
                    "Pick up where you left off",
                    Some("Pick up where you left off"),
                ),
                AttributedProviderEvent {
                    attribution: ProviderEventAttribution::OwningSession,
                    event: ProviderEvent::SubagentModelChanged {
                        subagent_id: ProviderSubagentId::new(CHILD_THREAD),
                        model: ModelId::new("gpt-child"),
                    },
                },
                on_child(ProviderEvent::AgentMessageStarted),
            ],
            "the resume names the child's thread, which is the identity its Session was \
             stored under, and the Model the attach reported follows it into the new Turn"
        );
    }

    #[test]
    fn a_send_to_a_working_child_resumes_nothing_and_leaves_its_row_alone() {
        let mut correlation = working_child();

        assert_eq!(
            project_attributed(
                &mut correlation,
                collab_call(
                    NativeCollabTool::SendInput,
                    Some("Count the lines too"),
                    Some(NativeCollabAgentStatus::Running),
                ),
            ),
            Vec::new(),
            "a send into the running turn begins no stretch, and the row keeps its spawn's \
             description"
        );
        assert_eq!(correlation.take_pending_attaches(), Vec::<String>::new());
    }

    /// A root Turn whose collab call spawned the child, which is now working:
    /// it has streamed work of its own.
    fn working_child() -> NativeCorrelation {
        let mut correlation = reasoning_turn();
        project_attributed(
            &mut correlation,
            collab_call(
                NativeCollabTool::SpawnAgent,
                Some("Map the crate layout"),
                Some(NativeCollabAgentStatus::Running),
            ),
        );
        project_attributed(&mut correlation, child_message_started(CHILD_TURN));
        correlation.take_pending_attaches();
        correlation
    }

    /// Input `thread` drained under `turn`, reading `text`.
    fn user_message(thread: &str, turn: &str, text: &str) -> NativeNotification {
        NativeNotification::UserMessage {
            thread_id: thread.to_owned(),
            turn_id: turn.to_owned(),
            text: text.to_owned(),
        }
    }

    /// The steer `text` into `subagent`'s working Turn, sent by `sender`.
    fn steered(
        sender: ProviderEventAttribution,
        subagent: &str,
        text: &str,
    ) -> AttributedProviderEvent {
        AttributedProviderEvent {
            attribution: sender,
            event: ProviderEvent::SubagentSteered {
                subagent_id: ProviderSubagentId::new(subagent),
                delegation: text.to_owned(),
            },
        }
    }

    /// A `sendInput` from `sender` to `receiver` starting, under `call_id`.
    fn send_started(sender: &str, receiver: &str, call_id: &str, text: &str) -> NativeNotification {
        NativeNotification::CollabCallStarted {
            thread_id: sender.to_owned(),
            call_id: call_id.to_owned(),
            tool: NativeCollabTool::SendInput,
            receiver_thread_ids: vec![receiver.to_owned()],
            prompt: Some(text.to_owned()),
        }
    }

    /// The same `sendInput` completing, with its receiver still running.
    fn send_completed(
        sender: &str,
        receiver: &str,
        call_id: &str,
        text: &str,
    ) -> NativeNotification {
        NativeNotification::CollabCallCompleted {
            thread_id: sender.to_owned(),
            call_id: call_id.to_owned(),
            tool: NativeCollabTool::SendInput,
            status: NativeCollabCallStatus::Completed,
            receiver_thread_ids: vec![receiver.to_owned()],
            prompt: Some(text.to_owned()),
            agents_states: [(
                receiver.to_owned(),
                NativeCollabAgentState {
                    status: NativeCollabAgentStatus::Running,
                },
            )]
            .into_iter()
            .collect(),
        }
    }

    #[test]
    fn a_send_the_working_child_drains_steers_it_from_the_conversation_that_sent_it() {
        let mut correlation = working_child();
        project_attributed(
            &mut correlation,
            collab_call(
                NativeCollabTool::SendInput,
                Some("Count the lines too"),
                Some(NativeCollabAgentStatus::Running),
            ),
        );

        assert_eq!(
            project_attributed(
                &mut correlation,
                user_message(CHILD_THREAD, CHILD_TURN, "Count the lines too"),
            ),
            vec![steered(
                ProviderEventAttribution::OwningSession,
                CHILD_THREAD,
                "Count the lines too",
            )],
            "the drained input steers the child's working Turn, sent by the root thread"
        );
        assert_eq!(
            correlation.child_interrupt_target(CHILD_THREAD),
            Some((CHILD_THREAD.to_owned(), Some(CHILD_TURN.to_owned()))),
            "the child works on in the same stretch and native turn"
        );
    }

    #[test]
    fn the_input_opening_a_childs_stretch_is_not_a_steer() {
        let mut correlation = reasoning_turn();
        project_attributed(
            &mut correlation,
            collab_call(
                NativeCollabTool::SpawnAgent,
                Some("Map the crate layout"),
                Some(NativeCollabAgentStatus::Running),
            ),
        );

        assert_eq!(
            project_attributed(
                &mut correlation,
                user_message(CHILD_THREAD, CHILD_TURN, "Map the crate layout"),
            ),
            Vec::new(),
            "the spawn's Delegation already opens the child's Turn"
        );
        assert_eq!(
            project_attributed(
                &mut correlation,
                user_message(CHILD_THREAD, CHILD_TURN, "Map the crate layout"),
            ),
            vec![steered(
                ProviderEventAttribution::OwningSession,
                CHILD_THREAD,
                "Map the crate layout",
            )],
            "only the first is the opener: a later one is a steer, whatever it says"
        );
    }

    #[test]
    fn the_input_opening_a_resumed_stretch_is_not_a_steer() {
        let mut correlation = settled_child();
        project_attributed(
            &mut correlation,
            collab_call(
                NativeCollabTool::SendInput,
                Some("List the binaries."),
                Some(NativeCollabAgentStatus::Completed),
            ),
        );
        project_attributed(&mut correlation, child_turn_started(RESUMED_TURN));

        assert_eq!(
            project_attributed(
                &mut correlation,
                user_message(CHILD_THREAD, RESUMED_TURN, "List the binaries."),
            ),
            Vec::new(),
            "the resume's Delegation already opens the resumed Turn"
        );
    }

    /// The Delegation that began `subagent`'s stretch, reported by its opening
    /// input because the spawn or resume that began the stretch carried none.
    fn delegated(
        delegator: ProviderEventAttribution,
        subagent: &str,
        text: &str,
    ) -> AttributedProviderEvent {
        AttributedProviderEvent {
            attribution: delegator,
            event: ProviderEvent::SubagentDelegated {
                subagent_id: ProviderSubagentId::new(subagent),
                delegation: text.to_owned(),
            },
        }
    }

    #[test]
    fn the_input_opening_a_stretch_its_spawn_said_nothing_of_is_the_delegation_that_began_it() {
        let mut correlation = reasoning_turn();
        project_attributed(
            &mut correlation,
            child_activity(NativeSubagentActivityKind::Started),
        );
        correlation.take_pending_attaches();

        assert_eq!(
            project_attributed(
                &mut correlation,
                user_message(
                    CHILD_THREAD,
                    CHILD_TURN,
                    "Review the diff\nagainst CONTEXT.md"
                ),
            ),
            vec![delegated(
                ProviderEventAttribution::OwningSession,
                CHILD_THREAD,
                "Review the diff\nagainst CONTEXT.md",
            )],
            "the spawn activity never says what the child was handed, so the child's opening \
             input reports it, from the delegating Agent"
        );
        assert_eq!(
            project_attributed(
                &mut correlation,
                user_message(CHILD_THREAD, CHILD_TURN, "Also read the ADRs"),
            ),
            vec![steered(
                ProviderEventAttribution::OwningSession,
                CHILD_THREAD,
                "Also read the ADRs",
            )],
            "only the first is the opener: a later one is a steer"
        );
    }

    #[test]
    fn the_input_opening_a_stretch_its_resume_said_nothing_of_is_the_delegation_that_began_it() {
        let mut correlation = reasoning_turn();
        project_attributed(
            &mut correlation,
            child_activity(NativeSubagentActivityKind::Started),
        );
        project_attributed(
            &mut correlation,
            user_message(CHILD_THREAD, CHILD_TURN, "Review the diff"),
        );
        project_attributed(
            &mut correlation,
            child_turn_completed(CHILD_TURN, NativeTurnOutcome::Completed),
        );
        project_attributed(
            &mut correlation,
            child_activity(NativeSubagentActivityKind::Interacted),
        );
        correlation.take_pending_attaches();
        assert_eq!(
            project_attributed(&mut correlation, child_turn_started(RESUMED_TURN)),
            vec![child_resumed("scout", "", None)],
            "the interaction resumes the agent with nothing it was handed"
        );

        assert_eq!(
            project_attributed(
                &mut correlation,
                user_message(CHILD_THREAD, RESUMED_TURN, "Now review the tests"),
            ),
            vec![delegated(
                ProviderEventAttribution::OwningSession,
                CHILD_THREAD,
                "Now review the tests",
            )],
            "the resumed turn's opening input reports the Delegation the resume never carried"
        );
    }

    #[test]
    fn a_report_ahead_of_the_input_opening_a_stretch_leaves_every_later_input_a_steer() {
        let mut correlation = reasoning_turn();
        project_attributed(
            &mut correlation,
            child_activity(NativeSubagentActivityKind::Started),
        );
        correlation.take_pending_attaches();
        correlation
            .expect_handed_input(CHILD_THREAD, REPORT.to_owned())
            .expect("a child thread takes handed input");

        assert_eq!(
            project_attributed(
                &mut correlation,
                user_message(CHILD_THREAD, CHILD_TURN, REPORT)
            ),
            Vec::new(),
            "a Report Suru handed the child is no Delegation, even as the stretch's first input"
        );
        assert_eq!(
            project_attributed(
                &mut correlation,
                user_message(CHILD_THREAD, CHILD_TURN, "Also read the ADRs"),
            ),
            vec![steered(
                ProviderEventAttribution::OwningSession,
                CHILD_THREAD,
                "Also read the ADRs",
            )],
            "the Report passed the opening, as any other work would"
        );
    }

    #[test]
    fn an_opener_streamed_before_the_child_was_followed_leaves_the_first_input_a_steer() {
        let mut correlation = reasoning_turn();
        project_attributed(
            &mut correlation,
            collab_call(
                NativeCollabTool::SpawnAgent,
                Some("Map the crate layout"),
                Some(NativeCollabAgentStatus::Running),
            ),
        );

        assert_eq!(
            project_attributed(
                &mut correlation,
                user_message(CHILD_THREAD, CHILD_TURN, "Count the lines too"),
            ),
            vec![steered(
                ProviderEventAttribution::OwningSession,
                CHILD_THREAD,
                "Count the lines too",
            )],
            "input that is not what the spawn said cannot be its opener, which went by \
             before Suru followed the thread"
        );

        let mut correlation = working_child();
        assert_eq!(
            project_attributed(
                &mut correlation,
                user_message(CHILD_THREAD, CHILD_TURN, "Map the crate layout"),
            ),
            vec![steered(
                ProviderEventAttribution::OwningSession,
                CHILD_THREAD,
                "Map the crate layout",
            )],
            "once the child has worked, the opener can no longer arrive, so even input \
             repeating the spawn's is a steer"
        );
    }

    #[test]
    fn a_users_prompt_on_the_sessions_own_thread_is_no_delegation() {
        let mut correlation = working_child();

        assert_eq!(
            project_attributed(
                &mut correlation,
                user_message(THREAD, TURN, "Map the crates"),
            ),
            Vec::new()
        );
    }

    const SIBLING_THREAD: &str = "sibling-thread-fixture";

    /// The working child and a sibling the root thread spawned beside it.
    fn working_siblings() -> NativeCorrelation {
        let mut correlation = working_child();
        project_attributed(
            &mut correlation,
            NativeNotification::CollabCallCompleted {
                thread_id: THREAD.to_owned(),
                call_id: "call-spawn-sibling".to_owned(),
                tool: NativeCollabTool::SpawnAgent,
                status: NativeCollabCallStatus::Completed,
                receiver_thread_ids: vec![SIBLING_THREAD.to_owned()],
                prompt: Some("Review the map".to_owned()),
                agents_states: Default::default(),
            },
        );
        correlation.take_pending_attaches();
        correlation
    }

    #[test]
    fn a_steer_drained_before_its_send_completed_names_the_sibling_that_sent_it() {
        let mut correlation = working_siblings();

        assert_eq!(
            project_attributed(
                &mut correlation,
                send_started(
                    SIBLING_THREAD,
                    CHILD_THREAD,
                    "call-send",
                    "Also count tests"
                ),
            ),
            Vec::new(),
            "a send's start shows nothing"
        );
        let sibling = ProviderEventAttribution::Subagent(ProviderSubagentId::new(SIBLING_THREAD));
        assert_eq!(
            project_attributed(
                &mut correlation,
                user_message(CHILD_THREAD, CHILD_TURN, "Also count tests"),
            ),
            vec![steered(sibling, CHILD_THREAD, "Also count tests")],
            "the steer is the sibling's, whose send carried that text"
        );
        assert_eq!(
            project_attributed(
                &mut correlation,
                send_completed(
                    SIBLING_THREAD,
                    CHILD_THREAD,
                    "call-send",
                    "Also count tests"
                ),
            ),
            Vec::new(),
            "the send's completion trailing its steer shows nothing either"
        );
        assert_eq!(
            project_attributed(
                &mut correlation,
                user_message(CHILD_THREAD, CHILD_TURN, "Also count tests"),
            ),
            vec![steered(
                ProviderEventAttribution::OwningSession,
                CHILD_THREAD,
                "Also count tests",
            )],
            "a send delivers once: the same text again, carried by no known send, is \
             credited to the conversation that spawned the child"
        );
    }

    #[test]
    fn steers_carrying_the_same_text_are_credited_to_their_sends_in_order() {
        let mut correlation = working_siblings();
        let sibling = ProviderEventAttribution::Subagent(ProviderSubagentId::new(SIBLING_THREAD));
        project_attributed(
            &mut correlation,
            send_completed(SIBLING_THREAD, CHILD_THREAD, "call-sibling", "Stop"),
        );
        project_attributed(
            &mut correlation,
            send_started(THREAD, CHILD_THREAD, "call-root", "Stop"),
        );

        assert_eq!(
            [
                project_attributed(
                    &mut correlation,
                    user_message(CHILD_THREAD, CHILD_TURN, "Stop")
                ),
                project_attributed(
                    &mut correlation,
                    user_message(CHILD_THREAD, CHILD_TURN, "Stop")
                ),
            ],
            [
                vec![steered(sibling, CHILD_THREAD, "Stop")],
                vec![steered(
                    ProviderEventAttribution::OwningSession,
                    CHILD_THREAD,
                    "Stop"
                )],
            ]
        );
    }

    #[test]
    fn a_send_the_stretch_never_drained_is_forgotten_when_it_settles() {
        let mut correlation = reasoning_turn();
        project_attributed(
            &mut correlation,
            collab_call(
                NativeCollabTool::SpawnAgent,
                Some("Map the crate layout"),
                Some(NativeCollabAgentStatus::Running),
            ),
        );
        project_attributed(
            &mut correlation,
            NativeNotification::CollabCallCompleted {
                thread_id: CHILD_THREAD.to_owned(),
                call_id: "call-spawn-grandchild".to_owned(),
                tool: NativeCollabTool::SpawnAgent,
                status: NativeCollabCallStatus::Completed,
                receiver_thread_ids: vec![SIBLING_THREAD.to_owned()],
                prompt: Some("Read the tests".to_owned()),
                agents_states: Default::default(),
            },
        );
        project_attributed(
            &mut correlation,
            send_started(THREAD, SIBLING_THREAD, "call-root", "Hurry"),
        );
        project_attributed(
            &mut correlation,
            NativeNotification::TurnCompleted {
                thread_id: SIBLING_THREAD.to_owned(),
                turn_id: "grandchild-turn".to_owned(),
                outcome: NativeTurnOutcome::Completed,
                final_agent_message: None,
            },
        );
        project_attributed(
            &mut correlation,
            NativeNotification::SubagentActivity {
                thread_id: CHILD_THREAD.to_owned(),
                kind: NativeSubagentActivityKind::Interacted,
                agent_thread_id: SIBLING_THREAD.to_owned(),
                agent_path: "/root/scout/reader".to_owned(),
            },
        );
        project_attributed(
            &mut correlation,
            NativeNotification::TurnStarted {
                thread_id: SIBLING_THREAD.to_owned(),
                turn_id: "grandchild-turn-2".to_owned(),
            },
        );
        project_attributed(
            &mut correlation,
            NativeNotification::AgentMessageStarted {
                thread_id: SIBLING_THREAD.to_owned(),
                turn_id: "grandchild-turn-2".to_owned(),
                item_id: ITEM.to_owned(),
            },
        );

        assert_eq!(
            project_attributed(
                &mut correlation,
                user_message(SIBLING_THREAD, "grandchild-turn-2", "Hurry"),
            ),
            vec![steered(
                ProviderEventAttribution::Subagent(ProviderSubagentId::new(CHILD_THREAD)),
                SIBLING_THREAD,
                "Hurry",
            )],
            "the root's send never reached the settled stretch, so a steer into the child's \
             resumed stretch is credited to the child that resumed it"
        );
    }

    #[test]
    fn a_send_whose_report_settles_its_working_receiver_resumes_it() {
        let mut correlation = reasoning_turn();
        project_attributed(
            &mut correlation,
            collab_call(
                NativeCollabTool::SpawnAgent,
                Some("Map the crate layout"),
                Some(NativeCollabAgentStatus::Running),
            ),
        );
        project_attributed(&mut correlation, child_message_started(CHILD_TURN));

        assert_eq!(
            project_attributed(
                &mut correlation,
                collab_call(
                    NativeCollabTool::SendInput,
                    Some("Now the tests"),
                    Some(NativeCollabAgentStatus::Completed),
                ),
            ),
            vec![child_settled(ProviderSubagentStatus::Completed)],
            "a child Codex reports done was idle, so the send settles its stretch"
        );
        assert_eq!(
            project_attributed(&mut correlation, child_turn_started(RESUMED_TURN)),
            vec![child_resumed(
                "Agent",
                "Now the tests",
                Some("Now the tests")
            )],
            "and the turn the send started resumes it"
        );
    }

    #[test]
    fn model_evidence_for_a_settled_child_waits_for_its_resume_but_a_late_attach_lands() {
        let mut correlation = settled_child();

        assert_eq!(
            correlation.observe_child_model(CHILD_THREAD, ModelId::new("gpt-child"), true),
            vec![AttributedProviderEvent {
                attribution: ProviderEventAttribution::OwningSession,
                event: ProviderEvent::SubagentModelChanged {
                    subagent_id: ProviderSubagentId::new(CHILD_THREAD),
                    model: ModelId::new("gpt-child"),
                },
            }],
            "the attach reply trailing the spawn's stretch still names its row's Model"
        );
        assert_eq!(
            correlation.observe_child_model(CHILD_THREAD, ModelId::new("gpt-later"), false),
            Vec::new(),
            "a settings update between stretches rewrites no settled row"
        );
        project_attributed(
            &mut correlation,
            collab_call(NativeCollabTool::SendInput, Some("Again"), None),
        );
        assert_eq!(
            project_attributed(&mut correlation, child_turn_started(RESUMED_TURN)),
            vec![
                child_resumed("Agent", "Again", Some("Again")),
                AttributedProviderEvent {
                    attribution: ProviderEventAttribution::OwningSession,
                    event: ProviderEvent::SubagentModelChanged {
                        subagent_id: ProviderSubagentId::new(CHILD_THREAD),
                        model: ModelId::new("gpt-later"),
                    },
                },
            ],
            "the resume carries the latest Model into the Turn it begins"
        );
    }

    #[test]
    fn generic_failures_do_not_treat_incidental_choice_text_as_selection_rejection() {
        let selection = AgentSelection {
            provider: ProviderId::new("codex"),
            model: ModelId::new("gpt-fixture"),
            options: vec![ModelOptionSelection {
                id: ModelOptionId::new("reasoning_effort"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("low"),
                },
            }],
        };

        assert!(!is_native_selection_rejection(
            "Access denied because credits are low",
            &NativeTurnFailureKind::Other,
            &selection,
        ));
    }

    /// A Subagent Report as Suru hands a native child it, and as the
    /// `UserMessage` it arrives as reads.
    const REPORT: &str = "Subagent Report from Suru: the Subagent \"Researcher\" you delegated to \
                          through the Broker completed.";

    /// The settled child woken into a Continuation of its own Session, with
    /// no resume and no row.
    fn child_woken() -> AttributedProviderEvent {
        AttributedProviderEvent {
            attribution: ProviderEventAttribution::OwningSession,
            event: ProviderEvent::SubagentWoken {
                subagent_id: ProviderSubagentId::new(CHILD_THREAD),
            },
        }
    }

    #[test]
    fn a_report_handed_to_a_settled_child_wakes_it_into_the_turn_it_begins_with_no_resume() {
        let mut correlation = settled_child();

        assert!(
            correlation
                .expect_handed_input(CHILD_THREAD, REPORT.to_owned())
                .expect("a child thread takes handed input"),
            "a child with no stretch open is attached again before the input is handed over"
        );
        correlation.note_settled_child_model(CHILD_THREAD, ModelId::new("gpt-child"));
        assert_eq!(
            project_attributed(&mut correlation, child_turn_started(RESUMED_TURN)),
            vec![
                child_woken(),
                AttributedProviderEvent {
                    attribution: ProviderEventAttribution::OwningSession,
                    event: ProviderEvent::SubagentModelChanged {
                        subagent_id: ProviderSubagentId::new(CHILD_THREAD),
                        model: ModelId::new("gpt-child"),
                    },
                },
            ],
            "the turn the Report begins wakes the child into a Continuation with no resume, \
             since nothing delegated it, carrying on the Model it runs on"
        );
        correlation.handed_input_taken(CHILD_THREAD, RESUMED_TURN.to_owned());
        assert_eq!(
            project_attributed(
                &mut correlation,
                user_message(CHILD_THREAD, RESUMED_TURN, REPORT)
            ),
            Vec::new(),
            "the Report stands nowhere: it is no Delegation"
        );
        assert_eq!(
            project_attributed(&mut correlation, child_message_started(RESUMED_TURN)),
            vec![on_child(ProviderEvent::AgentMessageStarted)],
            "what the woken child does lands in its own Session"
        );
        assert_eq!(
            project_attributed(
                &mut correlation,
                child_turn_completed(RESUMED_TURN, NativeTurnOutcome::Completed),
            ),
            vec![child_settled(ProviderSubagentStatus::Completed)],
            "and the turn's end settles the stretch it woke into"
        );
    }

    #[test]
    fn a_report_codex_steers_into_a_turn_suru_saw_settle_still_ends_the_stretch_it_wakes() {
        let mut correlation = settled_child();
        correlation
            .expect_handed_input(CHILD_THREAD, REPORT.to_owned())
            .expect("a child thread takes handed input");

        // The child's lifecycle report settled it while its native turn still
        // ran, and Codex steered the Report into that turn.
        correlation.handed_input_taken(CHILD_THREAD, CHILD_TURN.to_owned());

        assert_eq!(
            project_attributed(
                &mut correlation,
                child_turn_completed(CHILD_TURN, NativeTurnOutcome::Completed),
            ),
            vec![
                child_woken(),
                child_settled(ProviderSubagentStatus::Completed)
            ],
            "the turn's end settles the stretch the Report woke the child into, which nothing \
             else would"
        );
    }

    #[test]
    fn a_report_codex_never_took_wakes_nothing() {
        let mut correlation = settled_child();
        correlation
            .expect_handed_input(CHILD_THREAD, REPORT.to_owned())
            .expect("a child thread takes handed input");
        correlation.forget_handed_input(CHILD_THREAD, REPORT);

        assert_eq!(
            project_attributed(&mut correlation, child_turn_started(RESUMED_TURN)),
            Vec::new(),
            "a turn the child begins is left for a delegation to claim"
        );
    }

    #[test]
    fn a_report_steered_into_a_working_childs_turn_stands_nowhere() {
        let mut correlation = working_child();

        assert!(
            !correlation
                .expect_handed_input(CHILD_THREAD, REPORT.to_owned())
                .expect("a child thread takes handed input"),
            "a working child streams here already"
        );
        assert_eq!(
            project_attributed(
                &mut correlation,
                user_message(CHILD_THREAD, CHILD_TURN, REPORT)
            ),
            Vec::new(),
            "the Report its running turn drains is no steer Delegation"
        );
        assert_eq!(
            project_attributed(
                &mut correlation,
                user_message(CHILD_THREAD, CHILD_TURN, "Also list the binaries.")
            ),
            vec![steered(
                ProviderEventAttribution::OwningSession,
                CHILD_THREAD,
                "Also list the binaries."
            )],
            "while a steer an Agent sent still stands"
        );
    }

    #[test]
    fn a_report_codex_begins_a_turn_with_after_a_working_childs_turn_ended_wakes_it_there() {
        let mut correlation = working_child();
        assert!(
            !correlation
                .expect_handed_input(CHILD_THREAD, REPORT.to_owned())
                .expect("a child thread takes handed input"),
            "a working child streams here already"
        );

        // The child's turn ended as the Report arrived, so Codex began another
        // turn with it rather than steering the one Suru saw working.
        correlation.handed_input_taken(CHILD_THREAD, RESUMED_TURN.to_owned());
        assert_eq!(
            project_attributed(
                &mut correlation,
                child_turn_completed(CHILD_TURN, NativeTurnOutcome::Completed),
            ),
            vec![child_settled(ProviderSubagentStatus::Completed)],
            "the stretch the child worked in settles with its own turn"
        );
        assert_eq!(
            project_attributed(&mut correlation, child_turn_started(RESUMED_TURN)),
            vec![child_woken()],
            "and the turn the Report began wakes the child into a Continuation, with no resume"
        );
        assert_eq!(
            project_attributed(
                &mut correlation,
                user_message(CHILD_THREAD, RESUMED_TURN, REPORT)
            ),
            Vec::new(),
            "the Report stands nowhere"
        );
        assert_eq!(
            project_attributed(&mut correlation, child_message_started(RESUMED_TURN)),
            vec![on_child(ProviderEvent::AgentMessageStarted)],
            "what the woken child does lands in its own Session"
        );
        let approval = project_attributed(
            &mut correlation,
            NativeNotification::CommandApprovalRequested {
                id: super::super::wire::RequestId::Integer(7),
                params: serde_json::from_value(serde_json::json!({
                    "threadId": CHILD_THREAD,
                    "turnId": RESUMED_TURN,
                    "itemId": "command-in-the-woken-turn",
                    "command": "cargo test",
                }))
                .expect("decode the fixture approval"),
            },
        );
        let [
            AttributedProviderEvent {
                attribution: ProviderEventAttribution::Subagent(subagent),
                event: ProviderEvent::ApprovalRequested { approval, .. },
            },
        ] = approval.as_slice()
        else {
            panic!("an Approval it asks for is routed to it, so it can be answered: {approval:?}");
        };
        assert_eq!(subagent.as_str(), CHILD_THREAD);
        assert_eq!(
            correlation.child_interrupt_target(CHILD_THREAD),
            Some((CHILD_THREAD.to_owned(), Some(RESUMED_TURN.to_owned()))),
            "a stop reaches the turn it works in"
        );
        assert_eq!(
            project_attributed(
                &mut correlation,
                child_turn_completed(RESUMED_TURN, NativeTurnOutcome::Completed),
            ),
            vec![
                on_child(ProviderEvent::ApprovalWithdrawn { id: approval.id }),
                child_settled(ProviderSubagentStatus::Completed),
            ],
            "and that turn's end settles the Continuation, withdrawing what it left unanswered"
        );
    }

    #[test]
    fn a_report_a_working_childs_turn_took_leaves_the_next_turn_to_delegations() {
        let mut correlation = working_child();
        correlation
            .expect_handed_input(CHILD_THREAD, REPORT.to_owned())
            .expect("a child thread takes handed input");
        project_attributed(&mut correlation, child_turn_started(CHILD_TURN));

        // Codex steered the Report into the turn the child works in.
        correlation.handed_input_taken(CHILD_THREAD, CHILD_TURN.to_owned());
        project_attributed(
            &mut correlation,
            child_turn_completed(CHILD_TURN, NativeTurnOutcome::Completed),
        );

        assert_eq!(
            project_attributed(&mut correlation, child_turn_started(RESUMED_TURN)),
            Vec::new(),
            "a later turn is no wake of the Report's: it waits for a delegation to claim it"
        );
    }

    #[test]
    fn no_report_is_handed_to_the_sessions_own_thread() {
        let mut correlation = reasoning_turn();

        assert!(
            correlation
                .expect_handed_input(THREAD, REPORT.to_owned())
                .is_err()
        );
    }
}
