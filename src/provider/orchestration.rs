use std::{
    collections::{HashMap, HashSet},
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
    AttributedProviderEvent, ProviderCommandStatus, ProviderError, ProviderEvent,
    ProviderEventAttribution, ProviderEventStream, ProviderFileChangeStatus, ProviderPrompt,
    ProviderRuntime, ProviderSession, ProviderSessionRequest, ProviderSteerInput,
    ProviderSubagentId, ProviderSubagentStatus, ProviderTurnInput,
};
use crate::ansi::{ProviderTextNormalizer, normalize_provider_text};
use crate::protocol::{
    Activity, ActivityId, ActivityStatus, AgentIdentity, Message, MessageId, MessageRole,
    MessageStatus, Prompt, PromptDelivery, PromptId, PromptOrder, PromptStatus, ProviderId,
    SessionChange, SessionId, SettingsSnapshot, SkillPromptDelivery, TurnId,
};
use crate::sessions::{
    DeliveredTurn, DeliveredTurnStatus, InterruptSessionError, InterruptTarget,
    ProviderTurnOutcome, QueuedPromptDisposition, SessionStore, TrailingCommandOutput,
    command_output_changes, earliest_pending_prompt, message_content_changes,
    reasoning_content_changes,
};
use crate::skill_catalog::{SkillCatalogError, SkillCatalogService};

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
}

struct ConnectedProviderSession {
    identity: AgentIdentity,
    session: Arc<dyn ProviderSession>,
    events: ProviderEventStream,
}

struct ProviderSessionContext {
    runtime: Arc<dyn ProviderRuntime>,
    sessions: SessionStore,
    skill_catalog: SkillCatalogService,
    session_id: SessionId,
    workspace: PathBuf,
    updates: ProviderUpdateGate,
}

