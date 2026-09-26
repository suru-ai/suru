//! Projection of Codex's native notifications onto attributed Provider events.
//!
//! [`NativeCorrelation`] is the running state this projection needs: which native Turn is active
//! on the Session's own thread, the Messages, commands, file changes, and native Reasoning items
//! each followed thread still has open, and the collab child threads the agent has spawned. A
//! spawn item on a followed thread opens its child as a Subagent — the event pump then attaches
//! the child thread so its own items stream over this connection too — and every item projects
//! under the attribution of the thread that produced it, which is how a child's work lands in its
//! Subagent's Session rather than the parent's Transcript, and how a child's own spawns recurse.
//! Subagent lifecycle items are read regardless of the active Turn, because Codex documents a
//! child's completion arriving after the parent turn's own (ADR 0015). Notifications that belong
//! to no thread Suru follows are dropped, notifications that contradict the recorded state fail
//! the Session, and everything else becomes the Provider events a Session consumes.

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex},
};

use futures_util::stream;
use serde_json::Value;
use tokio::sync::mpsc;

use super::super::shell_wrapper::strip_launcher_wrapper;
use super::{
    DEFAULT_SERVICE_TIER_CHOICE_ID, REASONING_EFFORT_OPTION_ID, SERVICE_TIER_OPTION_ID,
    approval::{CodexApprovals, NativeApprovalIdentity, NativeApprovalKind},
    codex_error,
    transport::JsonRpcTransport,
    wire::{
        CodexPosture, NativeCollabAgentState, NativeCollabAgentStatus, NativeCollabCallStatus,
        NativeCollabTool, NativeCommandStatus, NativeCumulativeUsage, NativeField,
        NativeFileChange, NativeFileChangeStatus, NativeNotification, NativeSubagentActivityKind,
        NativeTurnFailureKind, NativeTurnOutcome, ThreadConnectionResult, ThreadResumeParams,
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
        ProviderFileChangeStatus, ProviderSubagentId, ProviderSubagentStatus,
        harness::ProcessGuard,
        reasoning::{ReasoningSegment, ReasoningSummarySplitter},
    },
};

/// What a Subagent's row calls the agent when Codex's wire names no kind for
/// it, as on the collab shape whose spawn calls carry only a prompt.
const GENERIC_SUBAGENT_NAME: &str = "Agent";

/// The one measurement a followed child thread's whole run is taken under.
/// Codex announces no Turn boundary for a child, so naming every reading the
/// same fixes the child's baseline at nothing the first time and never rebases
/// it: the Subagent's Turn spans however many native Turns Codex runs.
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
    /// The spawned child threads Suru follows, by thread id — the identity
    /// each one's Subagent is known by.
    children: HashMap<String, AttachedChild>,
    /// Child threads whose Subagents settled. A spawn item repeating one of
    /// them re-opens nothing, because the settle already closed its Session.
    settled_children: HashSet<String>,
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
    active_reasoning: HashMap<String, ActiveNativeReasoning>,
}

