mod approvals;
mod questionnaires;

use approvals::{DecisionDeliveries, LiveApprovals};
use questionnaires::{LiveQuestionnaires, QuestionnaireDeliveries};

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt::Display,
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
};

use anyhow::Result;
use futures_util::StreamExt;
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::{Duration, Instant, timeout},
};

use super::{
    AttributedProviderEvent, MeteredCost, ProviderCommandStatus, ProviderError, ProviderEvent,
    ProviderEventAttribution, ProviderEventStream, ProviderFileChangeStatus,
    ProviderPostureApplication, ProviderPrompt, ProviderResumeState, ProviderRuntime,
    ProviderSession, ProviderSessionRequest, ProviderSteerInput, ProviderSubagentId,
    ProviderSubagentStatus, ProviderTurnInput,
};
use crate::ansi::{NormalizedText, ProviderTextNormalizer, normalize_provider_text};
use crate::protocol::{
    Activity, ActivityId, ActivityStatus, AgentIdentity, InterruptOutcome, Message, MessageId,
    MessageRole, MessageStatus, Prompt, PromptDelivery, PromptId, PromptOrder, PromptStatus,
    ProviderId, SessionChange, SessionId, SettingsSnapshot, SkillPromptDelivery, TurnId,
};
use crate::sessions::{
    ApprovalPostureUpdate, DeliveredTurn, DeliveredTurnStatus, InterruptSessionError,
    InterruptTarget, OpenInterventions, OpeningDelegation, ProviderTurnOutcome, SessionStore,
    StoredSubagent, TrailingCommandOutput, command_output_changes, earliest_pending_prompt,
    message_content_changes, reasoning_content_changes,
};
use crate::skill_catalog::{SkillCatalogError, SkillCatalogService};
use crate::storage::StoredSubagentIdentity;

/// The most characters of Provider-sent output Suru stores for one command; the
/// truncation marker Suru appends past the cap is its own and is not charged
/// against it. Command output is machine-generated and can run without bound, so
/// the cap sits well above the output a reader would scroll through while
/// keeping a single Activity small enough to load, project, and re-render on
/// every frame.
const MAX_STORED_COMMAND_OUTPUT_CHARS: usize = 64 * 1024;

/// The most characters of Provider-sent content Suru stores for one agent
/// Message, on the same terms as the command-output cap. Agent prose is written
/// to be read, so real Messages sit orders of magnitude below this; the cap only
/// stops a Provider that streams deltas without end from growing stored content
/// without bound. It is far more generous than the command-output cap because
/// cutting an explanation short costs a reader more than cutting a log short.
const MAX_STORED_MESSAGE_CHARS: usize = 512 * 1024;

/// The most characters of Provider-sent content Suru stores for one Reasoning
/// Activity. Reasoning is prose, so it is capped like an agent Message rather
/// than like a log, but it is a summary of work rather than the answer the
/// reader came for, so the cap sits at the command-output figure: generous next
/// to any real block of Reasoning, and low enough that a Provider reasoning
/// without end cannot grow one Activity without bound.
const MAX_STORED_REASONING_CHARS: usize = 64 * 1024;

/// The failure a routed Subagent Turn settles with when the Provider
/// connection is torn down under it. The connection is those Turns' only
/// event source, so none of them can complete once it is gone.
const SUBAGENT_CONNECTION_LOST_MESSAGE: &str =
    "Provider execution failed: the Provider Session ended before the Subagent's work completed.";

/// The failure a Subagent's child Turn settles with when the Provider reported
/// the Subagent itself failing. The Provider's settle carries no prose, so the
/// child's Transcript states the fact in Suru's own words.
const SUBAGENT_FAILED_MESSAGE: &str =
    "Provider execution failed: the Provider reported this Subagent failing.";

/// Checkout guards taken while admitting a Prompt, held until that Prompt's
/// preparation inherits them or the Prompt leaves the queue.
type CheckoutGuards = Arc<Mutex<HashMap<(SessionId, PromptId), tokio::sync::OwnedMutexGuard<()>>>>;

#[derive(Clone)]
pub(crate) struct ProviderOrchestrator {
    /// Every Provider runtime this server hosts, in the fixed built-in order.
    /// A Session routes to the runtime its Agent Selection names, and a
    /// Session that has no selection yet routes to the first runtime.
    runtimes: Arc<Vec<Arc<dyn ProviderRuntime>>>,
    sessions: SessionStore,
    actors: Arc<Mutex<ProviderActors>>,
    shutdown: watch::Receiver<bool>,
    updates: ProviderUpdateGate,
    shutdown_complete: watch::Sender<bool>,
    /// The effective Settings in force, read whenever a Prompt is about to
    /// reach a Provider. A Provider the user turned off is never asked to begin
    /// a Session, so no process starts on its behalf.
    settings: watch::Receiver<SettingsSnapshot>,
    skill_catalog: SkillCatalogService,
    source_control: crate::source_control::SourceControlService,
    checkout_guards: CheckoutGuards,
    connected_incarnations: Arc<Mutex<HashMap<SessionId, u64>>>,
    checkout_skill_timeout: Duration,
}

struct ProviderActors {
    shutting_down: bool,
    entries: HashMap<SessionId, ProviderActor>,
}

struct ProviderActor {
    commands: mpsc::UnboundedSender<ProviderCommand>,
    shutdown: watch::Sender<bool>,
    /// Taken by whoever waits for this actor to stop, and left registered until
    /// it has: an actor that is still settling its Turn must stay reachable, or
    /// a caller that finds no actor settles that Turn out from under it.
    task: Option<JoinHandle<()>>,
}

struct ProviderShutdown {
    server: watch::Receiver<bool>,
    session: watch::Receiver<bool>,
}

impl ProviderShutdown {
    fn requested(&self) -> bool {
        *self.server.borrow() || *self.session.borrow()
    }