struct ActiveProviderTurn {
    turn_id: TurnId,
    /// Whether this Turn is a Continuation — the one kind of Turn no Prompt
    /// began, opened by this actor for output that arrived after the previous
    /// Turn settled. The next delivered Prompt settles it rather than
    /// steering it, which is the one place the flag is consulted.
    continuation: bool,
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
            continuation: false,
            streaming_message: None,
            interruption_acknowledged: false,
            command_activities: HashMap::new(),
            file_change_activities: HashMap::new(),
            reasoning_activities: HashMap::new(),
        }
    }

    fn new_continuation(turn_id: TurnId) -> Self {
        Self {
            continuation: true,
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
/// Turn state its stream projects into. A spawn establishes a route along
/// with the child Session it opens, and the Subagent's settle drops it.
/// Routes live and die with the Provider connection, because the identities
/// are the connection's to mint. An event attributed to a Subagent with no
/// route lands nowhere.
///
/// The rows live beside the routes rather than inside any Turn's state,
/// because a Subagent may outlive the Turn that spawned it (ADR 0015): its
/// row must still be reachable when the Provider settles it after that Turn
/// has.
#[derive(Default)]
struct SubagentRoutes {
    routes: HashMap<ProviderSubagentId, SubagentRoute>,
    rows: HashMap<ProviderSubagentId, SubagentRow>,
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

/// One Subagent's row in its spawner's Transcript. Suru times the delegation
/// itself, the way it times a Reasoning block: `started` is the moment the
/// spawn was admitted, and the elapsed time it yields is the duration the
/// settled row reports.
struct SubagentRow {
    /// The Session whose Transcript holds the row — the Subagent's spawner,
    /// which for a nested Subagent is itself a child Session.
    owner_session_id: SessionId,
    activity_id: ActivityId,
    started: Instant,
}

impl SubagentRoutes {
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
            QueuedPromptDisposition::LeavePending,
        );
        if !matches!(projection, ProviderEventProjection::Terminal(_)) {
            self.routes.insert(subagent.clone(), route);
        }
    }

    /// Fails every routed Turn and open row, and forgets both. Called
    /// wherever the Provider connection is torn down: the routes' Turn state
    /// is this actor's only handle on those streams, and no more of their
    /// events — the settles the rows were owed included — can arrive once the
    /// connection is gone.
    fn fail_all(&mut self, sessions: &SessionStore, updates: &ProviderUpdateGate, message: &str) {
        for (_, mut route) in self.routes.drain() {
            fail_active_turn(
                sessions,
                updates,
                route.session_id,
                &mut route.turn,
                message.to_owned(),
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
    /// Interrupted: the child's Turn closes with the stop, the row records
    /// the duration Suru timed, and the identities join the stopped set so
    /// the Provider's trailing account of them is discarded.
    fn settle_stopped(
        &mut self,
        sessions: &SessionStore,
        updates: &ProviderUpdateGate,
        next_agent: &AgentIdentity,
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
                targets.extend(
                    self.rows
                        .iter()
                        .filter(|(_, held)| held.owner_session_id == route.session_id)
                        .map(|(descendant, _)| descendant.clone()),
                );
                let trailing_output = route.turn.take_trailing_output();
                let _ = updates.apply(|| {
                    sessions.finish_provider_turn(
                        route.session_id,
                        route.turn.turn_id,
                        next_agent.agent.clone(),
                        ProviderTurnOutcome::Interrupted { trailing_output },
                        QueuedPromptDisposition::LeavePending,
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
    fn stop_all(
        &mut self,
        sessions: &SessionStore,
        updates: &ProviderUpdateGate,
        next_agent: &AgentIdentity,
    ) {
        let working = self.rows.keys().cloned().collect::<Vec<_>>();
        for subagent in working {
            self.settle_stopped(sessions, updates, next_agent, &subagent);
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

    /// Settles one Subagent on the Provider's own settle signal: the child's
    /// Turn closes along with the row, because the Provider's boundary for
    /// the Subagent is one signal and no further event of the child's is owed
    /// once it has passed. Returns `None` when no open row carries the
    /// identity, on the same terms as [`Self::update_row`].
    fn settle_subagent(
        &mut self,
        sessions: &SessionStore,
        next_agent: &AgentIdentity,
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
                sessions.finish_provider_turn(
                    route.session_id,
                    route.turn.turn_id,
                    next_agent.agent.clone(),
                    outcome,
                    QueuedPromptDisposition::LeavePending,
                )?;
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
    Terminal(Option<DeliveredTurn>),
}

impl ProviderOrchestrator {
    pub(crate) fn new(
        runtimes: Vec<Arc<dyn ProviderRuntime>>,
        sessions: SessionStore,
        shutdown: watch::Receiver<bool>,
        updates: ProviderUpdateGate,
        settings: watch::Receiver<SettingsSnapshot>,
        skill_catalog: SkillCatalogService,
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
        }
    }

    pub(crate) fn open_session(
        &self,
        session_id: SessionId,
        workspace: PathBuf,
        prompt_id: PromptId,
    ) {
        if let Ok(commands_tx) =
            self.actor_commands_or_fail_prompt(session_id, workspace, prompt_id)
        {
            commands_tx
                .send(ProviderCommand::StartPrompt { prompt_id })
                .expect("new Provider actor accepts its initial Prompt");
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
        workspace: PathBuf,
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
        self.get_or_spawn_actor_commands(session_id, workspace, runtime)
    }

    fn get_or_spawn_actor_commands(
        &self,
        session_id: SessionId,
        workspace: PathBuf,
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
                runtime,
                sessions,
                skill_catalog: self.skill_catalog.clone(),
                session_id,
                workspace,
                updates: self.updates.clone(),
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
        let workspace = self
            .sessions
            .workspace(session_id)
            .ok_or_else(|| anyhow::anyhow!("Session does not exist on this server instance"))?;
        let Ok(commands) = self.actor_commands_or_fail_prompt(session_id, workspace, prompt_id)
        else {
            // The Prompt's Turn was already settled as failed; scheduling has
            // nothing left to deliver.
            return Ok(());
        };
        commands
            .send(ProviderCommand::StartPrompt { prompt_id })
            .map_err(|_| anyhow::anyhow!("Session Provider actor stopped unexpectedly"))
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

    /// Stops what a Session is doing, whatever that is: the active Turn along
    /// with the Subagents it spawned, or — with no Turn running — the
    /// Subagents alone. Interrupting a Subagent's own Session stops that one
    /// Subagent, through the Provider connection its root ancestor owns.
    pub(crate) async fn interrupt_session(
        &self,
        session_id: SessionId,
    ) -> Result<(), InterruptSessionError> {
        let actor_commands = |actor_id: SessionId| {
            self.actors
                .lock()
                .expect("Provider actor registry lock is not poisoned")
                .entries
                .get(&actor_id)
                .map(|actor| actor.commands.clone())
        };
        match self.sessions.interrupt_target(session_id)? {
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
        }
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
            )
        });
        InterruptSessionError::ProviderFailure(message.to_owned())
    }

    pub(crate) async fn close_session(&self, session_id: SessionId) {
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
        self.shutdown_complete.send_replace(true);
    }
}

async fn run_provider_session(
    context: ProviderSessionContext,
    mut commands: mpsc::UnboundedReceiver<ProviderCommand>,
    mut shutdown: ProviderShutdown,
) {
    let ProviderSessionContext {
        runtime,
        sessions,
        skill_catalog,
        session_id,
        workspace,
        updates,
    } = context;
    let mut provider: Option<ConnectedProviderSession> = None;
    let mut active: Option<ActiveProviderTurn> = None;
    let mut subagents = SubagentRoutes::default();
    let mut deferred_prompt_id = None;
    let provider_id = runtime.provider_id();

    'actor: loop {
        if shutdown.requested() {
            break;
        }
        if active.is_none() {
            let input = if let Some(prompt_id) = deferred_prompt_id.take() {
                ProviderInput::Command(Some(ProviderCommand::StartPrompt { prompt_id }))
            } else if let Some(connected) = provider.as_mut() {
                tokio::select! {
                    biased;
                    _ = shutdown.wait() => break,
                    event = connected.events.next() => ProviderInput::Event(event),
                    command = commands.recv() => ProviderInput::Command(command),
                }
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
                            ProviderEvent::SubagentCompleted {
                                subagent_id,
                                status,
                            } => {
                                let _ = updates.apply(|| {
                                    subagents.settle_subagent(
                                        &sessions,
                                        &identity,
                                        &subagent_id,
                                        status,
                                    )
                                });
                            }
                            // Anything else is late output. Owed to Subagents
                            // still working past their Turn's settle — or to
                            // one that just settled, whose provoked output
                            // follows its settle — it begins a Continuation
                            // (ADR 0015); with nothing owed, stray output —
                            // an interrupted Turn's trailing stream, say — is
                            // discarded as it always was.
                            event => {
                                if !subagents.owes_continuation() {
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
                                    QueuedPromptDisposition::LeavePending,
                                );
                                subagents.late_settle_owes_continuation = false;
                                if !matches!(projection, ProviderEventProjection::Terminal(_)) {
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
                    let identity = connected.identity.clone();
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
                            subagents.stop_all(&sessions, &updates, &identity);
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
                            provider
                                .as_ref()
                                .map(|c| (c.session.clone(), c.identity.clone())),
                            &mut subagents,
                            &sessions,
                            &updates,
                            target,
                        )
                        .await,
                    );
                    continue;
                }
                ProviderCommand::SteerPrompt => continue,
            };
            if provider.is_none() {
                let connection = tokio::select! {
                    biased;
                    _ = shutdown.wait() => break 'actor,
                    connection = runtime.start_session(ProviderSessionRequest {
                        workspace: workspace.clone(),
                        resume_state: sessions.resume_state(session_id, &provider_id),
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
                provider = Some(ConnectedProviderSession {
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
                continue;
            };
            let delivery = skill_prompt_delivery(&prompt);
            if let Err(message) = revalidate_prompt_skills(
                &skill_catalog,
                &sessions,
                session_id,
                &provider_id,
                &workspace,
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
            let delivered = match delivered {
                Ok(Some(delivered)) => delivered,
                Ok(None) => continue,
                Err(_) => continue,
            };
            let provider_session = provider
                .as_ref()
                .expect("Provider connection exists before Prompt delivery")
                .session
                .clone();
            let (turn_id, input) = provider_turn_start(delivered);
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
                command = commands.recv() => ProviderInput::Command(command),
            }
        };
        match input {
            ProviderInput::Command(None) => break,
            ProviderInput::Command(Some(ProviderCommand::StartPrompt { prompt_id })) => {
                let current = active
                    .as_mut()
                    .expect("Provider input is handled while a Turn is active");
                if current.continuation {
                    // The next delivered Prompt settles a stale Continuation
                    // rather than steering it (ADR 0015): close it out as
                    // worked, then deliver the Prompt as its own Turn.
                    let identity = provider
                        .as_ref()
                        .expect("Provider connection exists while its Turn is active")
                        .identity
                        .clone();
                    let trailing_output = current.take_trailing_output();
                    let Some(_) = updates.apply(|| {
                        sessions.finish_provider_turn(
                            session_id,
                            current.turn_id,
                            identity.agent,
                            ProviderTurnOutcome::Completed { trailing_output },
                            QueuedPromptDisposition::LeavePending,
                        )
                    }) else {
                        break;
                    };
                    active = None;
                    deferred_prompt_id = Some(prompt_id);
                }
                // Otherwise the Prompt was admitted while a real Turn ran and
                // stays pending until that Turn's settle delivers the queue.
            }
            ProviderInput::Command(Some(ProviderCommand::SteerPrompt)) => {
                let current = active
                    .as_ref()
                    .expect("Provider input is handled while a Turn is active");
                let prompt = match sessions.next_pending_steer(session_id, current.turn_id) {
                    Ok(Some(prompt)) => prompt,
                    Ok(None) | Err(_) => continue,
                };
                if let Err(message) = revalidate_prompt_skills(
                    &skill_catalog,
                    &sessions,
                    session_id,
                    &provider_id,
                    &workspace,
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
                let identity = provider
                    .as_ref()
                    .expect("Provider connection exists while its Turn is active")
                    .identity
                    .clone();
                if current.continuation {
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
                            subagents.stop_all(&sessions, &updates, &identity);
                            let trailing_output = current.take_trailing_output();
                            let settled = updates.apply(|| {
                                sessions.finish_provider_turn(
                                    session_id,
                                    current.turn_id,
                                    identity.agent,
                                    ProviderTurnOutcome::Interrupted { trailing_output },
                                    QueuedPromptDisposition::LeavePending,
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
                        subagents.stop_all(&sessions, &updates, &identity);
                        let _ = response.send(Ok(()));
                    }
                    Err(error) => {
                        let message = failure_message("Provider interruption failed", &error);
                        fail_active_turn(&sessions, &updates, session_id, current, message.clone());
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
                        provider
                            .as_ref()
                            .map(|c| (c.session.clone(), c.identity.clone())),
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
                        let selection_rejected =
                            matches!(event, ProviderEvent::AgentSelectionRejected { .. });
                        let identity = provider
                            .as_ref()
                            .expect("Provider connection exists while its Turn is active")
                            .identity
                            .clone();
                        let queued_prompt_disposition = if provider_event_settles_turn(&event) {
                            queued_prompt_disposition(
                                &skill_catalog,
                                &sessions,
                                session_id,
                                &provider_id,
                                &workspace,
                            )
                            .await
                        } else {
                            QueuedPromptDisposition::LeavePending
                        };
                        if let ProviderEventProjection::Terminal(next_turn) = project_provider_event(
                            &sessions,
                            &updates,
                            session_id,
                            current,
                            &mut subagents,
                            &identity,
                            event,
                            queued_prompt_disposition,
                        ) {
                            active = None;
                            if let Some(delivered) = next_turn {
                                let (turn_id, input) = provider_turn_start(delivered);
                                let started = tokio::select! {
                                    biased;
                                    _ = shutdown.wait() => break 'actor,
                                    started = provider_session.start_turn(input) => started,
                                };
                                if let Err(error) = started {
                                    let session_lost = error.is_session_lost();
                                    let selection_rejected = error.is_selection_rejected();
                                    project_turn_start_failure(
                                        &sessions, &updates, session_id, turn_id, &error,
                                    );
                                    if session_lost {
                                        lose_provider_connection(
                                            &mut provider,
                                            &mut subagents,
                                            &sessions,
                                            &updates,
                                        );
                                    }
                                    if !selection_rejected {
                                        defer_next_queued_prompt(
                                            &mut deferred_prompt_id,
                                            &sessions,
                                            session_id,
                                        );
                                    }
                                } else {
                                    active = Some(ActiveProviderTurn::new(turn_id));
                                    subagents.late_settle_owes_continuation = false;
                                }
                            } else if !selection_rejected {
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
    // Turn, and nothing else will finish the streams it left in flight. It fails
    // rather than settling an acknowledged interruption, because a Turn settled as
    // interrupted delivers the next queued Prompt, and this actor is in no state to
    // run it.
    if let Some(mut current) = active {
        fail_active_turn(
            &sessions,
            &updates,
            session_id,
            &mut current,
            "Provider execution failed: Suru stopped the Provider Session before the Turn completed."
                .to_owned(),
        );
    }
    subagents.fail_all(&sessions, &updates, SUBAGENT_CONNECTION_LOST_MESSAGE);

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
    connection: Option<(Arc<dyn ProviderSession>, AgentIdentity)>,
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
    let (provider_session, identity) =
        connection.expect("routed Subagents ride a live Provider connection");
    match provider_session.stop_subagent(subagent.clone()).await {
        Ok(()) => {
            subagents.settle_stopped(sessions, updates, &identity, &subagent);
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
    subagents.fail_all(sessions, updates, SUBAGENT_CONNECTION_LOST_MESSAGE);
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
) {
    let turn_id = active.turn_id;
    let trailing_output = active.take_trailing_output();
    let _ = updates.apply(|| sessions.fail_turn(session_id, turn_id, trailing_output, message));
}

fn provider_turn_start(delivered: DeliveredTurn) -> (TurnId, ProviderTurnInput) {
    let turn_id = delivered.turn_id;
    let selection = delivered
        .agent
        .expect("a connected Provider delivers a selected Turn")
        .selection;
    (
        turn_id,
        ProviderTurnInput {
            prompt: ProviderPrompt::from_user_prompt(
                delivered.prompt.text,
                delivered.prompt.skill_invocations,
            ),
            selection,
        },
    )
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
            sessions.fail_turn(session_id, turn_id, TrailingCommandOutput::new(), message)
        }
    });
}

fn skill_prompt_delivery(prompt: &Prompt) -> SkillPromptDelivery {
    if prompt.admission_order == PromptOrder::INITIAL {
        SkillPromptDelivery::Initial
    } else {
        match prompt.delivery {
            PromptDelivery::Queue => SkillPromptDelivery::Queue,
            PromptDelivery::Steer => SkillPromptDelivery::Steer,
        }
    }
}

fn provider_event_settles_turn(event: &ProviderEvent) -> bool {
    matches!(
        event,
        ProviderEvent::TurnCompleted
            | ProviderEvent::TurnInterrupted
            | ProviderEvent::TurnFailed { .. }
    )
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

async fn queued_prompt_disposition(
    skill_catalog: &SkillCatalogService,
    sessions: &SessionStore,
    session_id: SessionId,
    provider: &ProviderId,
    workspace: &std::path::Path,
) -> QueuedPromptDisposition {
    let Some(prompt) = next_queued_prompt(sessions, session_id) else {
        return QueuedPromptDisposition::LeavePending;
    };
    match revalidate_prompt_skills(
        skill_catalog,
        sessions,
        session_id,
        provider,
        workspace,
        &prompt,
        SkillPromptDelivery::Queue,
    )
    .await
    {
        Ok(()) => QueuedPromptDisposition::Deliver {
            prompt_id: prompt.id,
        },
        Err(message) => QueuedPromptDisposition::Fail {
            prompt_id: prompt.id,
            message,
        },
    }
}

async fn revalidate_prompt_skills(
    skill_catalog: &SkillCatalogService,
    sessions: &SessionStore,
    session_id: SessionId,
    provider: &ProviderId,
    workspace: &std::path::Path,
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
        .validate_prompt(provider.clone(), workspace, &prompt, delivery)
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
    queued_prompt_disposition: QueuedPromptDisposition,
) -> ProviderEventProjection {
    let Some(projected) = updates.apply(|| {
        let projection = match event {
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
                        next_agent,
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
                        next_agent,
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
                        next_agent,
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
                        next_agent,
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
                        next_agent,
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
                        next_agent,
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
                        next_agent,
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
                        next_agent,
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
                        next_agent,
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
                        next_agent,
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
            } => {
                if subagents.rows.contains_key(&subagent_id)
                    || subagents.routes.contains_key(&subagent_id)
                {
                    Err(anyhow::anyhow!(
                        "Provider reused an active Subagent identity"
                    ))
                } else {
                    let name = normalize_provider_text(&name);
                    let description = normalize_provider_text(&description);
                    sessions
                        .create_subagent(session_id, Some(next_agent.clone()), &name, &description)
                        .and_then(|spawned| {
                            let subagent_activity_id = ActivityId::new();
                            sessions.publish_agent_output(
                                session_id,
                                SessionChange::ActivityAdded {
                                    activity: Activity::Subagent {
                                        id: subagent_activity_id,
                                        turn_id: active.turn_id,
                                        status: ActivityStatus::Active,
                                        name,
                                        description,
                                        session_id: spawned.session_id,
                                        duration_ms: None,
                                    },
                                },
                            )?;
                            subagents.rows.insert(
                                subagent_id.clone(),
                                SubagentRow {
                                    owner_session_id: session_id,
                                    activity_id: subagent_activity_id,
                                    started: Instant::now(),
                                },
                            );
                            subagents.routes.insert(
                                subagent_id,
                                SubagentRoute {
                                    session_id: spawned.session_id,
                                    turn: ActiveProviderTurn::new(spawned.turn_id),
                                },
                            );
                            Ok(ProviderEventProjection::Continue)
                        })
                }
            }
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
                        next_agent,
                        "Provider updated a Subagent before spawning it",
                    );
                }
                Some(updated) => updated.map(|()| ProviderEventProjection::Continue),
            },
            ProviderEvent::SubagentCompleted {
                subagent_id,
                status,
            } => match subagents.settle_subagent(sessions, next_agent, &subagent_id, status) {
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
                        next_agent,
                        "Provider settled a Subagent before spawning it",
                    );
                }
                Some(settled) => settled.map(|()| ProviderEventProjection::Continue),
            },
            ProviderEvent::TurnCompleted => {
                // A Subagent still working holds nothing open here: the Turn
                // settles at the Provider's own boundary (ADR 0015), and the
                // rows and routes live on at the connection until each
                // Subagent's own settle arrives.
                if active.streaming_message.is_some()
                    || !active.command_activities.is_empty()
                    || !active.file_change_activities.is_empty()
                    || !active.reasoning_activities.is_empty()
                {
                    Err(anyhow::anyhow!(
                        "Provider completed the Turn before completing its streamed output"
                    ))
                } else {
                    sessions
                        .finish_provider_turn(
                            session_id,
                            active.turn_id,
                            next_agent.agent.clone(),
                            ProviderTurnOutcome::Completed {
                                trailing_output: TrailingCommandOutput::new(),
                            },
                            queued_prompt_disposition,
                        )
                        .map(ProviderEventProjection::Terminal)
                }
            }
            ProviderEvent::TurnInterrupted => {
                let trailing_output = active.take_trailing_output();
                sessions
                    .finish_provider_turn(
                        session_id,
                        active.turn_id,
                        next_agent.agent.clone(),
                        ProviderTurnOutcome::Interrupted { trailing_output },
                        queued_prompt_disposition,
                    )
                    .map(ProviderEventProjection::Terminal)
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
                    .map(|_| ProviderEventProjection::Terminal(None))
            }
            ProviderEvent::TurnFailed { message } => {
                let trailing_output = active.take_trailing_output();
                sessions
                    .finish_provider_turn(
                        session_id,
                        active.turn_id,
                        next_agent.agent.clone(),
                        ProviderTurnOutcome::Failed {
                            trailing_output,
                            message: normalize_provider_text(&message),
                        },
                        queued_prompt_disposition,
                    )
                    .map(ProviderEventProjection::Terminal)
            }
        };

        projection.unwrap_or_else(|error| {
            finish_invalid_provider_event(
                sessions,
                session_id,
                active,
                next_agent,
                failure_message("Provider execution failed", &error),
            )
        })
    }) else {
        return ProviderEventProjection::Terminal(None);
    };
    projected
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
    next_agent: &AgentIdentity,
    message: &str,
) -> ProviderEventProjection {
    finish_invalid_provider_event(
        sessions,
        session_id,
        active,
        next_agent,
        format!("Provider execution failed: {message}"),
    )
}

fn finish_invalid_provider_event(
    sessions: &SessionStore,
    session_id: SessionId,
    active: &mut ActiveProviderTurn,
    next_agent: &AgentIdentity,
    message: String,
) -> ProviderEventProjection {
    let trailing_output = active.take_trailing_output();
    let next_turn = sessions
        .finish_provider_turn(
            session_id,
            active.turn_id,
            next_agent.agent.clone(),
            ProviderTurnOutcome::Failed {
                trailing_output,
                message,
            },
            QueuedPromptDisposition::LeavePending,
        )
        .ok()
        .flatten();
    ProviderEventProjection::Terminal(next_turn)
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
        ModelId, SessionSnapshot, TurnStatus, Workspace,
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
        let workspace = tempfile::tempdir().expect("create valid Workspace");
        let repository = StorageRepository::open(data_dir.path())
            .await
            .expect("open Session repository");
        let (writer, storage) = StorageWriter::spawn(repository, &[]);
        let sessions = SessionStore::new(Default::default(), storage);
        let owning = create_session(&sessions, workspace.path(), "The owning conversation");
        let routed = create_session(&sessions, workspace.path(), "Delegated work");
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
            _workspace: workspace,
        }
    }

    fn create_session(
        sessions: &SessionStore,
        workspace: &std::path::Path,
        prompt: &str,
    ) -> SessionSnapshot {
        let created = sessions
            .create(CreateSessionRequest {
                agent_selection: None,
                workspace: Workspace {
                    path: workspace.to_owned(),
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
        let mut routes = SubagentRoutes::default();
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
        let mut routes = SubagentRoutes::default();
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

    #[tokio::test]
    async fn an_event_attributed_to_an_unknown_subagent_lands_nowhere() {
        let fixture = routed_sessions().await;
        let updates = ProviderUpdateGate::new();
        let identity = provider_identity();
        let mut routes = SubagentRoutes::default();
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
        let mut routes = SubagentRoutes::default();
        route_to(&mut routes, &subagent, &fixture);

        routes.fail_all(
            &fixture.sessions,
            &updates,
            SUBAGENT_CONNECTION_LOST_MESSAGE,
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
}