/// One spawned child thread Suru follows: the items it has open, the
/// description its row currently reads — kept so a collab call repeating it
/// unchanged publishes nothing — and the latest native turn its items have
/// ridden under, which is what a stop must name to `turn/interrupt` the
/// child. Codex announces no turn boundary for a followed child, so the items
/// themselves are the only account of it.
#[derive(Default)]
struct AttachedChild {
    in_flight: ThreadInFlight,
    description: String,
    latest_turn_id: Option<String>,
    /// Codex's running total for this child's own thread. It keeps its opening
    /// baseline of nothing for the thread's whole life, because a spawned
    /// thread is opened for the delegation and everything it ever meters is
    /// the Subagent's Turn — including whatever it ran before Suru attached.
    metering: ThreadMetering,
    model: Option<ModelId>,
    pricing_baseline: NativeCumulativeUsage,
    unpriced_prefix: bool,
    estimate_blocked: bool,
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
            settled_children: HashSet::new(),
            child_context_turns: HashMap::new(),
            pending_attaches: Vec::new(),
        }
    }

    fn observe_child_model(&mut self, thread_id: &str, model: ModelId) -> Option<Usage> {
        let child = self.children.get_mut(thread_id)?;
        match child.model.as_ref() {
            None => {
                child.pricing_baseline = child.metering.latest;
                child.unpriced_prefix = has_billable_usage(
                    &child
                        .metering
                        .latest
                        .since(NativeCumulativeUsage::default()),
                );
                child.model = Some(model);
                None
            }
            Some(current) if current == &model => None,
            Some(_) => {
                child.model = Some(model);
                child.estimate_blocked = true;
                Some(child.metering.latest.since(child.metering.baseline))
            }
        }
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
    /// the active one, or a followed child thread under its Subagent's
    /// identity. A child's steps are not gated on its turn ids, because the
    /// child's whole run is one Subagent's stream however many native turns
    /// Codex runs it as. Anything else has nowhere to land and is dropped.
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
        if let Some(turns) = self.child_context_turns.get_mut(thread_id) {
            turns.observe(turn_id);
        }
        self.children.get_mut(thread_id).map(|child| {
            child.latest_turn_id = Some(turn_id.to_owned());
            (
                &mut child.in_flight,
                ProviderEventAttribution::Subagent(ProviderSubagentId::new(thread_id)),
            )
        })
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
    /// if that is a thread Suru follows. Deliberately not gated on the active
    /// Turn: the lifecycle speaks for threads rather than turns, and a child's
    /// completion may arrive after the turn that spawned it completed.
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
            || self.settled_children.contains(&child_thread_id)
        {
            return Vec::new();
        }
        self.children.insert(
            child_thread_id.clone(),
            AttachedChild {
                description: description.clone(),
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

    /// Revises what a followed child's row says it is doing. Threads not
    /// followed, and calls repeating the description unchanged, publish
    /// nothing.
    fn revise_child_description(
        &mut self,
        child_thread_id: &str,
        description: &str,
    ) -> Vec<AttributedProviderEvent> {
        let Some(child) = self.children.get_mut(child_thread_id) else {
            return Vec::new();
        };
        if child.description == description {
            return Vec::new();
        }
        child.description = description.to_owned();
        vec![AttributedProviderEvent {
            attribution: ProviderEventAttribution::OwningSession,
            event: ProviderEvent::SubagentUpdated {
                subagent_id: ProviderSubagentId::new(child_thread_id),
                description: description.to_owned(),
            },
        }]
    }

    /// Settles one Subagent, dropping its thread from the followed set: the
    /// settle closes the row and the child Session together. Later output is
    /// discarded, while Context Fill retains its attribution. Like the spawn's counterparts on the
    /// other Providers, the settle addresses the row by the Subagent's own
    /// identity and rides the owning conversation, so a nested Subagent's
    /// settle still lands after its spawner's own — order the wire does not
    /// promise.
    fn settle_child(
        &mut self,
        child_thread_id: &str,
        status: ProviderSubagentStatus,
    ) -> Vec<AttributedProviderEvent> {
        if self.children.remove(child_thread_id).is_none() {
            return Vec::new();
        }
        self.questionnaires.end_thread(child_thread_id);
        self.approvals.end_thread(child_thread_id);
        self.settled_children.insert(child_thread_id.to_owned());
        vec![AttributedProviderEvent {
            attribution: ProviderEventAttribution::OwningSession,
            event: ProviderEvent::SubagentCompleted {
                subagent_id: ProviderSubagentId::new(child_thread_id),
                status,
            },
        }]
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

/// What the event pump needs to attach a spawned child thread: the transport
/// the Session speaks over, and the working directory its threads run under.
pub(super) struct ChildThreadAttachment {
    pub(super) transport: JsonRpcTransport,
    pub(super) cwd: String,
    pub(super) posture: Arc<std::sync::Mutex<CodexPosture>>,
}

impl ChildThreadAttachment {
    /// Requests the child thread's stream, so its items reach this connection.
    /// The resumed thread's own lineage — the parent it declares — is not
    /// re-checked: the spawn item on the parent's stream already named the
    /// relationship, and it is the spawner's account Suru follows.
    /// Fire-and-forget: a child Codex will not hand over leaves its Subagent's
    /// Session sparse — settled by the lifecycle items the spawner's thread
    /// still carries — rather than failing the parent's.
    fn attach(&self, thread_id: String, results: mpsc::UnboundedSender<(String, ModelId)>) {
        let transport = self.transport.clone();
        let cwd = self.cwd.clone();
        let posture = self.posture.clone();
        tokio::spawn(async move {
            let posture = *posture
                .lock()
                .expect("Codex Session posture lock is not poisoned");
            let Ok(result) = transport
                .request(
                    "thread/resume",
                    &ThreadResumeParams {
                        thread_id: &thread_id,
                        cwd: &cwd,
                        approval_policy: posture.approval_policy(),
                        sandbox: posture.sandbox(),
                    },
                )
                .await
            else {
                return;
            };
            let Ok(attached) = serde_json::from_value::<ThreadConnectionResult>(result) else {
                return;
            };
            if attached.thread.id != thread_id || attached.model.is_empty() {
                return;
            }
            let _ = results.send((thread_id, ModelId::new(attached.model)));
        });
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
                let restated_usage = events
                    .correlation
                    .lock()
                    .expect("Codex native correlation lock is not poisoned")
                    .observe_child_model(&thread_id, model.clone());
                if let Some(usage) = restated_usage {
                    events.pending.push_back(Ok(AttributedProviderEvent {
                        attribution: ProviderEventAttribution::Subagent(
                            ProviderSubagentId::new(thread_id.clone()),
                        ),
                        event: ProviderEvent::Usage { usage, cost: None },
                    }));
                }
                return Some((Ok(AttributedProviderEvent {
                    attribution: ProviderEventAttribution::OwningSession,
                    event: ProviderEvent::SubagentModelChanged {
                        subagent_id: ProviderSubagentId::new(thread_id),
                        model,
                    },
                }), events));
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
/// baseline, and an ambiguous later Model change stops further estimates.
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
            (
                model,
                child.metering.latest.since(child.pricing_baseline),
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

fn project_native_notification(
    correlation: &mut NativeCorrelation,
    notification: NativeNotification,
) -> Result<Vec<AttributedProviderEvent>, ProviderError> {
    match notification {
        NativeNotification::TurnStarted { thread_id, turn_id } => {
            if let Some(turns) = correlation.child_context_turns.get_mut(&thread_id) {
                turns.observe(&turn_id);
            }
            if thread_id != correlation.thread_id
                || correlation.settled_turns.contains(&turn_id)
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
            } else if model.is_empty() || !correlation.children.contains_key(&thread_id) {
                Ok(Vec::new())
            } else {
                let model = ModelId::new(model);
                let restated = correlation.observe_child_model(&thread_id, model.clone());
                let mut projected = vec![AttributedProviderEvent {
                    attribution: ProviderEventAttribution::OwningSession,
                    event: ProviderEvent::SubagentModelChanged {
                        subagent_id: ProviderSubagentId::new(thread_id.clone()),
                        model,
                    },
                }];
                if let Some(usage) = restated {
                    projected.push(AttributedProviderEvent {
                        attribution: ProviderEventAttribution::Subagent(ProviderSubagentId::new(
                            thread_id,
                        )),
                        event: ProviderEvent::Usage { usage, cost: None },
                    });
                }
                Ok(projected)
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
                Ok(correlation
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
                    .unwrap_or_default())
            }
        }
        NativeNotification::CollabCallCompleted {
            thread_id,
            tool,
            status,
            receiver_thread_ids,
            prompt,
            agents_states,
        } => Ok(project_collab_call_completed(
            correlation,
            &thread_id,
            tool,
            status,
            receiver_thread_ids,
            prompt,
            agents_states,
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

/// One step of a spawned agent's lifecycle, as the spawner's thread reports
/// it: a start opens the agent's thread as a Subagent, and the terminal kinds
/// settle it. Interactions revise nothing the row shows. The activity names
/// only the agent, never what it was handed, so a spawn reported this way
/// opens its child Turn with no Delegation.
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
        NativeSubagentActivityKind::Interacted | NativeSubagentActivityKind::Other => Vec::new(),
    }
}

/// A collab tool call completing on a followed thread. A completed spawn opens
/// its receiver threads as Subagents, described by the prompt the call handed
/// them, which is also the Delegation each child Turn opens with; a completed
/// send revises what its receivers' rows say they are doing;
/// and whatever the call was, the terminal lifecycle states it observed settle
/// the Subagents they name — which is how a wait learns of a child finishing.
fn project_collab_call_completed(
    correlation: &mut NativeCorrelation,
    thread_id: &str,
    tool: NativeCollabTool,
    status: NativeCollabCallStatus,
    receiver_thread_ids: Vec<String>,
    prompt: Option<String>,
    agents_states: BTreeMap<String, NativeCollabAgentState>,
) -> Vec<AttributedProviderEvent> {
    let Some(spawner) = correlation.spawner_attribution(thread_id) else {
        return Vec::new();
    };
    let mut projected = Vec::new();
    if status == NativeCollabCallStatus::Completed {
        match tool {
            NativeCollabTool::SpawnAgent => {
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
            NativeCollabTool::SendInput => {
                if let Some(prompt) = prompt.as_ref().filter(|prompt| !prompt.is_empty()) {
                    for receiver in &receiver_thread_ids {
                        projected.extend(correlation.revise_child_description(receiver, prompt));
                    }
                }
            }
            NativeCollabTool::Other => {}
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
    projected
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
/// A followed child thread has no Turn boundaries of its own: its whole run is
/// one Subagent's Turn however many native Turns Codex takes over it. The Cost
/// is left for the Session's rate lookup to fill in, because Codex states no
/// dollar figure of its own.
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
    let Some(child) = correlation.children.get_mut(thread_id) else {
        return if correlation.settled_children.contains(thread_id) {
            attributed(
                &ProviderEventAttribution::Subagent(ProviderSubagentId::new(thread_id)),
                context_event.into_iter().collect(),
            )
        } else {
            Vec::new()
        };
    };
    let usage = child.metering.record(total, CHILD_THREAD_TURN);
    attributed(
        &ProviderEventAttribution::Subagent(ProviderSubagentId::new(thread_id)),
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
    };

    use super::{
        NativeCommandStatus, NativeCorrelation, NativeNotification, NativeSubagentActivityKind,
        NativeTurnFailureKind, ProviderActivityId, ProviderEvent, ProviderEventAttribution,
        is_native_selection_rejection, project_native_notification,
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
}