    async fn wait(&mut self) {
        loop {
            if self.requested() {
                return;
            }
            tokio::select! {
                changed = self.server.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
                changed = self.session.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct ProviderUpdateGate {
    accepting: Arc<RwLock<bool>>,
}

impl ProviderUpdateGate {
    pub(crate) fn new() -> Self {
        Self {
            accepting: Arc::new(RwLock::new(true)),
        }
    }

    pub(crate) fn stop(&self) {
        *self
            .accepting
            .write()
            .expect("Provider update gate lock is not poisoned") = false;
    }

    fn apply<T>(&self, update: impl FnOnce() -> T) -> Option<T> {
        let accepting = self
            .accepting
            .read()
            .expect("Provider update gate lock is not poisoned");
        (*accepting).then(update)
    }
}

enum ProviderCommand {
    SubmitDecision {
        target: SessionId,
        id: crate::protocol::ApprovalId,
        decision: crate::protocol::Decision,
        response: oneshot::Sender<Result<(), String>>,
    },
    SubmitQuestionnaire {
        target: SessionId,
        id: crate::protocol::QuestionnaireId,
        submission: crate::protocol::QuestionnaireSubmission,
        response: oneshot::Sender<Result<(), String>>,
    },
    StartPrompt {
        prompt_id: PromptId,
    },
    SteerPrompt,
    /// Stop the Session's work, whatever it is: the active Turn — the
    /// Provider stops its background work first, in the established ordering
    /// — or, with no Turn running, every Subagent still working.
    InterruptSession {
        response: oneshot::Sender<Result<(), InterruptSessionError>>,
    },
    /// Stop the one working Subagent whose child Session is `target`, leaving
    /// everything else running.
    StopSubagent {
        target: SessionId,
        response: oneshot::Sender<Result<(), InterruptSessionError>>,
    },
    UpdateApprovalPosture {
        update: ApprovalPostureUpdate,
        response: oneshot::Sender<Result<ProviderPostureApplication, String>>,
    },
}

struct ConnectedProviderSession {
    incarnation: u64,
    identity: AgentIdentity,
    session: Arc<dyn ProviderSession>,
    events: ProviderEventStream,
}

struct ProviderSessionContext {
    source_control: crate::source_control::SourceControlService,
    checkout_guards: CheckoutGuards,
    connected_incarnations: Arc<Mutex<HashMap<SessionId, u64>>>,
    checkout_skill_timeout: Duration,
    runtime: Arc<dyn ProviderRuntime>,
    sessions: SessionStore,
    skill_catalog: SkillCatalogService,
    session_id: SessionId,
    execution_directory: PathBuf,
    updates: ProviderUpdateGate,
    settings: watch::Receiver<SettingsSnapshot>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ContinuationExecution {
    LateOutput,
    ProviderTurn,
}

struct ActiveProviderTurn {
    turn_id: TurnId,
    approvals: LiveApprovals,
    questionnaires: LiveQuestionnaires,
    /// Whether this Turn is a Continuation — the one kind of Turn no Prompt
    /// began, opened by this actor for output that arrived after the previous
    /// Turn settled. The next delivered Prompt settles it rather than
    /// steering it. A Provider-owned Continuation must first interrupt its
    /// native Turn; late output alone has no Provider Turn to interrupt.
    continuation: Option<ContinuationExecution>,
    streaming_message: Option<ActiveProviderMessage>,
    interruption_acknowledged: bool,
    command_activities: HashMap<super::ProviderActivityId, ActiveProviderCommand>,
    file_change_activities: HashMap<super::ProviderActivityId, ActivityId>,
    reasoning_activities: HashMap<super::ProviderActivityId, ActiveProviderReasoning>,
}

/// An Agent Message the Provider is still streaming. Its normalizer buffers no
/// unterminated line, so settling the Turn may drop it; give this one line
/// overwrite and the settle paths must gain a place to carry its drained
/// content, the way [`TrailingCommandOutput`] carries a command's. The store
/// completes the Message from its own snapshot and cannot reach what the
/// normalizer holds.
struct ActiveProviderMessage {
    id: MessageId,
    normalizer: ProviderTextNormalizer,
}

struct ActiveProviderCommand {
    id: ActivityId,
    output_normalizer: ProviderTextNormalizer,
}

/// A Reasoning block the Provider is still streaming. Suru times the block
/// itself rather than asking every Provider to report a duration, so `started`
/// is the moment this actor admitted the block and the elapsed time it yields
/// is what the Transcript reports. Like an Agent Message, its normalizer holds
/// back no unterminated line, so settling the Turn drops nothing.
struct ActiveProviderReasoning {
    id: ActivityId,
    started: Instant,
    normalizer: ProviderTextNormalizer,
}

impl ActiveProviderTurn {
    fn new(turn_id: TurnId) -> Self {
        Self {
            turn_id,
            approvals: LiveApprovals::default(),
            questionnaires: LiveQuestionnaires::default(),
            continuation: None,
            streaming_message: None,
            interruption_acknowledged: false,
            command_activities: HashMap::new(),
            file_change_activities: HashMap::new(),
            reasoning_activities: HashMap::new(),
        }
    }

    fn new_continuation(turn_id: TurnId) -> Self {
        Self {
            continuation: Some(ContinuationExecution::LateOutput),
            ..Self::new(turn_id)
        }
    }

    /// Whether the Turn already has a stream open under this Provider Activity
    /// identity, whatever kind of stream it is. Identities are the Provider's
    /// to choose and are unique across kinds, so every stream that opens one
    /// asks here rather than each restating the list of kinds.
    fn claims_activity(&self, activity_id: &super::ProviderActivityId) -> bool {
        self.command_activities.contains_key(activity_id)
            || self.file_change_activities.contains_key(activity_id)
            || self.reasoning_activities.contains_key(activity_id)
    }

    fn tool_activity_id(&self, activity_id: &super::ProviderActivityId) -> Option<ActivityId> {
        self.command_activities
            .get(activity_id)
            .map(|command| command.id)
            .or_else(|| self.file_change_activities.get(activity_id).copied())
    }

    /// Drains what the Turn's streams hold that only this actor knows: the
    /// unterminated line each command normalizer buffered. The Session store
    /// settles the streams themselves from its own snapshot, so forgetting a
    /// stream here costs its pending line, not its Transcript row.
    fn take_trailing_output(&mut self) -> TrailingCommandOutput {
        self.command_activities
            .drain()
            .map(|(_, mut command)| (command.id, command.output_normalizer.finish()))
            .collect()
    }
}

/// Where the connection's Subagent-attributed events land: for each Subagent
/// the Provider has named, the Session that is that Subagent's own and the
/// Turn state its stream projects into. Each stretch of a Subagent's work —
/// its spawn, and each resume after it settled — establishes a route into the
/// Turn that stretch works in, and the stretch's settle drops it. Routes live
/// and die with the Provider connection, because the identities are the
/// connection's to mint. An event attributed to a Subagent with no route
/// lands nowhere.
///
/// The rows live beside the routes rather than inside any Turn's state,
/// because a Subagent may outlive the Turn that delegated to it (ADR 0015):
/// its row must still be reachable when the Provider settles it after that
/// Turn has.
struct SubagentRoutes {
    /// The Provider whose connection mints every identity here, stored with
    /// each Subagent's Session so only that Provider's resume can find it.
    provider: ProviderId,
    routes: HashMap<ProviderSubagentId, SubagentRoute>,
    /// Context measurements can arrive after the child's output route settles.
    context_routes: HashMap<ProviderSubagentId, (SessionId, TurnId)>,
    /// The row each working Subagent's current stretch stands as.
    rows: HashMap<ProviderSubagentId, SubagentRow>,
    /// Every Subagent the connection has named, retained for its lifetime —
    /// and, since each identity is stored with its Subagent's Session, every
    /// one it named before a restart, relearned when the connection resumes:
    /// a resume finds the Session it continues here, and delayed
    /// attach/metadata replies can still identify a child after it settles.
    identities: HashMap<ProviderSubagentId, SubagentIdentity>,
    /// The Subagents Suru itself settled by stopping them. Their rows and
    /// routes are gone, but the Provider was not the one to close them, so
    /// its own account of their end — the settle it still owes, the progress
    /// it had in flight — can trail in afterwards and must read as a late
    /// echo to discard rather than as an event it never spawned.
    stopped: HashSet<ProviderSubagentId>,
    /// Whether a Subagent has settled since a Turn last began. The output a
    /// completion provokes can arrive only after the settle — Claude notifies
    /// a task's end before its loop wakes to deliver the outcome — so every
    /// settle leaves a Continuation owed to whatever that output turns out to
    /// be. Any Turn beginning clears it, because from then on such output has
    /// a Turn to land in.
    late_settle_owes_continuation: bool,
}

struct SubagentRoute {
    session_id: SessionId,
    turn: ActiveProviderTurn,
}

/// One stretch of a Subagent's work as its row in the delegating Transcript.
/// Suru times the stretch itself, the way it times a Reasoning block:
/// `started` is the moment the spawn or resume was admitted, and the elapsed
/// time it yields is the duration the settled row reports — this stretch's
/// alone, however long the Subagent worked before it.
struct SubagentRow {
    /// The Session whose Transcript holds the row — the one whose Turn
    /// delegated the stretch: the Subagent's spawner, or for a resume
    /// whichever Agent sent it, a sibling Subagent's included.
    owner_session_id: SessionId,
    activity_id: ActivityId,
    started: Instant,
}

/// What the connection knows of one Subagent, working or not: the Session
/// that is its own, the Session that spawned it — its place in the tree,
/// whoever resumes it later — the name every row of it carries, and where
/// its latest stretch of work stands. A Subagent relearned from the store has
/// no stretch this connection began until a resume begins one.
#[derive(Clone)]
struct SubagentIdentity {
    session_id: SessionId,
    spawner: SessionId,
    name: String,
    stretch: Option<SubagentStretch>,
}

/// Where one stretch of a Subagent's work stands: the Turn it works in within
/// the Subagent's own Session, and its row in the delegating Session. A
/// resume moves a Subagent onto a new stretch, and its Model evidence follows
/// — the spawn's Turn and row keep what they settled with.
#[derive(Clone, Copy)]
struct SubagentStretch {
    turn_id: TurnId,
    owner_session_id: SessionId,
    activity_id: ActivityId,
}

impl SubagentRoutes {
    fn new(provider: ProviderId) -> Self {
        Self {
            provider,
            routes: HashMap::new(),
            context_routes: HashMap::new(),
            rows: HashMap::new(),
            identities: HashMap::new(),
            stopped: HashSet::new(),
            late_settle_owes_continuation: false,
        }
    }

    /// Relearns the Subagents the store holds by this connection's identities,
    /// for a connection that resumes a conversation this process may never
    /// have run — after a restart above all — so a resume naming one finds
    /// the Session it continues. What the connection already knows, it keeps:
    /// the stretch it is working in is this process's alone.
    fn restore(&mut self, stored: Vec<StoredSubagent>) {
        for subagent in stored {
            self.identities
                .entry(subagent.subagent_id)
                .or_insert(SubagentIdentity {
                    session_id: subagent.session_id,
                    spawner: subagent.spawner,
                    name: subagent.name,
                    stretch: None,
                });
        }
    }

    fn live_approvals(&mut self, session_id: SessionId) -> Option<&mut LiveApprovals> {
        self.routes
            .values_mut()
            .find(|route| route.session_id == session_id)
            .map(|route| &mut route.turn.approvals)
    }

    fn live_questionnaires(&mut self, session_id: SessionId) -> Option<&mut LiveQuestionnaires> {
        self.routes
            .values_mut()
            .find(|route| route.session_id == session_id)
            .map(|route| &mut route.turn.questionnaires)
    }

    /// Whether output arriving with no Turn active is owed a Continuation
    /// rather than being stray: some Subagent is still working, or one just
    /// settled and its provoked output is still to come.
    fn owes_continuation(&self) -> bool {
        self.late_settle_owes_continuation || !self.rows.is_empty() || !self.routes.is_empty()
    }

    /// Projects one Subagent-attributed event into the Session its route
    /// names, through the same projection and commit path the owning Session's
    /// events take. The route is taken out while its event projects, because a
    /// nested spawn inserts the grandchild's route into this same table mid-
    /// projection. A Terminal projection settles the routed Turn, so the route
    /// has nothing left to receive and stays out.
    fn project_event(
        &mut self,
        sessions: &SessionStore,
        updates: &ProviderUpdateGate,
        next_agent: &AgentIdentity,
        subagent: &ProviderSubagentId,
        event: ProviderEvent,
    ) {
        if let ProviderEvent::ContextFill { report } = event {
            if let Some(&(session_id, turn_id)) = self.context_routes.get(subagent) {
                let _ = updates.apply(|| sessions.report_context_fill(session_id, turn_id, report));
            }
            return;
        }
        let Some(mut route) = self.routes.remove(subagent) else {
            return;
        };
        let projection = project_provider_event(
            sessions,
            updates,
            route.session_id,
            &mut route.turn,
            self,
            next_agent,
            event,
        );
        if !matches!(projection, ProviderEventProjection::Terminal) {
            self.routes.insert(subagent.clone(), route);
        }
    }

    /// Fails every routed Turn and open row, and forgets both. Called
    /// wherever the Provider connection is torn down: the routes' Turn state
    /// is this actor's only handle on those streams, and no more of their
    /// events — the settles the rows were owed included — can arrive once the
    /// connection is gone.
    fn fail_all(
        &mut self,
        sessions: &SessionStore,
        updates: &ProviderUpdateGate,
        message: &str,
        interventions: OpenInterventions,
    ) {
        self.context_routes.clear();
        for (_, mut route) in self.routes.drain() {
            fail_active_turn(
                sessions,
                updates,
                route.session_id,
                &mut route.turn,
                message.to_owned(),
                interventions,
            );
        }
        for (_, row) in self.rows.drain() {
            let _ = updates.apply(|| {
                sessions.publish_agent_output(
                    row.owner_session_id,
                    SessionChange::SubagentStatusChanged {
                        activity_id: row.activity_id,
                        status: ActivityStatus::Failed,
                        // The Provider never reported this Subagent settling,
                        // so there is no duration to record.
                        duration_ms: None,
                    },
                )
            });
        }
    }

    /// Whether Suru itself stopped this Subagent, which is what makes the
    /// Provider's later events for it late echoes rather than events it never
    /// spawned.
    fn was_stopped(&self, subagent: &ProviderSubagentId) -> bool {
        self.stopped.contains(subagent)
    }

    /// The working Subagent whose child Session is `target`, resolved for a
    /// stop request that names the Session rather than the Provider's own
    /// identity. `None` once the Subagent has settled — there is nothing left
    /// to stop.
    fn subagent_for_session(&self, target: SessionId) -> Option<ProviderSubagentId> {
        self.routes
            .iter()
            .find(|(_, route)| route.session_id == target)
            .map(|(subagent, _)| subagent.clone())
    }

    /// Settles one stopped Subagent — and every Subagent below it, since
    /// stopping a delegation stops whatever it delegated in turn — as
    /// Interrupted: the Turn its current stretch works in closes with the
    /// stop, that stretch's row records the duration Suru timed, and the
    /// identities join the stopped set so the Provider's trailing account of
    /// them is discarded.
    fn settle_stopped(
        &mut self,
        sessions: &SessionStore,
        updates: &ProviderUpdateGate,
        subagent: &ProviderSubagentId,
    ) {
        let mut targets = vec![subagent.clone()];
        while let Some(subagent) = targets.pop() {
            let Some(row) = self.rows.remove(&subagent) else {
                continue;
            };
            let route = self.routes.remove(&subagent);
            self.stopped.insert(subagent);
            if let Some(mut route) = route {
                // Below it are the Subagents it spawned. A Subagent it only
                // resumed stands elsewhere in the tree, so its row here does
                // not make it one of them.
                targets.extend(
                    self.rows
                        .keys()
                        .filter(|working| {
                            self.identities
                                .get(*working)
                                .is_some_and(|identity| identity.spawner == route.session_id)
                        })
                        .cloned(),
                );
                let trailing_output = route.turn.take_trailing_output();
                let _ = updates.apply(|| {
                    sessions.finish_provider_turn(
                        route.session_id,
                        route.turn.turn_id,
                        ProviderTurnOutcome::Interrupted { trailing_output },
                    )
                });
            }
            let duration_ms = u64::try_from(row.started.elapsed().as_millis()).ok();
            let _ = updates.apply(|| {
                sessions.publish_agent_output(
                    row.owner_session_id,
                    SessionChange::SubagentStatusChanged {
                        activity_id: row.activity_id,
                        status: ActivityStatus::Interrupted,
                        duration_ms,
                    },
                )
            });
        }
    }

    /// Settles every working Subagent as stopped, for the interrupt that
    /// reaches them all. Whatever output a settle was still owed is owed no
    /// longer — an interrupted stream's trailing output is discarded, as it
    /// always was — so this also stands down the Continuation.
    fn stop_all(&mut self, sessions: &SessionStore, updates: &ProviderUpdateGate) {
        let working = self.rows.keys().cloned().collect::<Vec<_>>();
        for subagent in working {
            self.settle_stopped(sessions, updates, &subagent);
        }
        self.late_settle_owes_continuation = false;
    }

    /// Applies a Provider's description update to the row it names, or `None`
    /// when no open row carries the identity — the caller decides whether an
    /// unknown identity is an invalid event or a late echo to discard.
    fn update_row(
        &self,
        sessions: &SessionStore,
        subagent: &ProviderSubagentId,
        description: &str,
    ) -> Option<anyhow::Result<()>> {
        let row = self.rows.get(subagent)?;
        Some(
            sessions
                .publish_agent_output(
                    row.owner_session_id,
                    SessionChange::SubagentDescriptionChanged {
                        activity_id: row.activity_id,
                        description: normalize_provider_text(description),
                    },
                )
                .map(|_| ()),
        )
    }

    /// Applies Provider-confirmed identity to both places a reader needs it:
    /// the Turn the Subagent's current stretch works in and the row that
    /// stretch stands as. Requested parent options are deliberately dropped
    /// because they are not child evidence.
    fn update_model(
        &self,
        sessions: &SessionStore,
        subagent: &ProviderSubagentId,
        next_agent: &AgentIdentity,
        model: crate::protocol::ModelId,
    ) -> Option<anyhow::Result<()>> {
        let identity = self.identities.get(subagent)?;
        let stretch = identity.stretch?;
        let mut agent = next_agent.clone();
        agent.selection.model = model.clone();
        agent.selection.options.clear();
        Some((|| {
            sessions.publish(
                identity.session_id,
                vec![SessionChange::SubagentAgentChanged {
                    turn_id: stretch.turn_id,
                    agent,
                }],
            )?;
            sessions.publish(
                stretch.owner_session_id,
                vec![SessionChange::SubagentModelChanged {
                    activity_id: stretch.activity_id,
                    model,
                }],
            )?;
            Ok(())
        })())
    }

    /// Opens a Subagent the Provider spawned: its own Session, titled from
    /// the spawn, opened by the spawn's Delegation and holding the Provider's
    /// identity for it, and the row that stands for its first stretch of work
    /// in the delegating Turn.
    fn spawn(
        &mut self,
        sessions: &SessionStore,
        delegating: (SessionId, TurnId),
        subagent: ProviderSubagentId,
        name: &str,
        description: &str,
        delegation: Option<&str>,
    ) -> anyhow::Result<()> {
        if self.rows.contains_key(&subagent) || self.routes.contains_key(&subagent) {
            return Err(anyhow::anyhow!(
                "Provider reused an active Subagent identity"
            ));
        }
        let name = normalize_provider_text(name);
        let description = normalize_provider_text(description);
        let identity = StoredSubagentIdentity {
            provider: self.provider.clone(),
            subagent_id: subagent.clone(),
        };
        let spawned = sessions.create_subagent(
            delegating.0,
            identity,
            &name,
            &description,
            delegation.and_then(delegation_text),
        )?;
        let activity_id =
            add_subagent_row(sessions, delegating, &name, description, spawned.session_id)?;
        self.open_stretch(
            subagent,
            (spawned.session_id, delegating.0),
            name,
            SubagentStretch {
                turn_id: spawned.turn_id,
                owner_session_id: delegating.0,
                activity_id,
            },
        );
        Ok(())
    }

    /// Resumes a settled Subagent the Provider names again: the next Turn in
    /// the Subagent's own Session, opened by the resume's Delegation, and a
    /// new row in the delegating Turn leading into that same Session (ADR
    /// 0031). The rows before it stay as they settled, and the Session keeps
    /// its Title and the name its spawn gave it. A Continuation still open in
    /// the Subagent's Session settles first, with the output this actor still
    /// held for it, because the resume begins a Turn of its own.
    ///
    /// A resume naming a Subagent neither this connection nor the store knows
    /// — spawned before its identity was stored, say — is recorded as a new
    /// Subagent under `name`, with a row and a Session of its own opened by
    /// the resume's Delegation, rather than lost: its work still lands
    /// somewhere a reader can find it.
    fn resume(
        &mut self,
        sessions: &SessionStore,
        delegating: (SessionId, TurnId),
        subagent: ProviderSubagentId,
        name: &str,
        description: &str,
        delegation: Option<&str>,
    ) -> anyhow::Result<()> {
        if self.rows.contains_key(&subagent) {
            return Err(anyhow::anyhow!(
                "Provider resumed a Subagent that is still working"
            ));
        }
        let Some(identity) = self.identities.get(&subagent) else {
            tracing::info!(
                subagent = subagent.as_str(),
                "recording a resume of a Subagent Suru holds no Session for as a new Subagent"
            );
            return self.spawn(
                sessions,
                delegating,
                subagent,
                name,
                description,
                delegation,
            );
        };
        let (session_id, spawner, name) =
            (identity.session_id, identity.spawner, identity.name.clone());
        if let Some(mut continuation) = self.routes.remove(&subagent) {
            let trailing_output = continuation.turn.take_trailing_output();
            sessions.finish_provider_turn(
                session_id,
                continuation.turn.turn_id,
                ProviderTurnOutcome::Completed { trailing_output },
            )?;
        }
        let turn_id = sessions.begin_subagent_turn(
            session_id,
            delegation
                .and_then(delegation_text)
                .map(|text| OpeningDelegation {
                    delegating_session: delegating.0,
                    text,
                }),
        )?;
        let activity_id = add_subagent_row(
            sessions,
            delegating,
            &name,
            normalize_provider_text(description),
            session_id,
        )?;
        self.open_stretch(
            subagent,
            (session_id, spawner),
            name,
            SubagentStretch {
                turn_id,
                owner_session_id: delegating.0,
                activity_id,
            },
        );
        Ok(())
    }

    /// Routes a Subagent's events into the stretch just begun: its row is
    /// timed from now, its output and Context Fill land in the stretch's
    /// Turn, and its Model evidence follows it there. A Subagent Suru stopped
    /// before is working again, so what the Provider says of it from here on
    /// is no late echo.
    fn open_stretch(
        &mut self,
        subagent: ProviderSubagentId,
        (session_id, spawner): (SessionId, SessionId),
        name: String,
        stretch: SubagentStretch,
    ) {
        self.rows.insert(
            subagent.clone(),
            SubagentRow {
                owner_session_id: stretch.owner_session_id,
                activity_id: stretch.activity_id,
                started: Instant::now(),
            },
        );
        self.context_routes
            .insert(subagent.clone(), (session_id, stretch.turn_id));
        self.routes.insert(
            subagent.clone(),
            SubagentRoute {
                session_id,
                turn: ActiveProviderTurn::new(stretch.turn_id),
            },
        );
        self.stopped.remove(&subagent);
        self.identities.insert(
            subagent,
            SubagentIdentity {
                session_id,
                spawner,
                name,
                stretch: Some(stretch),
            },
        );
    }

    /// Settles one Subagent's current stretch on the Provider's own settle
    /// signal: the Turn it worked in closes along with its row, because the
    /// Provider's boundary for the stretch is one signal and no further event
    /// of the child's is owed once it has passed — until a resume begins the
    /// next. Returns `None` when no open row carries the identity, on the
    /// same terms as [`Self::update_row`].
    fn settle_subagent(
        &mut self,
        sessions: &SessionStore,
        subagent: &ProviderSubagentId,
        status: ProviderSubagentStatus,
    ) -> Option<anyhow::Result<()>> {
        let row = self.rows.remove(subagent)?;
        let route = self.routes.remove(subagent);
        self.late_settle_owes_continuation = true;
        Some((|| {
            if let Some(mut route) = route {
                let outcome = match status {
                    ProviderSubagentStatus::Completed => ProviderTurnOutcome::Completed {
                        trailing_output: route.turn.take_trailing_output(),
                    },
                    ProviderSubagentStatus::Failed => ProviderTurnOutcome::Failed {
                        trailing_output: route.turn.take_trailing_output(),
                        message: SUBAGENT_FAILED_MESSAGE.to_owned(),
                    },
                    ProviderSubagentStatus::Interrupted => ProviderTurnOutcome::Interrupted {
                        trailing_output: route.turn.take_trailing_output(),
                    },
                };
                sessions.finish_provider_turn(route.session_id, route.turn.turn_id, outcome)?;
            }
            let duration_ms = u64::try_from(row.started.elapsed().as_millis()).ok();
            sessions.publish_agent_output(
                row.owner_session_id,
                SessionChange::SubagentStatusChanged {
                    activity_id: row.activity_id,
                    status: match status {
                        ProviderSubagentStatus::Completed => ActivityStatus::Completed,
                        ProviderSubagentStatus::Failed => ActivityStatus::Failed,
                        ProviderSubagentStatus::Interrupted => ActivityStatus::Interrupted,
                    },
                    duration_ms,
                },
            )?;
            Ok(())
        })())
    }
}

enum ProviderInput {
    Command(Option<ProviderCommand>),
    Event(Option<Result<AttributedProviderEvent, super::ProviderError>>),
}

enum ProviderEventProjection {
    Continue,
    Terminal,
}

impl ProviderOrchestrator {
    // One parameter per server-wide service the orchestrator coordinates; each
    // is shared with other owners, so none is built here.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        runtimes: Vec<Arc<dyn ProviderRuntime>>,
        sessions: SessionStore,
        shutdown: watch::Receiver<bool>,
        updates: ProviderUpdateGate,
        settings: watch::Receiver<SettingsSnapshot>,
        skill_catalog: SkillCatalogService,
        source_control: crate::source_control::SourceControlService,
        checkout_skill_timeout: Duration,
    ) -> Self {
        assert!(
            !runtimes.is_empty(),
            "Provider orchestration requires at least one hosted runtime"
        );
        let (shutdown_complete, _) = watch::channel(false);
        Self {
            runtimes: Arc::new(runtimes),
            sessions,
            actors: Arc::new(Mutex::new(ProviderActors {
                shutting_down: false,
                entries: HashMap::new(),
            })),
            shutdown,
            updates,
            shutdown_complete,
            settings,
            skill_catalog,
            source_control,
            checkout_skill_timeout,
            checkout_guards: Default::default(),
            connected_incarnations: Default::default(),
        }
    }

    pub(crate) fn hold_checkout_guard(
        &self,
        session: SessionId,
        prompt: PromptId,
        guard: tokio::sync::OwnedMutexGuard<()>,
    ) {
        self.checkout_guards
            .lock()
            .unwrap()
            .insert((session, prompt), guard);
    }
    pub(crate) fn connected_incarnation(&self, session: SessionId) -> Option<u64> {
        self.connected_incarnations
            .lock()
            .unwrap()
            .get(&session)
            .copied()
    }

    pub(crate) fn has_session_actor(&self, id: SessionId) -> bool {
        self.actors.lock().unwrap().entries.contains_key(&id)
    }

    pub(crate) fn open_session(
        &self,
        session_id: SessionId,
        execution_directory: PathBuf,
        prompt_id: PromptId,
    ) {
        if let Ok(commands_tx) =
            self.actor_commands_or_fail_prompt(session_id, execution_directory, prompt_id)
            && commands_tx
                .send(ProviderCommand::StartPrompt { prompt_id })
                .is_err()
        {
            let _ = self.fail_prompt(
                session_id,
                prompt_id,
                "Session Provider actor stopped unexpectedly".to_owned(),
            );
        }
    }

    fn is_enabled(&self, provider: &ProviderId) -> bool {
        self.settings.borrow().settings.provider_enabled(provider)
    }

    /// Settles a Prompt's Turn as failed before it ever reaches a Provider, and
    /// answers the caller with why nothing was scheduled.
    fn fail_prompt(
        &self,
        session_id: SessionId,
        prompt_id: PromptId,
        message: String,
    ) -> anyhow::Error {
        self.checkout_guards
            .lock()
            .unwrap()
            .remove(&(session_id, prompt_id));
        let reported = message.clone();
        let _ = self.updates.apply(|| {
            self.sessions.deliver_prompt(
                session_id,
                prompt_id,
                None,
                DeliveredTurnStatus::Failed { message },
            )
        });
        anyhow::anyhow!(reported)
    }

    /// Why the Session's Provider cannot begin another Turn, when the user has
    /// turned it off. Enablement is the one condition that has to reach a
    /// Session already running on its Provider, so it is asked separately from
    /// [`Self::resolve_runtime`], which only answers where a new actor would
    /// run. The fix is inside Suru rather than outside it, so the message names
    /// the Setting rather than reading like an unavailability reason.
    ///
    /// Steering deliberately does not ask: a steer Prompt joins the Turn
    /// already under way rather than beginning another, and Enablement governs
    /// what Suru does next rather than what it is doing.
    fn disabled_provider_failure(&self, session_id: SessionId) -> Option<String> {
        let provider = self.sessions.provider(session_id)?;
        if self.is_enabled(&provider) {
            return None;
        }
        Some(format!(
            "Provider `{provider}` is disabled: turn `provider.{provider}.enabled` \
             back on in Settings to use it again."
        ))
    }

    /// The Session's Provider runtime under this server's hosted set. A Session
    /// keeps the Provider it was selected with (ADR-0005); one that has no
    /// Agent Selection yet takes the first *enabled* runtime of the built-in
    /// default order, so Enablement governs that order rather than merely
    /// filtering what it produced.
    fn resolve_runtime(&self, session_id: SessionId) -> Result<Arc<dyn ProviderRuntime>, String> {
        let Some(provider) = self.sessions.provider(session_id) else {
            return self
                .runtimes
                .iter()
                .find(|runtime| self.is_enabled(&runtime.provider_id()))
                .cloned()
                .ok_or_else(|| {
                    "Provider startup failed: every Provider is disabled. \
                     Turn one back on in Settings to start a Turn."
                        .to_owned()
                });
        };
        self.runtimes
            .iter()
            .find(|runtime| runtime.provider_id() == provider)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "Provider startup failed: Provider `{provider}` is not hosted by this server."
                )
            })
    }

    /// Returns the Session's actor, or settles the Prompt's Turn as failed when
    /// its Provider cannot take it: a Session the user has turned its Provider
    /// off for, or one stored under a Provider this server no longer hosts.
    /// Either failure leaves a record in the Transcript, which a silently
    /// rejected Prompt would not.
    ///
    /// Enablement is asked before the actor is looked up, because an existing
    /// actor is exactly the case a disable has to reach: the Turn it is running
    /// is left to Settle, and this — the next Prompt — is what fails. Every
    /// other condition is about where a *new* actor would run, so an actor that
    /// survives the Enablement check keeps the Provider conversation it already
    /// owns (ADR-0005).
    fn actor_commands_or_fail_prompt(
        &self,
        session_id: SessionId,
        execution_directory: PathBuf,
        prompt_id: PromptId,
    ) -> Result<mpsc::UnboundedSender<ProviderCommand>> {
        if let Some(message) = self.disabled_provider_failure(session_id) {
            return Err(self.fail_prompt(session_id, prompt_id, message));
        }
        if let Some(commands) = self
            .actors
            .lock()
            .expect("Provider actor registry lock is not poisoned")
            .entries
            .get(&session_id)
            .map(|actor| actor.commands.clone())
        {
            return Ok(commands);
        }
        let runtime = match self.resolve_runtime(session_id) {
            Ok(runtime) => runtime,
            Err(message) => return Err(self.fail_prompt(session_id, prompt_id, message)),
        };
        self.get_or_spawn_actor_commands(session_id, execution_directory, runtime)
            .map_err(|error| self.fail_prompt(session_id, prompt_id, error.to_string()))
    }

    fn get_or_spawn_actor_commands(
        &self,
        session_id: SessionId,
        execution_directory: PathBuf,
        runtime: Arc<dyn ProviderRuntime>,
    ) -> Result<mpsc::UnboundedSender<ProviderCommand>> {
        let (commands_tx, commands_rx) = mpsc::unbounded_channel();
        let mut actors = self
            .actors
            .lock()
            .expect("Provider actor registry lock is not poisoned");
        if let Some(actor) = actors.entries.get(&session_id) {
            return Ok(actor.commands.clone());
        }
        if actors.shutting_down || *self.shutdown.borrow() {
            return Err(anyhow::anyhow!("Provider orchestrator is shutting down"));
        }
        let sessions = self.sessions.clone();
        let (session_shutdown, session_shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(run_provider_session(
            ProviderSessionContext {
                source_control: self.source_control.clone(),
                checkout_guards: self.checkout_guards.clone(),
                connected_incarnations: self.connected_incarnations.clone(),
                checkout_skill_timeout: self.checkout_skill_timeout,
                runtime,
                sessions,
                skill_catalog: self.skill_catalog.clone(),
                session_id,
                execution_directory,
                updates: self.updates.clone(),
                settings: self.settings.clone(),
            },
            commands_rx,
            ProviderShutdown {
                server: self.shutdown.clone(),
                session: session_shutdown_rx,
            },
        ));
        actors.entries.insert(
            session_id,
            ProviderActor {
                commands: commands_tx.clone(),
                shutdown: session_shutdown,
                task: Some(task),
            },
        );
        Ok(commands_tx)
    }

    pub(crate) fn schedule_prompt(&self, session_id: SessionId, prompt_id: PromptId) -> Result<()> {
        let execution_directory =
            self.sessions
                .execution_directory(session_id)
                .ok_or_else(|| {
                    self.checkout_guards
                        .lock()
                        .unwrap()
                        .remove(&(session_id, prompt_id));
                    anyhow::anyhow!("Session does not exist on this server instance")
                })?;
        let Ok(commands) =
            self.actor_commands_or_fail_prompt(session_id, execution_directory, prompt_id)
        else {
            // The Prompt's Turn was already settled as failed; scheduling has
            // nothing left to deliver.
            return Ok(());
        };
        if commands
            .send(ProviderCommand::StartPrompt { prompt_id })
            .is_err()
        {
            let _ = self.fail_prompt(
                session_id,
                prompt_id,
                "Session Provider actor stopped unexpectedly".to_owned(),
            );
        }
        Ok(())
    }

    pub(crate) fn schedule_steer(&self, session_id: SessionId) -> Result<()> {
        self.schedule(session_id, ProviderCommand::SteerPrompt)
    }

    fn schedule(&self, session_id: SessionId, command: ProviderCommand) -> Result<()> {
        let actor = self
            .actors
            .lock()
            .expect("Provider actor registry lock is not poisoned")
            .entries
            .get(&session_id)
            .map(|actor| actor.commands.clone())
            .ok_or_else(|| anyhow::anyhow!("Session has no Provider actor"))?;
        actor
            .send(command)
            .map_err(|_| anyhow::anyhow!("Session Provider actor stopped unexpectedly"))
    }

    pub(crate) async fn submit_questionnaire(
        &self,
        session_id: SessionId,
        id: crate::protocol::QuestionnaireId,
        submission: crate::protocol::QuestionnaireSubmission,
    ) -> Result<(), String> {
        let actor_id = self.sessions.actor_session(session_id);
        let (response, received) = oneshot::channel();
        self.schedule(
            actor_id,
            ProviderCommand::SubmitQuestionnaire {
                target: session_id,
                id,
                submission,
                response,
            },
        )
        .map_err(|_| "Questionnaire is unavailable".to_owned())?;
        received
            .await
            .map_err(|_| "Questionnaire delivery could not be confirmed".to_owned())?
    }

    pub(crate) async fn submit_decision(
        &self,
        session_id: SessionId,
        id: crate::protocol::ApprovalId,
        decision: crate::protocol::Decision,
    ) -> Result<(), String> {
        let actor_id = self.sessions.actor_session(session_id);
        let (response, received) = oneshot::channel();
        self.schedule(
            actor_id,
            ProviderCommand::SubmitDecision {
                target: session_id,
                id,
                decision,
                response,
            },
        )
        .map_err(|_| "Approval is unavailable".to_owned())?;
        received
            .await
            .map_err(|_| "Decision delivery could not be confirmed".to_owned())?
    }

    pub(crate) async fn update_approval_posture(
        &self,
        update: ApprovalPostureUpdate,
    ) -> Result<ProviderPostureApplication, String> {
        let session_id = update.session_id;
        let actor_id = self.sessions.actor_session(session_id);
        let commands = self
            .actors
            .lock()
            .expect("Provider actor registry lock is not poisoned")
            .entries
            .get(&actor_id)
            .map(|actor| actor.commands.clone());
        let Some(commands) = commands else {
            self.sessions.mark_approval_posture_application(
                update,
                crate::protocol::ApprovalPostureApplication::Applied,
            );
            return Ok(ProviderPostureApplication::Applied);
        };
        let (response, received) = oneshot::channel();
        if commands
            .send(ProviderCommand::UpdateApprovalPosture { update, response })
            .is_err()
        {
            self.sessions.mark_approval_posture_application(
                update,
                crate::protocol::ApprovalPostureApplication::Failed,
            );
            return Err("Approval Posture update could not reach the Provider".to_owned());
        }
        match received.await {
            Ok(result) => result,
            Err(_) => {
                self.sessions.mark_approval_posture_application(
                    update,
                    crate::protocol::ApprovalPostureApplication::Failed,
                );
                Err("Approval Posture update could not be confirmed".to_owned())
            }
        }
    }

    /// Stops what a Session is doing, whatever that is: the active Turn along
    /// with the Subagents it spawned, or — with no Turn running — the
    /// Subagents alone. Interrupting a Subagent's own Session stops that one
    /// Subagent, through the Provider connection its root ancestor owns.
    pub(crate) async fn interrupt_session(
        &self,
        session_id: SessionId,
    ) -> Result<InterruptOutcome, InterruptSessionError> {
        let actor_commands = |actor_id: SessionId| {
            self.actors
                .lock()
                .expect("Provider actor registry lock is not poisoned")
                .entries
                .get(&actor_id)
                .map(|actor| actor.commands.clone())
        };
        let stopped: Result<(), InterruptSessionError> = match self
            .sessions
            .interrupt_or_withdraw(session_id)?
        {
            // The startup this Prompt set going is left to abandon itself:
            // the actor re-reads the Prompt before it delivers one, and a
            // Cancelled Prompt is one it declines to deliver. A Provider
            // connection that finished starting stays where it is, idle,
            // exactly as a Session between Turns keeps its connection.
            InterruptTarget::WithdrewPrompt(prompt) => {
                return Ok(InterruptOutcome::WithdrewPrompt { prompt: *prompt });
            }
            InterruptTarget::Turn(turn) => {
                let actor = actor_commands(session_id).ok_or_else(|| {
                    self.fail_unavailable_interruption(
                        session_id,
                        turn.id,
                        "Provider interruption failed: the Session has no Provider actor.",
                    )
                })?;
                let (response_tx, response_rx) = oneshot::channel();
                actor
                    .send(ProviderCommand::InterruptSession {
                        response: response_tx,
                    })
                    .map_err(|_| {
                        self.fail_unavailable_interruption(
                            session_id,
                            turn.id,
                            "Provider interruption failed: the Provider Session stopped unexpectedly.",
                        )
                    })?;
                response_rx.await.map_err(|_| {
                    self.fail_unavailable_interruption(
                        session_id,
                        turn.id,
                        "Provider interruption failed: the Provider Session stopped unexpectedly.",
                    )
                })?
            }
            InterruptTarget::Subagents => {
                ask_actor_or_find_nothing_running(actor_commands(session_id), |response| {
                    ProviderCommand::InterruptSession { response }
                })
                .await
            }
            InterruptTarget::Subagent { root } => {
                ask_actor_or_find_nothing_running(actor_commands(root), |response| {
                    ProviderCommand::StopSubagent {
                        target: session_id,
                        response,
                    }
                })
                .await
            }
        };
        stopped.map(|()| InterruptOutcome::StoppedWork)
    }

    /// Fails a Turn whose Provider actor could not be reached, settling whatever
    /// it left in flight. It carries no trailing output because it has no
    /// normalizer to drain: an actor stays registered until it has stopped, so
    /// either no actor ever ran this Turn in this process, or the one that did
    /// has already settled it and this failure finds nothing left to settle.
    fn fail_unavailable_interruption(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        message: &str,
    ) -> InterruptSessionError {
        let _ = self.updates.apply(|| {
            self.sessions.fail_turn(
                session_id,
                turn_id,
                TrailingCommandOutput::new(),
                message.to_owned(),
                OpenInterventions::TurnEnded,
            )
        });
        InterruptSessionError::ProviderFailure(message.to_owned())
    }

    pub(crate) async fn close_session(&self, session_id: SessionId) {
        self.checkout_guards
            .lock()
            .unwrap()
            .retain(|(id, _), _| *id != session_id);
        self.connected_incarnations
            .lock()
            .unwrap()
            .remove(&session_id);
        let task = {
            let mut actors = self
                .actors
                .lock()
                .expect("Provider actor registry lock is not poisoned");
            let Some(actor) = actors.entries.get_mut(&session_id) else {
                return;
            };
            actor.shutdown.send_replace(true);
            actor.task.take()
        };
        if let Some(task) = task {
            let _ = task.await;
        }
        self.actors
            .lock()
            .expect("Provider actor registry lock is not poisoned")
            .entries
            .remove(&session_id);
    }

    pub(crate) async fn shutdown(&self) {
        let actors = {
            let mut registry = self
                .actors
                .lock()
                .expect("Provider actor registry lock is not poisoned");
            if registry.shutting_down {
                None
            } else {
                registry.shutting_down = true;
                Some(
                    registry
                        .entries
                        .drain()
                        .map(|(_, actor)| actor)
                        .collect::<Vec<_>>(),
                )
            }
        };

        let Some(actors) = actors else {
            let mut complete = self.shutdown_complete.subscribe();
            while !*complete.borrow() && complete.changed().await.is_ok() {}
            return;
        };

        for mut actor in actors {
            if let Some(task) = actor.task.take() {
                let _ = task.await;
            }
        }
        let _ = timeout(
            Duration::from_secs(2),
            futures_util::future::join_all(self.runtimes.iter().map(|runtime| runtime.shutdown())),
        )
        .await;
        self.checkout_guards.lock().unwrap().clear();
        self.connected_incarnations.lock().unwrap().clear();
        self.shutdown_complete.send_replace(true);
    }
}

/// An actor owns every pending startup lease and native incarnation it publishes.
/// Drop also handles cancellation or a panic before the normal shutdown path.
struct CheckoutActorCleanup {
    session_id: SessionId,
    checkout_guards: CheckoutGuards,
    connected_incarnations: Arc<Mutex<HashMap<SessionId, u64>>>,
}
impl Drop for CheckoutActorCleanup {
    fn drop(&mut self) {
        self.checkout_guards
            .lock()
            .unwrap()
            .retain(|(id, _), _| *id != self.session_id);
        self.connected_incarnations
            .lock()
            .unwrap()
            .remove(&self.session_id);
    }
}

async fn run_provider_session(
    context: ProviderSessionContext,
    mut commands: mpsc::UnboundedReceiver<ProviderCommand>,
    mut shutdown: ProviderShutdown,
) {
    let ProviderSessionContext {
        source_control,
        checkout_guards,
        connected_incarnations,
        checkout_skill_timeout,
        runtime,
        sessions,
        skill_catalog,
        session_id,
        execution_directory,
        updates,
        settings,
    } = context;
    let _checkout_cleanup = CheckoutActorCleanup {
        session_id,
        checkout_guards: checkout_guards.clone(),
        connected_incarnations: connected_incarnations.clone(),
    };
    let mut provider: Option<ConnectedProviderSession> = None;
    let mut active: Option<ActiveProviderTurn> = None;
    let mut subagents = SubagentRoutes::new(runtime.provider_id());
    let mut questionnaire_deliveries = QuestionnaireDeliveries::default();
    let mut decision_deliveries = DecisionDeliveries::default();
    let mut deferred_prompt_id = None;
    let mut pending_turn_starts = VecDeque::new();
    let provider_id = runtime.provider_id();

    'actor: loop {
        questionnaire_deliveries.reconcile(&sessions);
        decision_deliveries.reconcile(&sessions);
        if shutdown.requested() {
            break;
        }
        if active.is_none() {
            let pending_prompt = pending_turn_starts.front().copied().or(deferred_prompt_id);
            let input = if let Some(connected) = provider.as_mut() {
                tokio::select! {
                    biased;
                    _ = shutdown.wait() => break,
                    event = connected.events.next() => ProviderInput::Event(event),
                    _ = std::future::ready(()), if pending_prompt.is_some() => {
                        let prompt_id = pending_turn_starts.pop_front()
                            .or_else(|| deferred_prompt_id.take()).expect("pending Prompt");
                        ProviderInput::Command(Some(ProviderCommand::StartPrompt { prompt_id }))
                    }
                    command = commands.recv() => ProviderInput::Command(command),
                }
            } else if let Some(prompt_id) = pending_turn_starts
                .pop_front()
                .or_else(|| deferred_prompt_id.take())
            {
                ProviderInput::Command(Some(ProviderCommand::StartPrompt { prompt_id }))
            } else {
                tokio::select! {
                    biased;
                    _ = shutdown.wait() => break,
                    command = commands.recv() => ProviderInput::Command(command),
                }
            };
            let command = match input {
                ProviderInput::Command(command) => command,
                ProviderInput::Event(Some(Ok(attributed))) => {
                    let identity = provider
                        .as_ref()
                        .expect("Provider events arrive over a live connection")
                        .identity
                        .clone();
                    match attributed {
                        // Resume State is the owning Session's, whatever the
                        // attribution, and no output owed a Continuation.
                        AttributedProviderEvent {
                            event: ProviderEvent::ResumeStateChanged { resume_state },
                            ..
                        } => save_revised_resume_state(
                            &sessions,
                            &updates,
                            session_id,
                            &provider_id,
                            resume_state,
                        ),
                        // A Subagent's events land in its own Session whether
                        // or not the owning Session has a Turn open.
                        AttributedProviderEvent {
                            attribution: ProviderEventAttribution::Subagent(subagent),
                            event,
                        } => {
                            subagents
                                .project_event(&sessions, &updates, &identity, &subagent, event);
                        }
                        AttributedProviderEvent {
                            attribution: ProviderEventAttribution::OwningSession,
                            event,
                        } => match event {
                            ProviderEvent::ContextFill { report } => {
                                if let Some(turn_id) = report.turn_id.or_else(|| {
                                    sessions.snapshot(session_id).and_then(|snapshot| {
                                        snapshot.turns.last().map(|turn| turn.id)
                                    })
                                }) {
                                    let _ = updates.apply(|| {
                                        sessions.report_context_fill(session_id, turn_id, report)
                                    });
                                }
                            }
                            // With no Turn active there is nothing for a
                            // terminal or selection event to land on.
                            ProviderEvent::TurnCompleted
                            | ProviderEvent::TurnInterrupted
                            | ProviderEvent::TurnFailed { .. }
                            | ProviderEvent::AgentSelectionChanged { .. }
                            | ProviderEvent::AgentSelectionRejected { .. } => {}
                            // A Subagent's own lifecycle addresses its row,
                            // which outlives the Turn that spawned it; an
                            // identity no row carries lands nowhere, like any
                            // other unrouted Subagent event.
                            ProviderEvent::SubagentUpdated {
                                subagent_id,
                                description,
                            } => {
                                let _ = updates.apply(|| {
                                    subagents.update_row(&sessions, &subagent_id, &description)
                                });
                            }
                            ProviderEvent::SubagentModelChanged { subagent_id, model } => {
                                let _ = updates.apply(|| {
                                    subagents.update_model(
                                        &sessions,
                                        &subagent_id,
                                        &identity,
                                        model,
                                    )
                                });
                            }
                            ProviderEvent::SubagentCompleted {
                                subagent_id,
                                status,
                            } => {
                                let _ = updates.apply(|| {
                                    subagents.settle_subagent(&sessions, &subagent_id, status)
                                });
                            }
                            // Anything else is late output. Owed to Subagents
                            // still working past their Turn's settle — or to
                            // one that just settled, whose provoked output
                            // follows its settle — it begins a Continuation
                            // (ADR 0015); with nothing owed, stray output —
                            // an interrupted Turn's trailing stream, say — is
                            // discarded as it always was. A resume is never
                            // stray: it begins a stretch of delegated work
                            // whose row stands in a Continuation when the
                            // Turn that delegated it has already settled
                            // (ADR 0031, 0032), and dropping it would leave
                            // that stretch's work with nowhere to land.
                            event => {
                                let mut identity = identity.clone();
                                if let ProviderEvent::ContinuationStarted { selection } = &event {
                                    identity.selection = selection.clone();
                                } else if !subagents.owes_continuation()
                                    && !matches!(event, ProviderEvent::SubagentResumed { .. })
                                {
                                    continue;
                                }
                                let Some(begun) = updates.apply(|| {
                                    sessions.begin_continuation(session_id, identity.clone())
                                }) else {
                                    break;
                                };
                                let Ok(turn_id) = begun else {
                                    continue;
                                };
                                let mut continuation =
                                    ActiveProviderTurn::new_continuation(turn_id);
                                let projection = project_provider_event(
                                    &sessions,
                                    &updates,
                                    session_id,
                                    &mut continuation,
                                    &mut subagents,
                                    &identity,
                                    event,
                                );
                                subagents.late_settle_owes_continuation = false;
                                if !matches!(projection, ProviderEventProjection::Terminal) {
                                    active = Some(continuation);
                                }
                            }
                        },
                    }
                    continue;
                }
                ProviderInput::Event(Some(Err(_)) | None) => {
                    lose_provider_connection(&mut provider, &mut subagents, &sessions, &updates);
                    continue;
                }
            };
            let Some(command) = command else { break };
            let prompt_id = match command {
                ProviderCommand::SubmitDecision {
                    target,
                    id,
                    decision,
                    response,
                } => {
                    if let (Some(connected), Some(live)) =
                        (provider.as_ref(), subagents.live_approvals(target))
                    {
                        decision_deliveries.submit(
                            &sessions,
                            &updates,
                            target,
                            live,
                            connected.session.clone(),
                            id,
                            decision,
                            response,
                        );
                    } else {
                        let _ = response.send(Err("Approval is unavailable".into()));
                    }
                    continue;
                }
                ProviderCommand::SubmitQuestionnaire {
                    target,
                    id,
                    submission,
                    response,
                } => {
                    if let (Some(connected), Some(live)) =
                        (provider.as_ref(), subagents.live_questionnaires(target))
                    {
                        questionnaire_deliveries.submit(
                            &sessions,
                            &updates,
                            target,
                            live,
                            connected.session.clone(),
                            id,
                            submission,
                            response,
                        );
                    } else {
                        let _ = response.send(Err("Questionnaire is unavailable".into()));
                    }
                    continue;
                }
                ProviderCommand::StartPrompt { prompt_id } => prompt_id,
                ProviderCommand::InterruptSession { response } => {
                    // With no Turn active, the interrupt reaches the
                    // Subagents that outlived it (ADR 0015). Nothing still
                    // working answers success, because the work the caller
                    // meant to stop is already over.
                    if subagents.routes.is_empty() && subagents.rows.is_empty() {
                        let _ = response.send(Ok(()));
                        continue;
                    }
                    let connected = provider
                        .as_ref()
                        .expect("routed Subagents ride a live Provider connection");
                    let provider_session = connected.session.clone();
                    let stopped = tokio::select! {
                        biased;
                        _ = shutdown.wait() => {
                            let _ = response.send(Err(InterruptSessionError::ProviderFailure(
                                "Provider interruption failed: the Provider Session is shutting down."
                                    .to_owned(),
                            )));
                            break 'actor;
                        }
                        stopped = provider_session.stop_subagents() => stopped,
                    };
                    match stopped {
                        Ok(()) => {
                            subagents.stop_all(&sessions, &updates);
                            let _ = response.send(Ok(()));
                        }
                        Err(error) => {
                            let message = failure_message("Provider interruption failed", &error);
                            lose_provider_connection(
                                &mut provider,
                                &mut subagents,
                                &sessions,
                                &updates,
                            );
                            let _ =
                                response.send(Err(InterruptSessionError::ProviderFailure(message)));
                        }
                    }
                    continue;
                }
                ProviderCommand::StopSubagent { target, response } => {
                    let _ = response.send(
                        stop_one_subagent(
                            &runtime,
                            provider.as_ref().map(|c| c.session.clone()),
                            &mut subagents,
                            &sessions,
                            &updates,
                            target,
                        )
                        .await,
                    );
                    continue;
                }
                ProviderCommand::UpdateApprovalPosture { update, response } => {
                    if !sessions.approval_posture_update_is_pending(update) {
                        let _ = response.send(Ok(ProviderPostureApplication::Applied));
                        continue;
                    }
                    let updated = if let Some(connected) = provider.as_ref() {
                        tokio::select! {
                            biased;
                            _ = shutdown.wait() => Err(
                                "Approval Posture update failed: the Provider Session is shutting down."
                                    .to_owned(),
                            ),
                            updated = connected.session.update_approval_posture(
                                update.value,
                                !subagents.routes.is_empty() || !subagents.rows.is_empty(),
                            ) => updated.map_err(|error| {
                                failure_message("Approval Posture update failed", &error)
                            }),
                        }
                    } else {
                        Ok(ProviderPostureApplication::Applied)
                    };
                    record_posture_application(&sessions, update, &updated);
                    let _ = response.send(updated);
                    continue;
                }
                ProviderCommand::SteerPrompt => continue,
            };
            let Some(snapshot) = sessions.snapshot(session_id) else {
                break;
            };
            let Some(pending) = snapshot
                .prompts
                .iter()
                .find(|p| p.id == prompt_id && p.status == PromptStatus::Pending)
            else {
                checkout_guards
                    .lock()
                    .unwrap()
                    .remove(&(session_id, prompt_id));
                defer_next_queued_prompt(&mut deferred_prompt_id, &sessions, session_id);
                continue;
            };
            let inherited = checkout_guards
                .lock()
                .unwrap()
                .remove(&(session_id, prompt_id));
            let preparation = tokio::select! {
                _ = shutdown.wait() => break 'actor,
                result = source_control.prepare_execution(&snapshot.session, inherited) => result,
            };
            let preparation = preparation.and_then(|lease| {
                if let Some(reading) = lease.reading.clone() {
                    sessions
                        .record_checkout(reading)
                        .map_err(|e| format!("Cannot persist checkout recovery facts: {e}"))?;
                }
                Ok(lease)
            });
            let lease = match preparation {
                Ok(lease) => lease,
                Err(message) => {
                    let _ = updates.apply(|| sessions.deliver_prompt(session_id, prompt_id, None, DeliveredTurnStatus::Failed { message: format!("Worktree unavailable; restore the checkout or retry recovery: {message}") }));
                    defer_next_queued_prompt(&mut deferred_prompt_id, &sessions, session_id);
                    continue;
                }
            };
            let reconnect = provider
                .as_ref()
                .is_some_and(|p| p.incarnation != lease.incarnation);
            if reconnect && (!subagents.routes.is_empty() || !subagents.rows.is_empty()) {
                let _ = updates.apply(|| sessions.deliver_prompt(session_id, prompt_id, None, DeliveredTurnStatus::Failed { message: "The Worktree was recreated while Subagents still use the previous working copy; wait for their work to finish before retrying".to_owned() }));
                defer_next_queued_prompt(&mut deferred_prompt_id, &sessions, session_id);
                continue;
            }
            if reconnect {
                if let Some(previous) = provider.take() {
                    let _ = timeout(Duration::from_secs(2), previous.session.shutdown()).await;
                }
                connected_incarnations.lock().unwrap().remove(&session_id);
            }
            if snapshot
                .session
                .checkout
                .as_ref()
                .is_some_and(|c| c.kind == crate::protocol::CheckoutKind::Linked)
                && (provider.is_none() || lease.recreated)
            {
                let refreshed = tokio::select! {
                    _ = shutdown.wait() => break 'actor,
                    result = timeout(checkout_skill_timeout, skill_catalog.refresh_current(crate::protocol::SkillCatalogRequest { provider: provider_id.clone(), execution_directory: snapshot.session.execution_directory.clone() })) => result,
                };
                let error = match refreshed {
                    Ok(Ok(catalog))
                        if matches!(
                            catalog.status,
                            crate::protocol::SkillCatalogStatus::Fresh { .. }
                        ) =>
                    {
                        None
                    }
                    Ok(result) => Some(format!(
                        "Destination Skills are unavailable; retry after restoring them: {result:?}"
                    )),
                    Err(_) => Some(
                        "Destination Skill discovery timed out; retry Worktree execution"
                            .to_owned(),
                    ),
                };
                let initial = crate::protocol::InitialPrompt {
                    id: pending.id,
                    text: pending.text.clone(),
                    skill_invocations: pending.skill_invocations.clone(),
                };
                let error = if error.is_none() {
                    skill_catalog.validate_prompt(provider_id.clone(), &execution_directory, &initial, skill_prompt_delivery(pending)).await.err().map(|e| format!("Destination Skills must be revalidated before native execution: {e:?}"))
                } else {
                    error
                };
                if let Some(message) = error {
                    let _ = updates.apply(|| {
                        sessions.deliver_prompt(
                            session_id,
                            prompt_id,
                            None,
                            DeliveredTurnStatus::Failed { message },
                        )
                    });
                    defer_next_queued_prompt(&mut deferred_prompt_id, &sessions, session_id);
                    continue;
                }
            }
            if provider.is_none() {
                let session_posture =
                    effective_approval_posture(&snapshot, &settings.borrow(), &provider_id);
                let connection = tokio::select! {
                    biased;
                    _ = shutdown.wait() => break 'actor,
                    connection = runtime.start_session(ProviderSessionRequest {
                        execution_directory: execution_directory.clone(),
                        resume_state: sessions.resume_state(session_id, &provider_id),
                        approval_posture: session_posture,
                    }) => connection,
                };
                let connection = match connection {
                    Ok(connection) => connection,
                    Err(error) => {
                        let Some(_) = updates.apply(|| {
                            sessions.deliver_prompt(
                                session_id,
                                prompt_id,
                                None,
                                DeliveredTurnStatus::Failed {
                                    message: startup_failure_message(&provider_id, &error),
                                },
                            )
                        }) else {
                            break;
                        };
                        defer_next_queued_prompt(&mut deferred_prompt_id, &sessions, session_id);
                        continue;
                    }
                };
                let (identity, resume_state, session, events) = connection.into_parts();
                if let Some(resume_state) = resume_state
                    && let Err(error) =
                        sessions.save_resume_state(session_id, provider_id.clone(), resume_state)
                {
                    let _ = updates.apply(|| {
                        sessions.deliver_prompt(
                            session_id,
                            prompt_id,
                            None,
                            DeliveredTurnStatus::Failed {
                                message: failure_message(
                                    "Provider startup failed: save Resume State",
                                    &error,
                                ),
                            },
                        )
                    });
                    let _ = timeout(Duration::from_secs(2), session.shutdown()).await;
                    defer_next_queued_prompt(&mut deferred_prompt_id, &sessions, session_id);
                    continue;
                }
                let selection = identity.selection.clone();
                let Some(selected) =
                    updates.apply(|| sessions.initialize_agent_selection(session_id, selection))
                else {
                    let _ = timeout(Duration::from_secs(2), session.shutdown()).await;
                    break;
                };
                if let Err(error) = selected {
                    let _ = updates.apply(|| {
                        sessions.deliver_prompt(
                            session_id,
                            prompt_id,
                            None,
                            DeliveredTurnStatus::Failed {
                                message: failure_message("Provider startup failed", &error),
                            },
                        )
                    });
                    let _ = timeout(Duration::from_secs(2), session.shutdown()).await;
                    defer_next_queued_prompt(&mut deferred_prompt_id, &sessions, session_id);
                    continue;
                }
                // Provider discovery can supply the Session's first Agent Selection. Derive the
                // corresponding unpinned posture immediately so snapshots do not remain empty
                // until a later HTTP mutation happens to reconcile them.
                let posture_update = sessions
                    .reconcile_tree_approval_posture(session_id, &settings.borrow().settings)
                    .filter(|update| {
                        update.session_id == session_id && Some(update.value) == session_posture
                    });
                if let Some(update) = posture_update {
                    sessions.mark_approval_posture_application(
                        update,
                        crate::protocol::ApprovalPostureApplication::Applied,
                    );
                }
                connected_incarnations
                    .lock()
                    .unwrap()
                    .insert(session_id, lease.incarnation);
                // The connection may carry on a conversation whose Subagents
                // an earlier process spawned; it names them by the identities
                // stored with their Sessions.
                subagents.restore(sessions.stored_subagents(session_id, &provider_id));
                provider = Some(ConnectedProviderSession {
                    incarnation: lease.incarnation,
                    identity,
                    session,
                    events,
                });
            }

            let identity = provider
                .as_ref()
                .expect("Provider connection exists before Prompt delivery")
                .identity
                .clone();
            let prompt = sessions.snapshot(session_id).and_then(|snapshot| {
                snapshot
                    .prompts
                    .into_iter()
                    .find(|prompt| prompt.id == prompt_id && prompt.status == PromptStatus::Pending)
            });
            let Some(prompt) = prompt else {
                defer_next_queued_prompt(&mut deferred_prompt_id, &sessions, session_id);
                continue;
            };
            let delivery = skill_prompt_delivery(&prompt);
            if let Err(message) = revalidate_prompt_skills(
                &skill_catalog,
                &sessions,
                session_id,
                &provider_id,
                &execution_directory,
                &prompt,
                delivery,
            )
            .await
            {
                let _ = updates.apply(|| {
                    sessions.deliver_prompt(
                        session_id,
                        prompt_id,
                        Some(identity.agent.clone()),
                        DeliveredTurnStatus::Failed { message },
                    )
                });
                defer_next_queued_prompt(&mut deferred_prompt_id, &sessions, session_id);
                continue;
            }
            let Some(delivered) = updates.apply(|| {
                sessions.deliver_prompt(
                    session_id,
                    prompt_id,
                    Some(identity.agent.clone()),
                    DeliveredTurnStatus::Active,
                )
            }) else {
                break;
            };
            drop(lease);
            let delivered = match delivered {
                Ok(Some(delivered)) => delivered,
                Ok(None) => {
                    // Cancellation can win while Skill Catalog validation is awaited.
                    // The selected Prompt is gone, but the queue may still hold work.
                    defer_next_queued_prompt(&mut deferred_prompt_id, &sessions, session_id);
                    continue;
                }
                Err(_) => continue,
            };
            let provider_session = provider
                .as_ref()
                .expect("Provider connection exists before Prompt delivery")
                .session
                .clone();
            let posture = sessions.snapshot(session_id).and_then(|snapshot| {
                effective_approval_posture(&snapshot, &settings.borrow(), &provider_id)
            });
            let posture_update = posture.and_then(|posture| {
                sessions
                    .current_approval_posture_update(session_id)
                    .filter(|update| update.value == posture)
            });
            let (turn_id, input) = provider_turn_start(delivered, posture);
            let started = tokio::select! {
                biased;
                _ = shutdown.wait() => break 'actor,
                started = provider_session.start_turn(input) => started,
            };
            if let Err(error) = started {
                let session_lost = error.is_session_lost();
                let selection_rejected = error.is_selection_rejected();
                project_turn_start_failure(&sessions, &updates, session_id, turn_id, &error);
                if session_lost {
                    lose_provider_connection(&mut provider, &mut subagents, &sessions, &updates);
                }
                if !selection_rejected {
                    defer_next_queued_prompt(&mut deferred_prompt_id, &sessions, session_id);
                }
                continue;
            }
            if let Some(update) = posture_update {
                sessions.mark_approval_posture_application(
                    update,
                    crate::protocol::ApprovalPostureApplication::Applied,
                );
            }
            active = Some(ActiveProviderTurn::new(turn_id));
            subagents.late_settle_owes_continuation = false;
            continue;
        }

        let provider_session = provider
            .as_ref()
            .expect("an active Provider Turn has a Provider Session")
            .session
            .clone();
        let input = {
            let pending_native_continuation = active
                .as_ref()
                .is_some_and(|turn| turn.continuation == Some(ContinuationExecution::ProviderTurn));
            let deliver_pending = active
                .as_ref()
                .is_some_and(|turn| turn.continuation.is_some() && !turn.interruption_acknowledged)
                && (!pending_turn_starts.is_empty() || deferred_prompt_id.is_some());
            let events = &mut provider
                .as_mut()
                .expect("an active Provider Turn has a Provider Session")
                .events;
            tokio::select! {
                // Preserve the Provider's terminal boundary when both it and a later command
                // became ready while an RPC was in flight.
                biased;
                _ = shutdown.wait() => break 'actor,
                event = events.next() => ProviderInput::Event(event),
                _ = std::future::ready(()), if deliver_pending => {
                    if pending_native_continuation {
                        // Keep the queue in place while the native loop stops. Removing and
                        // re-enqueuing its front would let later admissions overtake it.
                        let (response, _) = oneshot::channel();
                        ProviderInput::Command(Some(ProviderCommand::InterruptSession { response }))
                    } else {
                        let prompt_id = pending_turn_starts.pop_front()
                            .or_else(|| deferred_prompt_id.take()).expect("pending Prompt");
                        ProviderInput::Command(Some(ProviderCommand::StartPrompt { prompt_id }))
                    }
                }
                command = commands.recv() => ProviderInput::Command(command),
            }
        };
        // A native Continuation must release its Provider Turn before the
        // next Prompt starts one. Reuse the normal interrupt path and wait
        // for its terminal event; a local settle alone leaves Codex busy.
        let input = match input {
            ProviderInput::Command(Some(ProviderCommand::StartPrompt { prompt_id }))
                if active.as_ref().is_some_and(|turn| {
                    turn.continuation == Some(ContinuationExecution::ProviderTurn)
                }) =>
            {
                pending_turn_starts.push_back(prompt_id);
                let (response, _) = oneshot::channel();
                ProviderInput::Command(Some(ProviderCommand::InterruptSession { response }))
            }
            input => input,
        };
        match input {
            ProviderInput::Command(None) => break,
            ProviderInput::Command(Some(ProviderCommand::StartPrompt { prompt_id })) => {
                let current = active
                    .as_mut()
                    .expect("Provider input is handled while a Turn is active");
                if current.continuation.is_some() {
                    // The next delivered Prompt settles a stale Continuation
                    // rather than steering it (ADR 0015): close it out as
                    // worked, then deliver the Prompt as its own Turn.
                    let trailing_output = current.take_trailing_output();
                    let Some(_) = updates.apply(|| {
                        sessions.finish_provider_turn(
                            session_id,
                            current.turn_id,
                            ProviderTurnOutcome::Completed { trailing_output },
                        )
                    }) else {
                        break;
                    };
                    active = None;
                    pending_turn_starts.push_front(prompt_id);
                } else {
                    // Admission owed this Prompt a Turn of its own. Its
                    // command may arrive after the Continuation settled and
                    // an earlier replacement Prompt already began running.
                    pending_turn_starts.push_back(prompt_id);
                }
            }
            ProviderInput::Command(Some(ProviderCommand::SubmitQuestionnaire {
                target,
                id,
                submission,
                response,
            })) => {
                let live = if target == session_id {
                    active.as_mut().map(|turn| &mut turn.questionnaires)
                } else {
                    subagents.live_questionnaires(target)
                };
                if let Some(live) = live {
                    questionnaire_deliveries.submit(
                        &sessions,
                        &updates,
                        target,
                        live,
                        provider_session,
                        id,
                        submission,
                        response,
                    );
                } else {
                    let _ = response.send(Err("Questionnaire is unavailable".into()));
                }
            }
            ProviderInput::Command(Some(ProviderCommand::SubmitDecision {
                target,
                id,
                decision,
                response,
            })) => {
                let live = if target == session_id {
                    active.as_mut().map(|turn| &mut turn.approvals)
                } else {
                    subagents.live_approvals(target)
                };
                if let Some(live) = live {
                    decision_deliveries.submit(
                        &sessions,
                        &updates,
                        target,
                        live,
                        provider_session,
                        id,
                        decision,
                        response,
                    );
                } else {
                    let _ = response.send(Err("Approval is unavailable".into()));
                }
            }

            ProviderInput::Command(Some(ProviderCommand::UpdateApprovalPosture {
                update,
                response,
            })) => {
                if !sessions.approval_posture_update_is_pending(update) {
                    let _ = response.send(Ok(ProviderPostureApplication::Applied));
                    continue;
                }
                let updated = tokio::select! {
                    biased;
                    _ = shutdown.wait() => Err(
                        "Approval Posture update failed: the Provider Session is shutting down."
                            .to_owned(),
                    ),
                    updated = provider_session.update_approval_posture(update.value, true) => {
                        updated.map_err(|error| failure_message("Approval Posture update failed", &error))
                    }
                };
                record_posture_application(&sessions, update, &updated);
                let _ = response.send(updated);
            }

            ProviderInput::Command(Some(ProviderCommand::SteerPrompt)) => {
                let current = active
                    .as_ref()
                    .expect("Provider input is handled while a Turn is active");
                let prompt = match sessions.next_pending_steer(session_id, current.turn_id) {
                    Ok(Some(prompt)) => prompt,
                    Ok(None) | Err(_) => continue,
                };
                let Some(snapshot) = sessions.snapshot(session_id) else {
                    continue;
                };
                let checkout = tokio::select! {
                    biased;
                    _ = shutdown.wait() => break 'actor,
                    lease = source_control.prepare_execution(&snapshot.session, None) => lease,
                };
                let checkout = checkout.and_then(|lease| {
                    if let Some(reading) = lease.reading.clone() {
                        sessions
                            .record_checkout(reading)
                            .map_err(|e| format!("Cannot persist checkout recovery facts: {e}"))?;
                    }
                    Ok(lease)
                });
                let _checkout_lease = match checkout {
                    Ok(lease)
                        if provider
                            .as_ref()
                            .is_some_and(|p| p.incarnation == lease.incarnation) =>
                    {
                        lease
                    }
                    result => {
                        let message = match result {
                            Err(message) => format!("Worktree unavailable; retry after resolving recovery: {message}"),
                            Ok(_) => "The Worktree was recreated while this Agent is still Working; wait for it to settle before retrying".to_owned(),
                        };
                        let _ = updates.apply(|| {
                            sessions.fail_skill_steer(
                                session_id,
                                current.turn_id,
                                prompt.id,
                                message,
                            )
                        });
                        continue;
                    }
                };
                if let Err(message) = revalidate_prompt_skills(
                    &skill_catalog,
                    &sessions,
                    session_id,
                    &provider_id,
                    &execution_directory,
                    &prompt,
                    SkillPromptDelivery::Steer,
                )
                .await
                {
                    let _ = updates.apply(|| {
                        sessions.fail_skill_steer(session_id, current.turn_id, prompt.id, message)
                    });
                    continue;
                }
                let steered = tokio::select! {
                    biased;
                    _ = shutdown.wait() => break 'actor,
                    steered = provider_session.steer_turn(ProviderSteerInput {
                        prompt: ProviderPrompt::from_user_prompt(
                            prompt.text.clone(),
                            prompt.skill_invocations.clone(),
                        ),
                    }) => steered,
                };
                match steered {
                    Ok(()) => {
                        let _ = updates.apply(|| {
                            sessions.deliver_steer(session_id, current.turn_id, prompt.id)
                        });
                    }
                    Err(error) => {
                        let _ = updates.apply(|| {
                            let message = failure_message("Provider steering failed", &error);
                            if prompt.skill_invocations.is_empty() {
                                sessions.report_steer_failure(
                                    session_id,
                                    current.turn_id,
                                    prompt.id,
                                    message,
                                )
                            } else {
                                sessions.fail_skill_steer(
                                    session_id,
                                    current.turn_id,
                                    prompt.id,
                                    message,
                                )
                            }
                        });
                    }
                }
            }
            ProviderInput::Command(Some(ProviderCommand::InterruptSession { response })) => {
                let current = active
                    .as_mut()
                    .expect("Provider input is handled while a Turn is active");
                if current.interruption_acknowledged {
                    let _ = response.send(Ok(()));
                    continue;
                }
                if current.continuation == Some(ContinuationExecution::LateOutput) {
                    // A Continuation runs no Provider loop of its own — it is
                    // the Turn Suru opened for the output its Subagents still
                    // owed. Interrupting it stops those Subagents, and the
                    // Continuation settles as interrupted here rather than on
                    // a terminal event no loop will send.
                    let stopped = tokio::select! {
                        biased;
                        _ = shutdown.wait() => {
                            let _ = response.send(Err(InterruptSessionError::ProviderFailure(
                                "Provider interruption failed: the Provider Session is shutting down."
                                    .to_owned(),
                            )));
                            break 'actor;
                        }
                        stopped = provider_session.stop_subagents() => stopped,
                    };
                    match stopped {
                        Ok(()) => {
                            subagents.stop_all(&sessions, &updates);
                            let trailing_output = current.take_trailing_output();
                            let settled = updates.apply(|| {
                                sessions.finish_provider_turn(
                                    session_id,
                                    current.turn_id,
                                    ProviderTurnOutcome::Interrupted { trailing_output },
                                )
                            });
                            active = None;
                            let _ = response.send(Ok(()));
                            if settled.is_none() {
                                break;
                            }
                            defer_next_queued_prompt(
                                &mut deferred_prompt_id,
                                &sessions,
                                session_id,
                            );
                        }
                        Err(error) => {
                            let message = failure_message("Provider interruption failed", &error);
                            fail_active_turn(
                                &sessions,
                                &updates,
                                session_id,
                                current,
                                message.clone(),
                                OpenInterventions::TurnEnded,
                            );
                            active = None;
                            lose_provider_connection(
                                &mut provider,
                                &mut subagents,
                                &sessions,
                                &updates,
                            );
                            defer_next_queued_prompt(
                                &mut deferred_prompt_id,
                                &sessions,
                                session_id,
                            );
                            let _ =
                                response.send(Err(InterruptSessionError::ProviderFailure(message)));
                        }
                    }
                    continue;
                }
                let interrupted = tokio::select! {
                    biased;
                    // Answer the caller before leaving, so it reports the interruption
                    // failure rather than inventing one that would race this actor's
                    // settling of the Turn on its way out.
                    _ = shutdown.wait() => {
                        let _ = response.send(Err(InterruptSessionError::ProviderFailure(
                            "Provider interruption failed: the Provider Session is shutting down."
                                .to_owned(),
                        )));
                        break 'actor;
                    }
                    interrupted = provider_session.interrupt_turn() => interrupted,
                };
                match interrupted {
                    Ok(()) => {
                        current.interruption_acknowledged = true;
                        // The Provider stopped the Turn's background work
                        // ahead of the loop — the established ordering — so
                        // the Subagents settle as stopped now, rather than
                        // waiting on notifications an ended loop may never
                        // deliver.
                        subagents.stop_all(&sessions, &updates);
                        let _ = response.send(Ok(()));
                    }
                    Err(error) => {
                        let message = failure_message("Provider interruption failed", &error);
                        fail_active_turn(
                            &sessions,
                            &updates,
                            session_id,
                            current,
                            message.clone(),
                            OpenInterventions::TurnEnded,
                        );
                        active = None;
                        lose_provider_connection(
                            &mut provider,
                            &mut subagents,
                            &sessions,
                            &updates,
                        );
                        defer_next_queued_prompt(&mut deferred_prompt_id, &sessions, session_id);
                        let _ = response.send(Err(InterruptSessionError::ProviderFailure(message)));
                    }
                }
            }
            ProviderInput::Command(Some(ProviderCommand::StopSubagent { target, response })) => {
                let _ = response.send(
                    stop_one_subagent(
                        &runtime,
                        provider.as_ref().map(|c| c.session.clone()),
                        &mut subagents,
                        &sessions,
                        &updates,
                        target,
                    )
                    .await,
                );
            }
            ProviderInput::Event(event) => {
                let Some(current) = active.as_mut() else {
                    continue;
                };
                match event {
                    Some(Ok(AttributedProviderEvent {
                        event: ProviderEvent::ResumeStateChanged { resume_state },
                        ..
                    })) => save_revised_resume_state(
                        &sessions,
                        &updates,
                        session_id,
                        &provider_id,
                        resume_state,
                    ),
                    Some(Ok(AttributedProviderEvent {
                        attribution: ProviderEventAttribution::Subagent(subagent),
                        event,
                    })) => {
                        let identity = &provider
                            .as_ref()
                            .expect("Provider connection exists while its Turn is active")
                            .identity;
                        subagents.project_event(&sessions, &updates, identity, &subagent, event);
                    }
                    Some(Ok(AttributedProviderEvent {
                        attribution: ProviderEventAttribution::OwningSession,
                        event,
                    })) => {
                        // There is no originating Prompt to restore when a
                        // Continuation's selection is rejected. Keep the
                        // Provider's failure and let queued work proceed.
                        let event = match event {
                            ProviderEvent::AgentSelectionRejected { message }
                                if current.continuation.is_some() =>
                            {
                                ProviderEvent::TurnFailed { message }
                            }
                            event => event,
                        };
                        let selection_rejected =
                            matches!(event, ProviderEvent::AgentSelectionRejected { .. });
                        let identity = provider
                            .as_ref()
                            .expect("Provider connection exists while its Turn is active")
                            .identity
                            .clone();
                        // Settle before delivering queued work. The next loop drains ready
                        // Provider events first, so a buffered native Continuation establishes
                        // ownership before a Prompt can take its output and terminal result.
                        if let ProviderEventProjection::Terminal = project_provider_event(
                            &sessions,
                            &updates,
                            session_id,
                            current,
                            &mut subagents,
                            &identity,
                            event,
                        ) {
                            active = None;
                            if !selection_rejected {
                                defer_next_queued_prompt(
                                    &mut deferred_prompt_id,
                                    &sessions,
                                    session_id,
                                );
                            }
                        }
                    }
                    Some(Err(error)) => {
                        fail_active_turn(
                            &sessions,
                            &updates,
                            session_id,
                            current,
                            failure_message("Provider execution failed", &error),
                            OpenInterventions::TurnEnded,
                        );
                        active = None;
                        lose_provider_connection(
                            &mut provider,
                            &mut subagents,
                            &sessions,
                            &updates,
                        );
                        defer_next_queued_prompt(&mut deferred_prompt_id, &sessions, session_id);
                    }
                    None => {
                        fail_active_turn(
                            &sessions,
                            &updates,
                            session_id,
                            current,
                            "Provider execution failed: the Provider Session ended before the Turn completed."
                                .to_owned(),
                            OpenInterventions::TurnEnded,
                        );
                        active = None;
                        lose_provider_connection(
                            &mut provider,
                            &mut subagents,
                            &sessions,
                            &updates,
                        );
                        defer_next_queued_prompt(&mut deferred_prompt_id, &sessions, session_id);
                    }
                }
            }
        }
    }

    // The actor holds the only handle to its Turn's normalizers, so settle the Turn
    // here rather than dropping them: whatever stopped this actor also stopped the
    // Turn, and nothing else will finish the streams it left in flight. Losing the
    // actor is a failure even if it had acknowledged an interrupt before it stopped.
    if let Some(mut current) = active {
        fail_active_turn(
            &sessions,
            &updates,
            session_id,
            &mut current,
            "Provider execution failed: Suru stopped the Provider Session before the Turn completed."
                .to_owned(),
            OpenInterventions::Abandoned,
        );
    }
    subagents.fail_all(
        &sessions,
        &updates,
        SUBAGENT_CONNECTION_LOST_MESSAGE,
        OpenInterventions::Abandoned,
    );
    drop(questionnaire_deliveries);
    drop(decision_deliveries);

    if let Some(connected) = provider {
        let _ = timeout(Duration::from_secs(2), connected.session.shutdown()).await;
    }
}

/// Delivers one stop-shaped command to a Session's actor and awaits its
/// answer. These stops target only Subagents, which live and die with their
/// actor's connection: no actor left — or one that stopped before answering —
/// means nothing is still running, and stopping nothing is success.
async fn ask_actor_or_find_nothing_running(
    actor: Option<mpsc::UnboundedSender<ProviderCommand>>,
    command: impl FnOnce(oneshot::Sender<Result<(), InterruptSessionError>>) -> ProviderCommand,
) -> Result<(), InterruptSessionError> {
    let Some(actor) = actor else {
        return Ok(());
    };
    let (response_tx, response_rx) = oneshot::channel();
    if actor.send(command(response_tx)).is_err() {
        return Ok(());
    }
    response_rx.await.unwrap_or(Ok(()))
}

/// Answers one per-Subagent stop request: resolves the Subagent whose child
/// Session was named, asks the Provider to stop it, and settles it — with
/// whatever it delegated in turn — as stopped. A Subagent already settled
/// leaves nothing to stop, and stopping nothing succeeds. The Provider call
/// is bounded by the Provider's own interrupt timeout, so this holds the
/// actor no longer than an interrupt would; a stop the Provider refuses
/// leaves everything running and reports the refusal, because one Subagent
/// the Provider would not stop is no reason to tear the Session down.
async fn stop_one_subagent(
    runtime: &Arc<dyn ProviderRuntime>,
    connection: Option<Arc<dyn ProviderSession>>,
    subagents: &mut SubagentRoutes,
    sessions: &SessionStore,
    updates: &ProviderUpdateGate,
    target: SessionId,
) -> Result<(), InterruptSessionError> {
    if !runtime.supports_subagent_stop() {
        return Err(InterruptSessionError::SubagentStopUnsupported);
    }
    let Some(subagent) = subagents.subagent_for_session(target) else {
        return Ok(());
    };
    let provider_session = connection.expect("routed Subagents ride a live Provider connection");
    match provider_session.stop_subagent(subagent.clone()).await {
        Ok(()) => {
            subagents.settle_stopped(sessions, updates, &subagent);
            Ok(())
        }
        Err(error) => Err(InterruptSessionError::ProviderFailure(failure_message(
            "Provider Subagent stop failed",
            &error,
        ))),
    }
}

/// Drops the Provider connection and fails every Turn routed over it, in one
/// move so no teardown path can forget that the routes die with the
/// connection they were minted on.
fn lose_provider_connection(
    provider: &mut Option<ConnectedProviderSession>,
    subagents: &mut SubagentRoutes,
    sessions: &SessionStore,
    updates: &ProviderUpdateGate,
) {
    *provider = None;
    subagents.fail_all(
        sessions,
        updates,
        SUBAGENT_CONNECTION_LOST_MESSAGE,
        OpenInterventions::TurnEnded,
    );
}

/// Fails the Turn this actor was running, flushing the pending line each of its
/// command normalizers still holds. The store settles the streams the Turn
/// leaves in flight from its own snapshot; only these lines are the actor's to
/// hand over.
fn fail_active_turn(
    sessions: &SessionStore,
    updates: &ProviderUpdateGate,
    session_id: SessionId,
    active: &mut ActiveProviderTurn,
    message: String,
    interventions: OpenInterventions,
) {
    let turn_id = active.turn_id;
    let trailing_output = active.take_trailing_output();
    let _ = updates
        .apply(|| sessions.fail_turn(session_id, turn_id, trailing_output, message, interventions));
}

fn provider_turn_start(
    delivered: DeliveredTurn,
    approval_posture: Option<crate::protocol::ApprovalPosture>,
) -> (TurnId, ProviderTurnInput) {
    let turn_id = delivered.turn_id;
    let selection = delivered
        .agent
        .expect("a connected Provider delivers a selected Turn")
        .selection;
    (
        turn_id,
        ProviderTurnInput {
            turn_id,
            prompt: ProviderPrompt::from_user_prompt(
                delivered.prompt.text,
                delivered.prompt.skill_invocations,
            ),
            selection,
            approval_posture,
        },
    )
}

fn effective_approval_posture(
    snapshot: &crate::protocol::SessionSnapshot,
    settings: &SettingsSnapshot,
    provider: &crate::protocol::ProviderId,
) -> Option<crate::protocol::ApprovalPosture> {
    snapshot
        .session
        .approval_posture
        .as_ref()
        .filter(|posture| posture.pinned)
        .map(|posture| posture.value)
        .filter(|posture| posture.provider() == *provider)
        .or_else(|| crate::protocol::ApprovalPosture::for_provider(provider, &settings.settings))
}

fn record_posture_application(
    sessions: &SessionStore,
    update: ApprovalPostureUpdate,
    result: &Result<ProviderPostureApplication, String>,
) {
    let application = match result {
        Ok(ProviderPostureApplication::Applied) => {
            crate::protocol::ApprovalPostureApplication::Applied
        }
        Ok(ProviderPostureApplication::NextTurn) => {
            crate::protocol::ApprovalPostureApplication::NextTurn
        }
        Err(_) => crate::protocol::ApprovalPostureApplication::Failed,
    };
    sessions.mark_approval_posture_application(update, application);
}

fn project_turn_start_failure(
    sessions: &SessionStore,
    updates: &ProviderUpdateGate,
    session_id: SessionId,
    turn_id: TurnId,
    error: &super::ProviderError,
) {
    let selection_rejected = error.is_selection_rejected();
    let _ = updates.apply(|| {
        let message = failure_message("Provider execution failed", &error);
        if selection_rejected {
            sessions.reject_agent_selection(
                session_id,
                turn_id,
                TrailingCommandOutput::new(),
                message,
            )
        } else {
            sessions.fail_turn(
                session_id,
                turn_id,
                TrailingCommandOutput::new(),
                message,
                OpenInterventions::TurnEnded,
            )
        }
    });
}

/// The Skill delivery of a Prompt that begins a Turn of its own.
///
/// Only started Prompts come here; a Prompt admitted as a steer on an idle
/// Session still starts its own Turn, so its requested delivery does not make
/// it a steer.
fn skill_prompt_delivery(prompt: &Prompt) -> SkillPromptDelivery {
    if prompt.admission_order == PromptOrder::INITIAL {
        SkillPromptDelivery::Initial
    } else {
        SkillPromptDelivery::Queue
    }
}

fn next_queued_prompt(sessions: &SessionStore, session_id: SessionId) -> Option<Prompt> {
    sessions.snapshot(session_id).and_then(|snapshot| {
        earliest_pending_prompt(&snapshot.prompts, PromptDelivery::Queue).cloned()
    })
}

fn defer_next_queued_prompt(
    deferred_prompt_id: &mut Option<PromptId>,
    sessions: &SessionStore,
    session_id: SessionId,
) {
    *deferred_prompt_id = next_queued_prompt(sessions, session_id).map(|prompt| prompt.id);
}

async fn revalidate_prompt_skills(
    skill_catalog: &SkillCatalogService,
    sessions: &SessionStore,
    session_id: SessionId,
    provider: &ProviderId,
    execution_directory: &std::path::Path,
    prompt: &Prompt,
    delivery: SkillPromptDelivery,
) -> Result<(), String> {
    if prompt.skill_invocations.is_empty() {
        return Ok(());
    }
    if sessions.provider(session_id).as_ref() != Some(provider) {
        return Err(
            "Skill Invocation delivery failed: the Session Provider changed before delivery"
                .to_owned(),
        );
    }
    let prompt = crate::protocol::InitialPrompt {
        id: prompt.id,
        text: prompt.text.clone(),
        skill_invocations: prompt.skill_invocations.clone(),
    };
    skill_catalog
        .validate_prompt(provider.clone(), execution_directory, &prompt, delivery)
        .await
        .map_err(skill_delivery_failure_message)
}

fn skill_delivery_failure_message(error: SkillCatalogError) -> String {
    let cause = match error {
        SkillCatalogError::InvalidWorkspace => {
            "the Session Workspace is no longer valid".to_owned()
        }
        SkillCatalogError::ProviderNotHosted(provider) => {
            format!("Provider `{provider}` is no longer hosted")
        }
        SkillCatalogError::InvalidCatalog(message)
        | SkillCatalogError::InvalidInvocation(message) => message,
    };
    format!("Skill Invocation delivery failed: {cause}")
}

// One parameter per piece of actor state the event vocabulary reaches; the
// owning Session and a Subagent route project through the identical list.
#[allow(clippy::too_many_arguments)]
fn project_provider_event(
    sessions: &SessionStore,
    updates: &ProviderUpdateGate,
    session_id: SessionId,
    active: &mut ActiveProviderTurn,
    subagents: &mut SubagentRoutes,
    next_agent: &AgentIdentity,
    event: ProviderEvent,
) -> ProviderEventProjection {
    let Some(projected) = updates.apply(|| {
        let projection = match event {
            ProviderEvent::ContinuationStarted { selection } => {
                active.continuation = Some(ContinuationExecution::ProviderTurn);
                sessions.reconcile_effective_agent_selection(
                    session_id,
                    active.turn_id,
                    next_agent.agent.clone(),
                    selection,
                ).map(|_| ProviderEventProjection::Continue)
            }
            ProviderEvent::ApprovalRequested {
                approval,
                tool_activity_id,
            } => {
                let tool_activity_id = tool_activity_id
                    .as_ref()
                    .and_then(|native| active.tool_activity_id(native));
                active
                    .approvals
                    .register(
                        sessions,
                        session_id,
                        active.turn_id,
                        approval,
                        tool_activity_id,
                    )
                    .map(|_| ProviderEventProjection::Continue)
            }
            ProviderEvent::ApprovalWithdrawn { id } => {
                active.approvals.withdraw(id);
                let activity = sessions.snapshot(session_id).and_then(|snapshot| {
                    snapshot.activities.into_iter().find(|activity| {
                        matches!(
                            activity,
                            Activity::Approval {
                                approval,
                                outcome: crate::protocol::ApprovalOutcome::Pending
                                    | crate::protocol::ApprovalOutcome::SubmissionRejected
                                    | crate::protocol::ApprovalOutcome::Submitting,
                                ..
                            } if approval.id == id
                        )
                    })
                });
                match activity {
                    Some(activity) => sessions
                        .publish_agent_output(
                            session_id,
                            SessionChange::ApprovalSettled {
                                activity_id: activity.id(),
                                outcome: crate::protocol::ApprovalOutcome::Withdrawn,
                                decision: None,
                            },
                        )
                        .map(|_| ProviderEventProjection::Continue),
                    None => Ok(ProviderEventProjection::Continue),
                }
            }
            ProviderEvent::QuestionnaireRequested { questionnaire } => active
                .questionnaires
                .register(sessions, session_id, active.turn_id, questionnaire)
                .map(|_| ProviderEventProjection::Continue),
            ProviderEvent::QuestionnaireWithdrawn { id } => {
                active.questionnaires.withdraw(id);
                let activity = sessions.snapshot(session_id).and_then(|snapshot| {
                    snapshot.activities.into_iter().find(|activity| {
                        matches!(
                            activity,
                            Activity::Questionnaire {
                                questionnaire,
                                outcome: crate::protocol::QuestionnaireOutcome::Pending | crate::protocol::QuestionnaireOutcome::SubmissionRejected | crate::protocol::QuestionnaireOutcome::Submitting,
                                ..
                            } if questionnaire.id == id
                        )
                    })
                });
                match activity {
                    Some(activity) => sessions
                        .publish_agent_output(
                            session_id,
                            SessionChange::QuestionnaireSettled {
                                activity_id: activity.id(),
                                outcome: crate::protocol::QuestionnaireOutcome::Withdrawn,
                                answer: None,
                            },
                        )
                        .map(|_| ProviderEventProjection::Continue),
                    None => Ok(ProviderEventProjection::Continue),
                }
            }
            ProviderEvent::AgentSelectionChanged { selection } => sessions
                .reconcile_effective_agent_selection(
                    session_id,
                    active.turn_id,
                    next_agent.agent.clone(),
                    selection,
                )
                .map(|_| ProviderEventProjection::Continue),
            ProviderEvent::AgentMessageStarted => {
                if active.streaming_message.is_some() {
                    Err(anyhow::anyhow!(
                        "Provider started a second Agent Message before completing the first"
                    ))
                } else {
                    let message_id = MessageId::new();
                    active.streaming_message = Some(ActiveProviderMessage {
                        id: message_id,
                        normalizer: ProviderTextNormalizer::with_max_chars(
                            MAX_STORED_MESSAGE_CHARS,
                        ),
                    });
                    sessions
                        .publish_agent_output(
                            session_id,
                            SessionChange::MessageAdded {
                                message: Message {
                                    id: message_id,
                                    turn_id: active.turn_id,
                                    role: MessageRole::Agent,
                                    status: MessageStatus::Streaming,
                                    content: String::new(),
                                    skill_invocations: Vec::new(),
                                    truncated: false,
                                },
                            },
                        )
                        .map(|_| ProviderEventProjection::Continue)
                }
            }
            ProviderEvent::AgentMessageDelta { content } => {
                let Some(message) = active.streaming_message.as_mut() else {
                    return fail_invalid_provider_event(
                        sessions,
                        session_id,
                        active,
                        "Provider sent Agent Message content before starting a Message",
                    );
                };
                let content = message.normalizer.push(&content);
                sessions
                    .publish_agent_output_changes(
                        session_id,
                        message_content_changes(message.id, content),
                    )
                    .map(|_| ProviderEventProjection::Continue)
            }
            ProviderEvent::AgentMessageCompleted => {
                let Some(message) = active.streaming_message.take() else {
                    return fail_invalid_provider_event(
                        sessions,
                        session_id,
                        active,
                        "Provider completed an Agent Message before starting one",
                    );
                };
                sessions
                    .publish_agent_output(
                        session_id,
                        SessionChange::MessageCompleted {
                            message_id: message.id,
                        },
                    )
                    .map(|_| ProviderEventProjection::Continue)
            }
            ProviderEvent::CommandStarted {
                activity_id,
                command,
                cwd,
            } => {
                if active.claims_activity(&activity_id) {
                    Err(anyhow::anyhow!(
                        "Provider reused an active command Activity identity"
                    ))
                } else {
                    let command_activity_id = ActivityId::new();
                    sessions
                        .publish_agent_output(
                            session_id,
                            SessionChange::ActivityAdded {
                                activity: Activity::Command {
                                    id: command_activity_id,
                                    turn_id: active.turn_id,
                                    status: ActivityStatus::Active,
                                    command: normalize_provider_text(&command),
                                    cwd,
                                    output: String::new(),
                                    output_truncated: false,
                                    exit_status: None,
                                },
                            },
                        )
                        .map(|_| {
                            active.command_activities.insert(
                                activity_id,
                                ActiveProviderCommand {
                                    id: command_activity_id,
                                    output_normalizer: ProviderTextNormalizer::with_line_overwrite(
                                        MAX_STORED_COMMAND_OUTPUT_CHARS,
                                    ),
                                },
                            );
                            ProviderEventProjection::Continue
                        })
                }
            }
            ProviderEvent::CommandOutputDelta {
                activity_id,
                content,
            } => {
                let Some(command) = active.command_activities.get_mut(&activity_id) else {
                    return fail_invalid_provider_event(
                        sessions,
                        session_id,
                        active,
                        "Provider sent command output before starting the Activity",
                    );
                };
                let content = command.output_normalizer.push(&content);
                sessions
                    .publish_agent_output_changes(
                        session_id,
                        command_output_changes(command.id, content),
                    )
                    .map(|_| ProviderEventProjection::Continue)
            }
            ProviderEvent::CommandCompleted {
                activity_id,
                status,
                exit_status,
            } => {
                let Some(command) = active.command_activities.get_mut(&activity_id) else {
                    return fail_invalid_provider_event(
                        sessions,
                        session_id,
                        active,
                        "Provider completed a command before starting the Activity",
                    );
                };
                let command_activity_id = command.id;
                let trailing_output = command.output_normalizer.finish();
                if let Err(error) = sessions.publish_agent_output_changes(
                    session_id,
                    command_output_changes(command_activity_id, trailing_output),
                ) {
                    return finish_invalid_provider_event(
                        sessions,
                        session_id,
                        active,
                        failure_message("Provider execution failed", &error),
                    );
                }
                sessions
                    .publish_agent_output(
                        session_id,
                        SessionChange::CommandStatusChanged {
                            activity_id: command_activity_id,
                            status: match status {
                                ProviderCommandStatus::Completed => ActivityStatus::Completed,
                                ProviderCommandStatus::Failed => ActivityStatus::Failed,
                            },
                            exit_status,
                        },
                    )
                    .map(|_| {
                        active.command_activities.remove(&activity_id);
                        ProviderEventProjection::Continue
                    })
            }
            ProviderEvent::FileChangeStarted {
                activity_id,
                changes,
            } => {
                if active.claims_activity(&activity_id) {
                    Err(anyhow::anyhow!(
                        "Provider reused an active file-change Activity identity"
                    ))
                } else {
                    let file_change_activity_id = ActivityId::new();
                    sessions
                        .publish_agent_output(
                            session_id,
                            SessionChange::ActivityAdded {
                                activity: Activity::FileChange {
                                    id: file_change_activity_id,
                                    turn_id: active.turn_id,
                                    status: ActivityStatus::Active,
                                    changes,
                                },
                            },
                        )
                        .map(|_| {
                            active
                                .file_change_activities
                                .insert(activity_id, file_change_activity_id);
                            ProviderEventProjection::Continue
                        })
                }
            }
            ProviderEvent::FileChangeUpdated {
                activity_id,
                changes,
            } => {
                let Some(file_change_activity_id) =
                    active.file_change_activities.get(&activity_id).copied()
                else {
                    return fail_invalid_provider_event(
                        sessions,
                        session_id,
                        active,
                        "Provider updated file changes before starting the Activity",
                    );
                };
                sessions
                    .publish_agent_output(
                        session_id,
                        SessionChange::FileChangeUpdated {
                            activity_id: file_change_activity_id,
                            changes,
                        },
                    )
                    .map(|_| ProviderEventProjection::Continue)
            }
            ProviderEvent::FileChangeCompleted {
                activity_id,
                status,
            } => {
                let Some(file_change_activity_id) =
                    active.file_change_activities.get(&activity_id).copied()
                else {
                    return fail_invalid_provider_event(
                        sessions,
                        session_id,
                        active,
                        "Provider completed file changes before starting the Activity",
                    );
                };
                sessions
                    .publish_agent_output(
                        session_id,
                        SessionChange::FileChangeStatusChanged {
                            activity_id: file_change_activity_id,
                            status: match status {
                                ProviderFileChangeStatus::Completed => ActivityStatus::Completed,
                                ProviderFileChangeStatus::Failed => ActivityStatus::Failed,
                            },
                        },
                    )
                    .map(|_| {
                        active.file_change_activities.remove(&activity_id);
                        ProviderEventProjection::Continue
                    })
            }
            ProviderEvent::ReasoningStarted { activity_id } => {
                if active.claims_activity(&activity_id) {
                    Err(anyhow::anyhow!(
                        "Provider reused an active Reasoning Activity identity"
                    ))
                } else {
                    let reasoning_activity_id = ActivityId::new();
                    sessions
                        .publish_agent_output(
                            session_id,
                            SessionChange::ActivityAdded {
                                activity: Activity::Reasoning {
                                    id: reasoning_activity_id,
                                    turn_id: active.turn_id,
                                    status: ActivityStatus::Active,
                                    title: None,
                                    content: String::new(),
                                    content_truncated: false,
                                    duration_ms: None,
                                },
                            },
                        )
                        .map(|_| {
                            active.reasoning_activities.insert(
                                activity_id,
                                ActiveProviderReasoning {
                                    id: reasoning_activity_id,
                                    started: Instant::now(),
                                    normalizer: ProviderTextNormalizer::with_max_chars(
                                        MAX_STORED_REASONING_CHARS,
                                    ),
                                },
                            );
                            ProviderEventProjection::Continue
                        })
                }
            }
            ProviderEvent::ReasoningTitleChanged { activity_id, title } => {
                let Some(reasoning) = active.reasoning_activities.get(&activity_id) else {
                    return fail_invalid_provider_event(
                        sessions,
                        session_id,
                        active,
                        "Provider titled Reasoning before starting the Activity",
                    );
                };
                sessions
                    .publish_agent_output(
                        session_id,
                        SessionChange::ReasoningTitleChanged {
                            activity_id: reasoning.id,
                            title: normalize_provider_text(&title),
                        },
                    )
                    .map(|_| ProviderEventProjection::Continue)
            }
            ProviderEvent::ReasoningDelta {
                activity_id,
                content,
            } => {
                let Some(reasoning) = active.reasoning_activities.get_mut(&activity_id) else {
                    return fail_invalid_provider_event(
                        sessions,
                        session_id,
                        active,
                        "Provider sent Reasoning content before starting the Activity",
                    );
                };
                let content = reasoning.normalizer.push(&content);
                sessions
                    .publish_agent_output_changes(
                        session_id,
                        reasoning_content_changes(reasoning.id, content),
                    )
                    .map(|_| ProviderEventProjection::Continue)
            }
            ProviderEvent::ReasoningCompleted { activity_id } => {
                let Some(reasoning) = active.reasoning_activities.get(&activity_id) else {
                    return fail_invalid_provider_event(
                        sessions,
                        session_id,
                        active,
                        "Provider completed Reasoning before starting the Activity",
                    );
                };
                let reasoning_activity_id = reasoning.id;
                let duration_ms = u64::try_from(reasoning.started.elapsed().as_millis()).ok();
                sessions
                    .publish_agent_output(
                        session_id,
                        SessionChange::ReasoningStatusChanged {
                            activity_id: reasoning_activity_id,
                            status: ActivityStatus::Completed,
                            duration_ms,
                        },
                    )
                    .map(|_| {
                        active.reasoning_activities.remove(&activity_id);
                        ProviderEventProjection::Continue
                    })
            }
            ProviderEvent::SubagentStarted {
                subagent_id,
                name,
                description,
                delegation,
            } => subagents
                .spawn(
                    sessions,
                    (session_id, active.turn_id),
                    subagent_id,
                    &name,
                    &description,
                    delegation.as_deref(),
                )
                .map(|()| ProviderEventProjection::Continue),
            ProviderEvent::SubagentResumed {
                subagent_id,
                name,
                description,
                delegation,
            } => subagents
                .resume(
                    sessions,
                    (session_id, active.turn_id),
                    subagent_id,
                    &name,
                    &description,
                    delegation.as_deref(),
                )
                .map(|()| ProviderEventProjection::Continue),
            ProviderEvent::SubagentUpdated {
                subagent_id,
                description,
            } => match subagents.update_row(sessions, &subagent_id, &description) {
                // A Subagent Suru stopped can still trail the Provider's
                // account of it; that is a late echo to discard, not an
                // update to a Subagent never spawned.
                None if subagents.was_stopped(&subagent_id) => {
                    Ok(ProviderEventProjection::Continue)
                }
                None => {
                    return fail_invalid_provider_event(
                        sessions,
                        session_id,
                        active,
                        "Provider updated a Subagent before spawning it",
                    );
                }
                Some(updated) => updated.map(|()| ProviderEventProjection::Continue),
            },
            ProviderEvent::SubagentModelChanged { subagent_id, model } => {
                match subagents.update_model(sessions, &subagent_id, next_agent, model) {
                    None if subagents.was_stopped(&subagent_id) => {
                        Ok(ProviderEventProjection::Continue)
                    }
                    None => return fail_invalid_provider_event(
                        sessions,
                        session_id,
                        active,
                        "Provider identified a Subagent Model before spawning it",
                    ),
                    Some(updated) => updated.map(|()| ProviderEventProjection::Continue),
                }
            }
            ProviderEvent::SubagentCompleted {
                subagent_id,
                status,
            } => match subagents.settle_subagent(sessions, &subagent_id, status) {
                // The settle a stopped Subagent still owed arrives as a late
                // echo: Suru already settled the row, so there is nothing
                // left for the Provider's own account to close.
                None if subagents.was_stopped(&subagent_id) => {
                    Ok(ProviderEventProjection::Continue)
                }
                None => {
                    return fail_invalid_provider_event(
                        sessions,
                        session_id,
                        active,
                        "Provider settled a Subagent before spawning it",
                    );
                }
                Some(settled) => settled.map(|()| ProviderEventProjection::Continue),
            },
            ProviderEvent::ContextFill { report } => sessions
                .report_context_fill(session_id, active.turn_id, report)
                .map(|()| ProviderEventProjection::Continue),
            // The actor stores a revised Resume State for the owning Session
            // before anything projects, so none ever lands in a Turn.
            ProviderEvent::ResumeStateChanged { .. } => Ok(ProviderEventProjection::Continue),
            ProviderEvent::Usage { usage, cost } => sessions
                .publish_agent_output(
                    session_id,
                    SessionChange::TurnUsageChanged {
                        turn_id: active.turn_id,
                        usage,
                        cost: cost.as_ref().map(MeteredCost::cost),
                        cost_basis: cost.as_ref().map(MeteredCost::basis),
                        cost_coverage: cost.as_ref().map(|cost| cost.coverage().clone()),
                        cost_is_partial: cost.as_ref().is_some_and(MeteredCost::is_partial),
                        cost_recorded_at: None,
                    },
                )
                .map(|_| ProviderEventProjection::Continue),
            ProviderEvent::TurnCompleted => {
                // A Provider may reach its own boundary with streams it never
                // settled — Codex completes a Turn without the `item/completed`
                // an open command or Agent Message was owed. Settling the Turn
                // settles them, the way an interruption does: the Turn is the
                // outer boundary, and losing it over a stream the reader
                // already watched arrive would cost them the answer it led to.
                //
                // A Subagent still working holds nothing open here: the Turn
                // settles at the Provider's own boundary (ADR 0015), and the
                // rows and routes live on at the connection until each
                // Subagent's own settle arrives.
                let trailing_output = active.take_trailing_output();
                sessions
                    .finish_provider_turn(
                        session_id,
                        active.turn_id,
                        ProviderTurnOutcome::Completed { trailing_output },
                    )
                    .map(|()| ProviderEventProjection::Terminal)
            }
            ProviderEvent::TurnInterrupted => {
                let trailing_output = active.take_trailing_output();
                sessions
                    .finish_provider_turn(
                        session_id,
                        active.turn_id,
                        ProviderTurnOutcome::Interrupted { trailing_output },
                    )
                    .map(|()| ProviderEventProjection::Terminal)
            }
            ProviderEvent::AgentSelectionRejected { message } => {
                let trailing_output = active.take_trailing_output();
                sessions
                    .reject_agent_selection(
                        session_id,
                        active.turn_id,
                        trailing_output,
                        normalize_provider_text(&message),
                    )
                    .map(|_| ProviderEventProjection::Terminal)
            }
            ProviderEvent::TurnFailed { message } => {
                let trailing_output = active.take_trailing_output();
                sessions
                    .finish_provider_turn(
                        session_id,
                        active.turn_id,
                        ProviderTurnOutcome::Failed {
                            trailing_output,
                            message: normalize_provider_text(&message),
                        },
                    )
                    .map(|()| ProviderEventProjection::Terminal)
            }
        };

        projection.unwrap_or_else(|error| {
            finish_invalid_provider_event(
                sessions,
                session_id,
                active,
                failure_message("Provider execution failed", &error),
            )
        })
    }) else {
        return ProviderEventProjection::Terminal;
    };
    projected
}

/// A Delegation's text as the Subagent's Transcript stores it: normalized
/// and capped on the same terms as an Agent Message, since it is prose one
/// Agent wrote for another. Text with nothing to read stands as no Message.
fn delegation_text(text: &str) -> Option<NormalizedText> {
    if text.trim().is_empty() {
        return None;
    }
    Some(ProviderTextNormalizer::with_max_chars(MAX_STORED_MESSAGE_CHARS).push(text))
}

/// Stores the Resume State a Provider revised mid-connection for the Session
/// that owns the connection. A write that fails costs only what the revision
/// added — the next start resumes from the state stored before it — so it is
/// logged rather than failing whatever the connection is doing.
fn save_revised_resume_state(
    sessions: &SessionStore,
    updates: &ProviderUpdateGate,
    session_id: SessionId,
    provider_id: &ProviderId,
    resume_state: ProviderResumeState,
) {
    if let Some(Err(error)) =
        updates.apply(|| sessions.save_resume_state(session_id, provider_id.clone(), resume_state))
    {
        tracing::warn!(%session_id, "a revised Resume State could not be saved: {error:#}");
    }
}

/// Adds the row one stretch of a Subagent's work stands as to the delegating
/// Turn's Transcript, leading into the Subagent's own Session, and answers
/// the row's identity.
fn add_subagent_row(
    sessions: &SessionStore,
    (owner_session_id, turn_id): (SessionId, TurnId),
    name: &str,
    description: String,
    session_id: SessionId,
) -> anyhow::Result<ActivityId> {
    let activity_id = ActivityId::new();
    sessions.publish_agent_output(
        owner_session_id,
        SessionChange::ActivityAdded {
            activity: Activity::Subagent {
                id: activity_id,
                turn_id,
                status: ActivityStatus::Active,
                name: name.to_owned(),
                description,
                model: None,
                session_id,
                duration_ms: None,
            },
        },
    )?;
    Ok(activity_id)
}

/// Renders a Provider failure as the message the Turn or Prompt carries. Every
/// failure reads the same way — what failed, then what went wrong — and is
/// normalized, because the cause is usually the Provider's own text and can
/// carry the escape sequences Provider output carries.
fn failure_message(failure: &str, cause: &impl Display) -> String {
    normalize_provider_text(&format!("{failure}: {cause}"))
}

/// The message a Provider startup failure carries. A failure naming a typed
/// condition the user fixes outside Suru — an uninstalled CLI, an account not
/// signed in — leads with that condition, so the Turn says what to do about it
/// rather than only that a launch went wrong.
fn startup_failure_message(provider: &ProviderId, error: &ProviderError) -> String {
    match error.unavailability() {
        Some(reason) => failure_message(
            &format!("Provider `{provider}` is {}", reason.label()),
            error,
        ),
        None => failure_message("Provider startup failed", error),
    }
}

fn fail_invalid_provider_event(
    sessions: &SessionStore,
    session_id: SessionId,
    active: &mut ActiveProviderTurn,
    message: &str,
) -> ProviderEventProjection {
    finish_invalid_provider_event(
        sessions,
        session_id,
        active,
        format!("Provider execution failed: {message}"),
    )
}

fn finish_invalid_provider_event(
    sessions: &SessionStore,
    session_id: SessionId,
    active: &mut ActiveProviderTurn,
    message: String,
) -> ProviderEventProjection {
    let trailing_output = active.take_trailing_output();
    let _ = sessions.finish_provider_turn(
        session_id,
        active.turn_id,
        ProviderTurnOutcome::Failed {
            trailing_output,
            message,
        },
    );
    ProviderEventProjection::Terminal
}

/// The routing seam is proven here on its own terms, apart from the spawn
/// that establishes routes in production: these tests are the contract the
/// Subagent lifecycle builds on, and the wire-driven suites prove that
/// lifecycle end to end. The store, the commit, and the subscription are all
/// real — only the route is placed by hand.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        AgentId, AgentSelection, CreateSessionRequest, InitialPrompt, MessageRole, MessageStatus,
        ModelId, SessionSnapshot, TranscriptItem, TurnStatus,
    };
    use crate::sessions::StoreOutcome;
    use crate::storage::{StorageRepository, StorageWriter};

    struct RoutedSessions {
        sessions: SessionStore,
        /// The Session whose Prompt started the work — the one every event
        /// used to be assumed to belong to.
        owning: SessionId,
        /// Another Session entirely, standing where a Subagent's will.
        routed: SessionId,
        routed_turn: TurnId,
        _writer: StorageWriter,
        _data_dir: tempfile::TempDir,
        _workspace: tempfile::TempDir,
    }

    async fn routed_sessions() -> RoutedSessions {
        let data_dir = tempfile::tempdir().expect("create isolated data directory");
        let execution_directory = tempfile::tempdir().expect("create valid Workspace");
        let repository = StorageRepository::open(data_dir.path())
            .await
            .expect("open Session repository");
        let (writer, storage) = StorageWriter::spawn(repository, &[]);
        let sessions =
            SessionStore::new(Default::default(), storage, Vec::new(), Default::default());
        let owning = create_session(
            &sessions,
            execution_directory.path(),
            "The owning conversation",
        );
        let routed = create_session(&sessions, execution_directory.path(), "Delegated work");
        let routed_prompt = routed.prompts[0].id;
        let routed_id = routed.session.id;
        let delivered = sessions
            .deliver_prompt(routed_id, routed_prompt, None, DeliveredTurnStatus::Active)
            .expect("deliver the routed Session's Prompt")
            .expect("the routed Session has no other active Turn");
        RoutedSessions {
            sessions,
            owning: owning.session.id,
            routed: routed_id,
            routed_turn: delivered.turn_id,
            _writer: writer,
            _data_dir: data_dir,
            _workspace: execution_directory,
        }
    }

    fn create_session(
        sessions: &SessionStore,
        execution_directory: &std::path::Path,
        prompt: &str,
    ) -> SessionSnapshot {
        let created = sessions
            .create(CreateSessionRequest {
                preparation_id: None,
                agent_selection: None,
                execution_directory: crate::protocol::ExecutionDirectory {
                    path: execution_directory.to_owned(),
                },
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: prompt.to_owned(),
                    skill_invocations: Vec::new(),
                },
            })
            .expect("create Session");
        let StoreOutcome::Created(snapshot) = created else {
            panic!("a fresh Prompt creates a Session");
        };
        snapshot
    }

    fn provider_identity() -> AgentIdentity {
        AgentIdentity {
            agent: AgentId::new("controlled"),
            selection: AgentSelection {
                provider: ProviderId::new("controlled"),
                model: ModelId::new("model"),
                options: Vec::new(),
            },
        }
    }

    fn turn_state(turn_id: TurnId) -> ActiveProviderTurn {
        ActiveProviderTurn::new(turn_id)
    }

    fn subagent_routes() -> SubagentRoutes {
        SubagentRoutes::new(ProviderId::new("controlled"))
    }

    fn route_to(
        routes: &mut SubagentRoutes,
        subagent: &ProviderSubagentId,
        fixture: &RoutedSessions,
    ) {
        routes.routes.insert(
            subagent.clone(),
            SubagentRoute {
                session_id: fixture.routed,
                turn: turn_state(fixture.routed_turn),
            },
        );
    }

    #[tokio::test]
    async fn a_subagent_attributed_event_lands_in_the_session_its_route_names() {
        let fixture = routed_sessions().await;
        let updates = ProviderUpdateGate::new();
        let identity = provider_identity();
        let subagent = ProviderSubagentId::new("delegation-1");
        let mut routes = subagent_routes();
        route_to(&mut routes, &subagent, &fixture);
        let owning_before = fixture
            .sessions
            .snapshot(fixture.owning)
            .expect("owning Session exists")
            .revision;
        let mut feed = fixture
            .sessions
            .subscribe(fixture.routed)
            .expect("routed Session exists");

        for event in [
            ProviderEvent::AgentMessageStarted,
            ProviderEvent::AgentMessageDelta {
                content: "Delegated hello".to_owned(),
            },
            ProviderEvent::AgentMessageCompleted,
        ] {
            routes.project_event(&fixture.sessions, &updates, &identity, &subagent, event);
        }

        let routed = fixture
            .sessions
            .snapshot(fixture.routed)
            .expect("routed Session exists");
        let message = routed
            .messages
            .iter()
            .find(|message| message.role == MessageRole::Agent)
            .expect("the routed Session carries the Subagent's Message");
        assert_eq!(message.content, "Delegated hello");
        assert_eq!(message.status, MessageStatus::Completed);
        assert_eq!(message.turn_id, fixture.routed_turn);
        assert!(
            routed.revision > feed.snapshot.revision,
            "the commit path advances the routed Session's revision"
        );
        let update = feed
            .updates
            .try_recv()
            .expect("the routed Session's subscribers hear the commit");
        assert_eq!(update.session_id, fixture.routed);
        assert!(update.revision.immediately_follows(feed.snapshot.revision));
        assert_eq!(
            fixture
                .sessions
                .snapshot(fixture.owning)
                .expect("owning Session exists")
                .revision,
            owning_before,
            "nothing lands in the owning Session"
        );
    }

    #[tokio::test]
    async fn a_subagent_terminal_event_settles_its_routed_turn_and_drops_the_route() {
        let fixture = routed_sessions().await;
        let updates = ProviderUpdateGate::new();
        let identity = provider_identity();
        let subagent = ProviderSubagentId::new("delegation-1");
        let mut routes = subagent_routes();
        route_to(&mut routes, &subagent, &fixture);

        routes.project_event(
            &fixture.sessions,
            &updates,
            &identity,
            &subagent,
            ProviderEvent::TurnCompleted,
        );

        let routed = fixture
            .sessions
            .snapshot(fixture.routed)
            .expect("routed Session exists");
        let turn = routed
            .turns
            .iter()
            .find(|turn| turn.id == fixture.routed_turn)
            .expect("the routed Turn exists");
        assert_eq!(turn.status, TurnStatus::Completed);
        assert!(
            routes.routes.is_empty(),
            "a settled route has nothing left to receive"
        );

        routes.project_event(
            &fixture.sessions,
            &updates,
            &identity,
            &subagent,
            ProviderEvent::AgentMessageStarted,
        );
        assert_eq!(
            fixture
                .sessions
                .snapshot(fixture.routed)
                .expect("routed Session exists")
                .revision,
            routed.revision,
            "an event for a dropped route lands nowhere"
        );
    }

    /// A Provider may reach its own Turn boundary with streams it never
    /// settled — Codex completes a Turn without the `item/completed` its open
    /// command or Agent Message was owed. Those streams are Suru's to close,
    /// the way an interruption closes them; losing the Turn over them would
    /// cost the reader the answer it already watched arrive.
    #[tokio::test]
    async fn a_turn_the_provider_completes_settles_the_streams_it_left_open() {
        let fixture = routed_sessions().await;
        let updates = ProviderUpdateGate::new();
        let identity = provider_identity();
        let subagent = ProviderSubagentId::new("delegation-1");
        let mut routes = subagent_routes();
        route_to(&mut routes, &subagent, &fixture);
        let command = super::super::ProviderActivityId::new("exec-1");

        for event in [
            ProviderEvent::CommandStarted {
                activity_id: command.clone(),
                command: "cargo test".to_owned(),
                cwd: None,
            },
            ProviderEvent::CommandOutputDelta {
                activity_id: command.clone(),
                // The second line is unterminated, so only this actor's
                // normalizer holds it when the Turn settles.
                content: "   Compiling suru
running 1 test"
                    .to_owned(),
            },
            ProviderEvent::AgentMessageStarted,
            ProviderEvent::AgentMessageDelta {
                content: "The tests pass.".to_owned(),
            },
            ProviderEvent::TurnCompleted,
        ] {
            routes.project_event(&fixture.sessions, &updates, &identity, &subagent, event);
        }

        let routed = fixture
            .sessions
            .snapshot(fixture.routed)
            .expect("routed Session exists");
        let turn = routed
            .turns
            .iter()
            .find(|turn| turn.id == fixture.routed_turn)
            .expect("the routed Turn exists");
        assert_eq!(
            turn.status,
            TurnStatus::Completed,
            "an unsettled stream does not cost the Turn"
        );
        assert!(
            !routed
                .activities
                .iter()
                .any(|activity| matches!(activity, Activity::Error { .. })),
            "nothing failed, so the Transcript says nothing failed"
        );
        let Some(Activity::Command { status, output, .. }) = routed
            .activities
            .iter()
            .find(|activity| matches!(activity, Activity::Command { .. }))
        else {
            panic!("the command the Provider left open is in the Transcript");
        };
        assert_ne!(
            *status,
            ActivityStatus::Active,
            "the Turn's settle closes the command stream"
        );
        assert_eq!(
            output,
            "   Compiling suru
running 1 test",
            "the line only this actor held is stored before the stream closes"
        );
        let message = routed
            .messages
            .iter()
            .find(|message| message.role == MessageRole::Agent)
            .expect("the Agent Message the Provider left open is in the Transcript");
        assert_eq!(message.status, MessageStatus::Completed);
        assert_eq!(message.content, "The tests pass.");
    }

    #[tokio::test]
    async fn an_event_attributed_to_an_unknown_subagent_lands_nowhere() {
        let fixture = routed_sessions().await;
        let updates = ProviderUpdateGate::new();
        let identity = provider_identity();
        let mut routes = subagent_routes();
        let owning_before = fixture
            .sessions
            .snapshot(fixture.owning)
            .expect("owning Session exists")
            .revision;
        let routed_before = fixture
            .sessions
            .snapshot(fixture.routed)
            .expect("routed Session exists")
            .revision;

        routes.project_event(
            &fixture.sessions,
            &updates,
            &identity,
            &ProviderSubagentId::new("never-announced"),
            ProviderEvent::AgentMessageStarted,
        );

        assert_eq!(
            fixture
                .sessions
                .snapshot(fixture.owning)
                .expect("owning Session exists")
                .revision,
            owning_before
        );
        assert_eq!(
            fixture
                .sessions
                .snapshot(fixture.routed)
                .expect("routed Session exists")
                .revision,
            routed_before
        );
    }

    #[tokio::test]
    async fn tearing_down_the_connection_fails_every_routed_turn() {
        let fixture = routed_sessions().await;
        let updates = ProviderUpdateGate::new();
        let subagent = ProviderSubagentId::new("delegation-1");
        let mut routes = subagent_routes();
        route_to(&mut routes, &subagent, &fixture);

        routes.fail_all(
            &fixture.sessions,
            &updates,
            SUBAGENT_CONNECTION_LOST_MESSAGE,
            OpenInterventions::TurnEnded,
        );

        let routed = fixture
            .sessions
            .snapshot(fixture.routed)
            .expect("routed Session exists");
        let turn = routed
            .turns
            .iter()
            .find(|turn| turn.id == fixture.routed_turn)
            .expect("the routed Turn exists");
        assert_eq!(turn.status, TurnStatus::Failed);
        assert!(
            routed.activities.iter().any(|activity| matches!(
                activity,
                Activity::Error { text, .. } if text == SUBAGENT_CONNECTION_LOST_MESSAGE
            )),
            "the routed Turn says why it failed"
        );
        assert!(routes.routes.is_empty());
    }

    /// A settled Subagent woken by its own work runs on in a Continuation of
    /// its Session. A resume arriving then is a Delegation that begins a Turn
    /// of its own, so the Continuation settles first — as worked, keeping the
    /// line only this actor still held — and the resume's Turn begins after it.
    #[tokio::test]
    async fn a_resume_settles_a_continuation_open_in_the_subagents_session_before_its_turn_begins()
    {
        let fixture = routed_sessions().await;
        let updates = ProviderUpdateGate::new();
        let identity = provider_identity();
        let subagent = ProviderSubagentId::new("task-1");
        let delegating = (fixture.routed, fixture.routed_turn);
        let mut routes = subagent_routes();
        routes
            .spawn(
                &fixture.sessions,
                delegating,
                subagent.clone(),
                "Explore",
                "Map the seams",
                Some("Map the provider seams."),
            )
            .expect("spawn the Subagent");
        let child = routes.identities[&subagent].session_id;
        routes
            .settle_subagent(
                &fixture.sessions,
                &subagent,
                ProviderSubagentStatus::Completed,
            )
            .expect("the spawn's row is open")
            .expect("settle the spawn's stretch");
        let continuation = fixture
            .sessions
            .begin_continuation(child, identity.clone())
            .expect("the Subagent's own work begins a Continuation");
        routes.routes.insert(
            subagent.clone(),
            SubagentRoute {
                session_id: child,
                turn: ActiveProviderTurn::new_continuation(continuation),
            },
        );
        let command = super::super::ProviderActivityId::new("exec-1");
        for event in [
            ProviderEvent::CommandStarted {
                activity_id: command.clone(),
                command: "cargo test".to_owned(),
                cwd: None,
            },
            ProviderEvent::CommandOutputDelta {
                activity_id: command,
                content: "running 1 test".to_owned(),
            },
        ] {
            routes.project_event(&fixture.sessions, &updates, &identity, &subagent, event);
        }

        routes
            .resume(
                &fixture.sessions,
                delegating,
                subagent.clone(),
                "Explore",
                "Map the tests too",
                Some("Now map the tests too."),
            )
            .expect("resume the Subagent");

        let child = fixture
            .sessions
            .snapshot(child)
            .expect("the Subagent's Session exists");
        let statuses = child
            .turns
            .iter()
            .map(|turn| turn.status)
            .collect::<Vec<_>>();
        assert_eq!(
            statuses,
            [
                TurnStatus::Completed,
                TurnStatus::Completed,
                TurnStatus::Active
            ],
            "the Continuation settles as worked before the resume's Turn begins"
        );
        assert_eq!(child.turns[1].id, continuation);
        let Some(Activity::Command { status, output, .. }) = child
            .activities
            .iter()
            .find(|activity| matches!(activity, Activity::Command { .. }))
        else {
            panic!("the Continuation's command is in the Transcript");
        };
        assert_ne!(*status, ActivityStatus::Active);
        assert_eq!(output, "running 1 test");
        let delegations = child
            .messages
            .iter()
            .filter(|message| message.role.delegator().is_some())
            .map(|message| (message.turn_id, message.content.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            delegations,
            [
                (child.turns[0].id, "Map the provider seams."),
                (child.turns[2].id, "Now map the tests too."),
            ],
            "the spawn's and the resume's Delegations each open the Turn they began"
        );
        let Some(TranscriptItem::Message { message_id }) = child.transcript.last() else {
            panic!("the resume's Delegation stands after the Continuation's work");
        };
        assert_eq!(
            child.messages.last().map(|message| message.id),
            Some(*message_id)
        );
        let route = &routes.routes[&subagent];
        assert_eq!(route.session_id, child.session.id);
        assert_eq!(
            route.turn.turn_id, child.turns[2].id,
            "the Subagent's events now land in the resume's Turn"
        );
        assert!(route.turn.continuation.is_none());
    }

    /// A delivered Prompt's Turn in a fresh Session, for a Subagent to be
    /// delegated from.
    fn delegating_turn(
        sessions: &SessionStore,
        execution_directory: &std::path::Path,
        prompt: &str,
    ) -> (SessionId, TurnId) {
        let created = create_session(sessions, execution_directory, prompt);
        let delivered = sessions
            .deliver_prompt(
                created.session.id,
                created.prompts[0].id,
                None,
                DeliveredTurnStatus::Active,
            )
            .expect("deliver the Session's Prompt")
            .expect("the Session has no other active Turn");
        (created.session.id, delivered.turn_id)
    }

    fn subagent_rows(snapshot: &SessionSnapshot) -> Vec<(&str, SessionId)> {
        snapshot
            .activities
            .iter()
            .filter_map(|activity| match activity {
                Activity::Subagent {
                    name, session_id, ..
                } => Some((name.as_str(), *session_id)),
                _ => None,
            })
            .collect()
    }

    /// The store a restart reads back, rather than a live one: the Subagent's
    /// identity reaches the next process only through storage, and the
    /// connection that process opens relearns it from there.
    #[tokio::test]
    async fn a_connection_resumed_over_a_restored_store_continues_the_session_its_subagent_left() {
        let data_dir = tempfile::tempdir().expect("create isolated data directory");
        let workspace = tempfile::tempdir().expect("create valid Workspace");
        let repository = StorageRepository::open(data_dir.path())
            .await
            .expect("open Session repository");
        let subagent = ProviderSubagentId::new("task-1");
        let (owning, child) = {
            let (writer, storage) = StorageWriter::spawn(repository.clone(), &[]);
            let sessions =
                SessionStore::new(Default::default(), storage, Vec::new(), Default::default());
            let delegating = delegating_turn(&sessions, workspace.path(), "Delegate the map");
            let mut routes = subagent_routes();
            routes
                .spawn(
                    &sessions,
                    delegating,
                    subagent.clone(),
                    "Explore",
                    "Map the seams",
                    Some("Map the provider seams."),
                )
                .expect("spawn the Subagent");
            routes
                .settle_subagent(&sessions, &subagent, ProviderSubagentStatus::Completed)
                .expect("the spawn's row is open")
                .expect("settle the spawn's stretch");
            let child = routes.identities[&subagent].session_id;
            drop(sessions);
            writer
                .shutdown()
                .await
                .expect("flush the first process's store");
            (delegating.0, child)
        };

        let (writer, storage) = StorageWriter::spawn(repository.clone(), &[]);
        let restored = repository
            .load_sessions()
            .await
            .expect("read the store back");
        let sessions = SessionStore::new(restored, storage, Vec::new(), Default::default());
        sessions.hydrate(owning).await.expect("hydrate the tree");
        let resuming = sessions
            .begin_continuation(owning, provider_identity())
            .expect("the resumed conversation works again");
        let mut routes = subagent_routes();
        routes.restore(sessions.stored_subagents(owning, &ProviderId::new("controlled")));
        routes
            .resume(
                &sessions,
                (owning, resuming),
                subagent.clone(),
                "general-purpose",
                "Map the tests too",
                Some("Now map the tests too."),
            )
            .expect("resume the Subagent");

        let child = sessions
            .snapshot(child)
            .expect("the Subagent's Session exists");
        let statuses = child
            .turns
            .iter()
            .map(|turn| turn.status)
            .collect::<Vec<_>>();
        assert_eq!(
            statuses,
            [TurnStatus::Completed, TurnStatus::Active],
            "the resume begins the next Turn in the Session its spawn opened"
        );
        let parent = sessions
            .snapshot(owning)
            .expect("the owning Session exists");
        assert_eq!(
            subagent_rows(&parent),
            [("Explore", child.session.id), ("Explore", child.session.id)],
            "both rows lead into the one Session, named as the spawn named it"
        );
        assert_eq!(routes.routes[&subagent].turn.turn_id, child.turns[1].id);
        let delegations = child
            .messages
            .iter()
            .filter(|message| matches!(message.role, MessageRole::Delegation(_)))
            .map(|message| (message.content.as_str(), message.turn_id))
            .collect::<Vec<_>>();
        assert_eq!(
            delegations,
            [
                ("Map the provider seams.", child.turns[0].id),
                ("Now map the tests too.", child.turns[1].id)
            ],
            "the resumed Turn opens with its Delegation, as the spawn's did"
        );
        writer.shutdown().await.expect("stop the writer");
    }

    /// Identities are each Provider connection's own to mint, and a
    /// connection belongs to one top-level Session: another tree holding the
    /// same identity — its own connection's — is never where a resume lands.
    #[tokio::test]
    async fn a_connection_relearns_only_the_subagents_of_its_own_tree() {
        let data_dir = tempfile::tempdir().expect("create isolated data directory");
        let workspace = tempfile::tempdir().expect("create valid Workspace");
        let repository = StorageRepository::open(data_dir.path())
            .await
            .expect("open Session repository");
        let (writer, storage) = StorageWriter::spawn(repository, &[]);
        let sessions =
            SessionStore::new(Default::default(), storage, Vec::new(), Default::default());
        let subagent = ProviderSubagentId::new("task-1");
        let nested = ProviderSubagentId::new("task-2");
        let mut children = Vec::new();
        for prompt in ["One tree", "Another tree"] {
            let delegating = delegating_turn(&sessions, workspace.path(), prompt);
            let mut routes = subagent_routes();
            routes
                .spawn(
                    &sessions,
                    delegating,
                    subagent.clone(),
                    "Explore",
                    prompt,
                    None,
                )
                .expect("spawn the Subagent");
            let child = routes.identities[&subagent].session_id;
            let child_turn = routes.identities[&subagent]
                .stretch
                .expect("the spawn begins a stretch")
                .turn_id;
            routes
                .spawn(
                    &sessions,
                    (child, child_turn),
                    nested.clone(),
                    "Plan",
                    prompt,
                    None,
                )
                .expect("spawn a nested Subagent");
            let grandchild = routes.identities[&nested].session_id;
            children.push((delegating.0, child, grandchild));
        }
        let [(_, _, _), (other, other_child, other_grandchild)] = children[..] else {
            unreachable!()
        };

        let stored = sessions.stored_subagents(other, &ProviderId::new("controlled"));

        assert_eq!(
            stored
                .iter()
                .map(|subagent| (
                    subagent.subagent_id.as_str(),
                    subagent.session_id,
                    subagent.spawner,
                    subagent.name.as_str()
                ))
                .collect::<Vec<_>>(),
            [
                ("task-1", other_child, other, "Explore"),
                ("task-2", other_grandchild, other_child, "Plan"),
            ],
            "the tree's own Subagents, a nested one's included, and none of the other tree's"
        );
        assert!(
            sessions
                .stored_subagents(other, &ProviderId::new("another-provider"))
                .is_empty(),
            "another Provider's connection names none of them"
        );
        writer.shutdown().await.expect("stop the writer");
    }
}
