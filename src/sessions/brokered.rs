//! Brokered Subagents' Sessions: the child Session the Broker spawns under the
//! Agent that asked for it, on the Provider and Model that Agent chose, and
//! the row each stretch of its work stands as in the delegating Transcript.
//!
//! A brokered Subagent runs on a Provider actor of its own (ADR 0035), so no
//! Provider reports its stretch settling the way a native Subagent's Provider
//! reports one of its own: the child's Turn settling is the stretch settling.
//! The row therefore follows that Turn here, in the store, whichever path
//! settles it — the child's Provider at its own boundary, a lost connection, a
//! stopping Server, or the next start's repair — and the Model the child's
//! Provider confirms for the Turn reaches the row the same way, so the Subagent
//! tree and the Subagent Picker read a brokered row exactly as a native one.
//!
//! A Delegation's row stands in the Turn the delegating Agent works in. A
//! spawn that comes after that Turn has settled — from work the caller's
//! Provider carries on past it, or from a native Subagent riding the caller's
//! token — stands in a Continuation of the caller's Session begun to hold it
//! (CONTEXT.md: Subagent), which settles as it is begun because nothing else
//! ever would (ADR 0033).
//!
//! The Agent that spawned a brokered Subagent, and every Agent above it, reads
//! how it stands through the Broker: its latest Turn's outcome, how long that
//! Turn has worked, and the latest Message it wrote there.
//!
//! No Provider tells the delegating Agent that its brokered Subagent's stretch
//! settled, so Suru does, as that stretch's row settles: a Subagent Report of
//! the outcome and the start of the Subagent's final Message, held for the
//! delegating Session's Agent until its Provider takes it (ADR 0035).

use std::fmt;

use crate::ansi::NormalizedText;
use crate::protocol::{
    Activity, ActivityStatus, AgentSelection, BrokerSettings, Message, MessageRole, SessionChange,
    SessionId, SessionSnapshot, SessionTimestamp, Turn, TurnId, TurnStatus,
};
use crate::provider::{SubagentReport, SubagentReportOutcome, first_line};
use crate::storage::StorageSink;

use super::{
    SessionRecord, SessionStore, SessionStoreState,
    posture::brokered_subagent_posture,
    projection::active_turn_id,
    settlement::{OpenInterventions, TrailingCommandOutput, fail_turn_changes},
    subagents::{
        ChildSession, DeliveredDelegation, SubagentRoute, delegator, opening_brokered_subagent_row,
        stretches_of_work, subagent_title,
    },
};

/// What a delegating Agent asked the Broker to spawn, checked against the
/// Model Catalog already: the Agent Selection the Subagent runs under, with
/// its Provider fixed for good, the name and description its row carries, and
/// the Delegation — normalized and capped the way an Agent Message's content
/// is, since it is prose one Agent wrote for another.
pub(crate) struct BrokeredSpawn {
    pub(crate) selection: AgentSelection,
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) delegation: NormalizedText,
}

/// The child Session a brokered spawn opened: the Session the Broker answers
/// with, the Turn its Delegation opened for its first stretch of work, and the
/// Agent that delegated it, as the Delegation delivered to the child's
/// Provider names it.
pub(crate) struct SpawnedBrokeredSubagent {
    pub(crate) session_id: SessionId,
    pub(crate) turn_id: TurnId,
    pub(crate) delegator: DelegatingAgent,
}

/// The Agent a Delegation came from, as the Delegation names it to the
/// Subagent receiving it: by its Session's Title, or — when it is itself a
/// Subagent — by the name its rows carry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DelegatingAgent {
    Session { title: String },
    Subagent { name: String },
}

/// Why the store could not spawn a brokered Subagent under the caller.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum BrokeredSpawnError {
    /// The calling Session is not one the store holds ready: deleted, or its
    /// history still unread.
    CallerNotFound,
    /// The spawn would pass a cap the Broker's Settings put on brokered
    /// Subagents, so nothing of it was created — and nothing of it waits for
    /// room to be made.
    Capped(BrokeredSpawnCap),
    Storage(String),
}

impl fmt::Display for BrokeredSpawnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CallerNotFound => formatter
                .write_str("the Session calling the Broker no longer exists on this Suru server"),
            // The Broker words a cap for the delegating Agent itself, from the
            // cap carried here; this says only what kind of refusal it is.
            Self::Capped(_) => formatter
                .write_str("it would pass a cap the Broker's Settings put on brokered Subagents"),
            Self::Storage(message) => write!(formatter, "Suru could not record it: {message}"),
        }
    }
}

/// A cap the Broker's Settings put on brokered Subagents, which a spawn would
/// pass: it is refused rather than queued, with what the delegating Agent
/// needs to be told why.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BrokeredSpawnCap {
    /// The Subagent would stand `depth` Sessions deep — its top-level Session
    /// counting as the first — deeper than `broker.maxDepth` allows.
    Depth { max_depth: u32, depth: u32 },
    /// `working` brokered Subagents already work beneath the caller's
    /// top-level Session: as many as `broker.maxConcurrentSubagents` allows,
    /// or more where the cap was lowered after they were spawned.
    Concurrency {
        max_concurrent_subagents: u32,
        working: u32,
    },
}

/// How a brokered Subagent stands, as the Agents above it read it through the
/// Broker. The Subagent itself never Settles, so this is its latest stretch
/// of work: the one it is doing, or the one it last settled — its Session's
/// latest Turn, passing over a Continuation that only holds the row of a
/// Subagent it spawned after that stretch settled (see [`stretches_of_work`]).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BrokeredSubagentReading {
    pub(crate) session_id: SessionId,
    /// The latest Turn's status: `Active` while the Subagent works, and
    /// otherwise the outcome that Turn settled with.
    pub(crate) status: TurnStatus,
    /// How long the latest Turn has worked so far, or — once settled — how
    /// long it worked, as the row its stretch stands as says; `None` where
    /// Suru never learned when its work ended (ADR 0029).
    pub(crate) duration_ms: Option<u64>,
    /// The latest Message the Subagent wrote in that Turn, whole: its final
    /// one once the Turn has settled. `None` when it has written none there.
    pub(crate) message: Option<String>,
    /// What that Turn failed with, where it failed and its Transcript says
    /// why: the text of the error row that settled it. `None` for a Turn
    /// still working or settled any other way, and for a failure nothing
    /// explained.
    pub(crate) error: Option<String>,
}

/// Why a brokered Subagent could not be read for the Agent asking.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BrokeredReadError {
    /// The calling Session is not one the store holds ready: deleted, or its
    /// history still unread.
    CallerNotFound,
    /// No Session goes by the id asked for.
    NoSuchSession,
    /// The Session named is no brokered Subagent beneath the caller: a native
    /// Subagent, the caller itself or a Session above or beside it, or one in
    /// another tree altogether.
    NotBrokeredBeneathCaller,
}

impl SessionStore {
    /// Spawns a brokered Subagent under `caller`: a child Session on the
    /// Agent Selection the spawn chose, its Provider fixed, in the caller's
    /// Execution Directory, Workspace and checkout, titled from the spawn's
    /// description or name, and opened with the one prompt-less Turn its
    /// first stretch of work runs in, headed by the Delegation as a Message
    /// from the caller's Agent. It owns a Provider actor of its own, and
    /// acts under the Approval Posture [`brokered_subagent_posture`] gives it.
    ///
    /// A spawn that would pass either of the Broker's caps is refused before
    /// anything is created: one placing the Subagent deeper than
    /// `broker.maxDepth`, or one made while as many brokered Subagents as
    /// `broker.maxConcurrentSubagents` allows already work beneath the
    /// caller's top-level Session. The count is taken under the same lock the
    /// spawn is made in, so spawns racing each other cannot both slip under
    /// the cap; see [`SessionStoreState::brokered_spawn_cap`].
    ///
    /// The row that stands for it joins the caller's working Turn in the same
    /// lock, so no reader sees the child without its row or the row without
    /// the child. A caller with no Turn working — its Turn settled while its
    /// Provider worked on, or a native Subagent riding its token called after
    /// it settled — has a Continuation begun to hold the row instead, settled
    /// in the same commit: the caller's Provider never learns of that Turn, so
    /// nothing else would ever settle it, and the row works on past it with
    /// the child, as any row outlives the Turn it stands in. Nothing here
    /// reaches a Provider: whoever spawned it starts the child's actor and
    /// delivers the Delegation.
    pub(crate) fn spawn_brokered_subagent(
        &self,
        caller: SessionId,
        spawn: BrokeredSpawn,
    ) -> Result<SpawnedBrokeredSubagent, BrokeredSpawnError> {
        let settings = self.settings.borrow().settings.clone();
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        if state.is_deferred(caller) {
            return Err(BrokeredSpawnError::CallerNotFound);
        }
        if !state.sessions.contains_key(&caller) {
            return Err(BrokeredSpawnError::CallerNotFound);
        }
        if let Some(cap) = state.brokered_spawn_cap(caller, &settings.broker) {
            return Err(BrokeredSpawnError::Capped(cap));
        }
        let delegator = state.delegating_agent(caller);
        let approval_posture = brokered_subagent_posture(
            state.sessions[&caller].snapshot.session.approval_posture,
            &spawn.selection.provider,
            &settings,
        );
        let spawned = state.open_child_session(
            &self.storage,
            caller,
            ChildSession {
                title: subagent_title(&spawn.name, &spawn.description),
                selection: Some(spawn.selection),
                approval_posture,
                delegation: Some(spawn.delegation),
                route: SubagentRoute::Brokered,
            },
        );
        if let Err(error) = state.stand_brokered_row(
            &self.storage,
            caller,
            spawn.name,
            spawn.description,
            spawned.session_id,
        ) {
            // A child with no row would work on where no reader could reach
            // it, keeping the caller Working for good, so it goes too.
            state.sessions.remove(&spawned.session_id);
            if let Err(error) = self.storage.deleted(spawned.session_id) {
                tracing::warn!(
                    session_id = %spawned.session_id,
                    "a brokered Subagent whose row could not be added was not unstored: {error}"
                );
            }
            state.reconcile_liveness(&self.storage, caller);
            state.reconcile_usage(&self.storage, caller);
            return Err(BrokeredSpawnError::Storage(error.to_string()));
        }
        Ok(SpawnedBrokeredSubagent {
            session_id: spawned.session_id,
            turn_id: spawned.turn_id,
            delegator,
        })
    }

    /// Resumes the brokered Subagent `subagent` for the Agent of `caller`
    /// with `delegation`, which that Agent sent after the spawn: a new Turn in
    /// the Subagent's own Session, opened by the Delegation as a Message from
    /// the caller's Agent, so the Session holds the Subagent's whole
    /// conversation (ADR 0031); and a row of its own for that stretch of work
    /// in the Turn the caller's Agent works in — or in a Continuation begun to
    /// hold it, exactly as a spawn's row is held — leading into that same
    /// Session, named as the spawn's row names the Subagent and describing
    /// what the Delegation asks. A Turn still open in the Subagent's Session —
    /// a Continuation its own work began — settles first, as worked, as a
    /// Prompt settles one (CONTEXT.md: Continuation). The row follows the new
    /// Turn from here as every brokered row does, and its settling reports to
    /// the caller's Agent.
    ///
    /// Only a brokered Subagent beneath the caller is resumed, as only one is
    /// read. A resume sets one more brokered Subagent working, so one that
    /// would pass `broker.maxConcurrentSubagents` is refused before anything
    /// is begun, counted as a spawn is but for the Subagent itself, which the
    /// resume sets working again rather than adds. Depth does not apply: the
    /// Subagent already stands where it stands.
    ///
    /// Nothing here reaches a Provider: the Subagent's own actor, which asked
    /// for the resume, delivers the Delegation as the new Turn's input.
    pub(crate) fn resume_brokered_subagent(
        &self,
        caller: SessionId,
        subagent: SessionId,
        delegation: NormalizedText,
    ) -> Result<ResumedBrokeredSubagent, BrokeredResumeError> {
        let settings = self.settings.borrow().settings.clone();
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        state.resume_refusal(caller, subagent, &settings.broker)?;
        let delegating = state.delegating_agent(caller);
        let title = &state.sessions[&subagent].snapshot.title;
        // The name the spawn's row carries, which every later row repeats.
        let name = delegator(&state.sessions, subagent)
            .name
            .unwrap_or_else(|| title.clone());
        let description = first_line(&delegation.content).unwrap_or_else(|| title.clone());
        let turn_id = state
            .begin_subagent_turn(
                &self.storage,
                subagent,
                Some(DeliveredDelegation {
                    delegating_session: caller,
                    text: delegation,
                }),
            )
            .map_err(|error| BrokeredResumeError::Storage(error.to_string()))?;
        if let Err(error) =
            state.stand_brokered_row(&self.storage, caller, name, description, subagent)
        {
            // A stretch no row stands for would work where no reader of the
            // delegating Transcript could reach it, and report to no one, so
            // it settles before its Provider ever hears of it.
            let failed = fail_turn_changes(
                &state.sessions[&subagent].snapshot,
                turn_id,
                TrailingCommandOutput::new(),
                format!("Suru could not record this resume: {error}"),
                None,
                OpenInterventions::TurnEnded,
            );
            if let Err(error) = state.commit(&self.storage, subagent, failed) {
                tracing::warn!(
                    session_id = %subagent,
                    "a resume whose row could not be added was not settled: {error:#}"
                );
            }
            return Err(BrokeredResumeError::Storage(error.to_string()));
        }
        Ok(ResumedBrokeredSubagent {
            turn_id,
            delegator: delegating,
        })
    }

    /// Why a resume of the brokered Subagent `subagent` for the Agent of
    /// `caller` would be refused now, for the reasons
    /// [`Self::resume_brokered_subagent`] refuses one, without beginning
    /// anything: asked before what the Subagent is doing is stopped for the
    /// resume, so a resume bound to be refused stops nothing.
    pub(crate) fn brokered_resume_refusal(
        &self,
        caller: SessionId,
        subagent: SessionId,
    ) -> Option<BrokeredResumeError> {
        let settings = self.settings.borrow().settings.clone();
        self.state
            .lock()
            .expect("Session store lock is not poisoned")
            .resume_refusal(caller, subagent, &settings.broker)
            .err()
    }

    /// Reads how the brokered Subagent `subagent` stands, for the Agent of
    /// `caller`: the outcome of its Session's latest Turn — or that it still
    /// works there — how long that Turn has worked, and the latest Message it
    /// wrote in it. Only a brokered Subagent beneath the caller is read: one
    /// the caller's Agent spawned, or one spawned by a Subagent beneath it, at
    /// any depth (see [`SessionStoreState::is_brokered_beneath`]).
    pub(crate) fn read_brokered_subagent(
        &self,
        caller: SessionId,
        subagent: SessionId,
    ) -> Result<BrokeredSubagentReading, BrokeredReadError> {
        let state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        state.reach_brokered_subagent(caller, subagent)?;
        Ok(state.brokered_subagent_reading(subagent, SessionTimestamp::now()))
    }

    /// Whether the Agent of `caller` may reach `subagent` through the Broker —
    /// read it, send it more, wait on it — and why not where it may not: see
    /// [`SessionStoreState::reach_brokered_subagent`].
    pub(crate) fn reach_brokered_subagent(
        &self,
        caller: SessionId,
        subagent: SessionId,
    ) -> Result<(), BrokeredReadError> {
        self.state
            .lock()
            .expect("Session store lock is not poisoned")
            .reach_brokered_subagent(caller, subagent)
    }

    /// The brokered Subagents the Agent of `caller` spawned — its Session's
    /// own brokered children, whichever Agent riding its token spawned them —
    /// that are working now, each by its own latest stretch of work, in the
    /// order they spawned. These are what a wait naming no Subagent waits on:
    /// a Subagent one of them spawned in turn reports to that one, not to the
    /// caller, so it is its spawner's to wait on.
    pub(crate) fn working_brokered_children(
        &self,
        caller: SessionId,
    ) -> Result<Vec<SessionId>, BrokeredReadError> {
        let state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        if state.is_deferred(caller) || !state.sessions.contains_key(&caller) {
            return Err(BrokeredReadError::CallerNotFound);
        }
        let mut working = state
            .sessions
            .iter()
            .filter(|(_, record)| {
                record.snapshot.session.parent == Some(caller)
                    && record.is_brokered_subagent()
                    && record.is_at_work_itself()
            })
            .map(|(session_id, record)| (record.summary.created_at, *session_id))
            .collect::<Vec<_>>();
        working.sort_by_key(|(created_at, session_id)| (*created_at, session_id.to_string()));
        Ok(working
            .into_iter()
            .map(|(_, session_id)| session_id)
            .collect())
    }

    /// The Agent of `session_id` as a Delegation it sends names it to the
    /// Subagent receiving it; see [`DelegatingAgent`].
    pub(crate) fn delegating_agent(&self, session_id: SessionId) -> Option<DelegatingAgent> {
        let state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        state
            .sessions
            .contains_key(&session_id)
            .then(|| state.delegating_agent(session_id))
    }
}

/// A brokered Subagent's resume, begun in the store: the Turn the Delegation
/// opened in the Subagent's own Session, and the Agent that sent it, as the
/// Delegation delivered to the Subagent's Provider names it.
pub(crate) struct ResumedBrokeredSubagent {
    pub(crate) turn_id: TurnId,
    pub(crate) delegator: DelegatingAgent,
}

/// Why the store could not resume a brokered Subagent for the Agent sending
/// it more work.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum BrokeredResumeError {
    /// The Subagent is none the sending Agent may reach through the Broker.
    Unreachable(BrokeredReadError),
    /// The resume would set more brokered Subagents working than
    /// `broker.maxConcurrentSubagents` allows, so nothing of it was begun —
    /// and nothing of it waits for room to be made.
    Capped(BrokeredSpawnCap),
    Storage(String),
}

/// The Turn a brokered Subagent's row joins in the caller's Session.
enum HoldingTurn {
    /// The Turn the caller's Agent is working in.
    Working(TurnId),
    /// A Continuation begun to hold the row, the caller having no Turn
    /// working, which settles as it is begun.
    Continuation(Box<Turn>),
}

impl HoldingTurn {
    fn turn_id(&self) -> TurnId {
        match self {
            Self::Working(turn_id) => *turn_id,
            Self::Continuation(turn) => turn.id,
        }
    }

    /// The changes that stand `row` in this Turn: for a Continuation, begun
    /// and settled around it in one commit, so no reader ever sees it open.
    fn holds(self, row: Activity) -> Vec<SessionChange> {
        let row = SessionChange::ActivityAdded { activity: row };
        match self {
            Self::Working(_) => vec![row],
            Self::Continuation(turn) => {
                let turn_id = turn.id;
                vec![
                    SessionChange::TurnAdded { turn: *turn },
                    row,
                    SessionChange::TurnStatusChanged {
                        turn_id,
                        status: TurnStatus::Completed,
                        settled_at: None,
                    },
                ]
            }
        }
    }
}

/// The Continuation that holds a brokered Subagent's row for a caller with no
/// Turn working: begun by neither a Prompt nor a Delegation, and run by the
/// Agent that ran the caller's latest Turn — the Agent calling — so the
/// Session's Model, and the Context Fill measured against it, stand as they
/// were. The commit that lands it stamps when it began and when it settled.
fn holding_continuation(caller: &SessionSnapshot) -> Turn {
    Turn::unprompted(caller.turns.last().and_then(|turn| turn.agent.clone()))
}

/// The Turns a batch of changes settles at a moment it carries itself rather
/// than at the commit's: a restart's repair of Turns a stop left open (ADR
/// 0029), which nothing timed. Read before the commit stamps its own moment
/// on every other settlement.
pub(super) fn repaired_settlements(changes: &[SessionChange]) -> Vec<TurnId> {
    changes
        .iter()
        .filter_map(|change| match change {
            SessionChange::TurnStatusChanged {
                turn_id,
                status,
                settled_at: Some(_),
            } if status.is_terminal() => Some(*turn_id),
            _ => None,
        })
        .collect()
}

impl SessionStoreState {
    /// The cap a brokered spawn under `caller` would pass, read against the
    /// Broker's Settings `broker`, or `None` where there is room for it.
    ///
    /// Depth counts Sessions from the top-level Session down, whatever route
    /// spawned each, since a tree's shape is the same whoever spawned into it:
    /// the top-level Session stands one deep and the Subagent would stand one
    /// deeper than `caller`. A tree too deep is refused first, since waiting
    /// makes no room there.
    ///
    /// Concurrency counts the brokered Subagents working anywhere beneath the
    /// caller's top-level Session — its own, and every Subagent's at any
    /// depth — each by its own latest Turn, so a settled Subagent frees its
    /// slot even while work it delegated goes on, and that work counts for
    /// itself. A native Subagent is not counted: Suru cannot refuse its
    /// Provider's spawns, so it cannot hold them to the cap either.
    fn brokered_spawn_cap(
        &self,
        caller: SessionId,
        broker: &BrokerSettings,
    ) -> Option<BrokeredSpawnCap> {
        let depth = cap_count(self.ancestors(caller).count()).saturating_add(1);
        if depth > broker.max_depth {
            return Some(BrokeredSpawnCap::Depth {
                max_depth: broker.max_depth,
                depth,
            });
        }
        self.brokered_concurrency_cap(caller, None, broker)
    }

    /// The concurrency cap `broker` puts on the tree `within` stands in,
    /// where setting one more brokered Subagent working there would pass it:
    /// `resuming`, when it is a settled Subagent a resume sets working again,
    /// or otherwise one a spawn adds. The resumed Subagent is not counted
    /// among those working, since the resume is what would set it working.
    fn brokered_concurrency_cap(
        &self,
        within: SessionId,
        resuming: Option<SessionId>,
        broker: &BrokerSettings,
    ) -> Option<BrokeredSpawnCap> {
        // The line's last Session heads the tree: the top-level Session, or
        // — for a line restoration left broken — the highest Session held.
        let (top_level, _) = self.ancestors(within).last()?;
        let working = cap_count(
            self.actor_owners_beneath(top_level)
                .into_iter()
                .filter(|session_id| {
                    Some(*session_id) != resuming
                        && self.sessions.get(session_id).is_some_and(|record| {
                            record.is_brokered_subagent() && record.is_at_work_itself()
                        })
                })
                .count(),
        );
        (working >= broker.max_concurrent_subagents).then_some(BrokeredSpawnCap::Concurrency {
            max_concurrent_subagents: broker.max_concurrent_subagents,
            working,
        })
    }

    /// Why a resume of `subagent` for the Agent of `caller` is refused, if it
    /// is: a Subagent out of the caller's reach, or one more brokered Subagent
    /// working than `broker` allows, counted as a spawn is but for `subagent`
    /// itself, which the resume sets working again rather than adds.
    fn resume_refusal(
        &self,
        caller: SessionId,
        subagent: SessionId,
        broker: &BrokerSettings,
    ) -> Result<(), BrokeredResumeError> {
        self.reach_brokered_subagent(caller, subagent)
            .map_err(BrokeredResumeError::Unreachable)?;
        match self.brokered_concurrency_cap(subagent, Some(subagent), broker) {
            Some(cap) => Err(BrokeredResumeError::Capped(cap)),
            None => Ok(()),
        }
    }

    /// Stands the row a stretch of the brokered Subagent `child`'s work opens
    /// as — named `name` and describing `description` — in the Turn the Agent
    /// of `holder` works in, which delegated that stretch; or, where that
    /// Agent has no Turn working, in a Continuation begun to hold it and
    /// settled in the same commit, since that Agent's Provider never learns
    /// of it and nothing else would ever settle it (ADR 0033, 0035).
    fn stand_brokered_row(
        &mut self,
        storage: &StorageSink,
        holder: SessionId,
        name: String,
        description: String,
        child: SessionId,
    ) -> anyhow::Result<()> {
        let snapshot = &self
            .sessions
            .get(&holder)
            .ok_or_else(|| anyhow::anyhow!("Session does not exist on this server instance"))?
            .snapshot;
        let holding = match active_turn_id(snapshot)? {
            Some(working) => HoldingTurn::Working(working),
            None => HoldingTurn::Continuation(Box::new(holding_continuation(snapshot))),
        };
        let row = opening_brokered_subagent_row(holding.turn_id(), name, description, child);
        self.commit(storage, holder, holding.holds(row))?;
        Ok(())
    }

    /// The Agent of `session_id`, which the caller has found held, as a
    /// Delegation it sends names it: by the name its rows carry when it is a
    /// Subagent, and otherwise by its Session's Title.
    fn delegating_agent(&self, session_id: SessionId) -> DelegatingAgent {
        match delegator(&self.sessions, session_id).name {
            Some(name) => DelegatingAgent::Subagent { name },
            None => DelegatingAgent::Session {
                title: self.sessions[&session_id].snapshot.title.clone(),
            },
        }
    }

    /// Whether the Agent of `caller` may reach `subagent` through the Broker:
    /// only a brokered Subagent beneath it (see
    /// [`SessionStoreState::is_brokered_beneath`]), and only while `caller`
    /// itself is held.
    fn reach_brokered_subagent(
        &self,
        caller: SessionId,
        subagent: SessionId,
    ) -> Result<(), BrokeredReadError> {
        if self.is_deferred(caller) || !self.sessions.contains_key(&caller) {
            return Err(BrokeredReadError::CallerNotFound);
        }
        if !self.sessions.contains_key(&subagent) {
            return Err(BrokeredReadError::NoSuchSession);
        }
        if !self.is_brokered_beneath(subagent, caller) {
            return Err(BrokeredReadError::NotBrokeredBeneathCaller);
        }
        Ok(())
    }

    /// Carries what a commit to a brokered Subagent's Session did to one of
    /// its Turns onto the row that Turn's stretch of work stands as: the
    /// Model its Provider confirmed for the Turn, and — once the Turn settles
    /// — the outcome it settled with and how long it worked, timed from the
    /// Turn's beginning, which is its spawn or resume. A Turn a restart
    /// settled at the moment it last showed work was timed by nothing, so its
    /// row settles with no duration, as a native Subagent's does (ADR 0029).
    ///
    /// The row is found where the Turn's opening Delegation says: in the
    /// Session of the Agent that delegated it. A Turn no Delegation opened —
    /// a Continuation the Subagent's own work began — stands as no row.
    ///
    /// The commit that settles the row also leaves that Agent a Subagent
    /// Report of the stretch, to be handed to its Provider (see
    /// [`SessionStoreState::hold_report`]) — except for a stretch an
    /// interrupt of a Session above it stopped, since the Agent that would
    /// hear of it was interrupted too. A Turn that stands as no row, the
    /// Continuations that only hold rows among them, reports nothing.
    pub(super) fn follow_brokered_turns(
        &mut self,
        storage: &StorageSink,
        session_id: SessionId,
        changes: &[SessionChange],
        repaired: &[TurnId],
    ) {
        if !self
            .sessions
            .get(&session_id)
            .is_some_and(|record| record.is_brokered_subagent())
        {
            return;
        }
        let mut moved = Vec::new();
        for change in changes {
            let turn_id = match change {
                SessionChange::SubagentAgentChanged { turn_id, .. }
                | SessionChange::TurnAgentChanged { turn_id, .. } => *turn_id,
                SessionChange::TurnStatusChanged {
                    turn_id, status, ..
                } if status.is_terminal() => *turn_id,
                _ => continue,
            };
            if !moved.contains(&turn_id) {
                moved.push(turn_id);
            }
        }
        for turn_id in moved {
            let Some(snapshot) = self
                .sessions
                .get(&session_id)
                .map(|record| &record.snapshot)
            else {
                return;
            };
            let Some(turn) = snapshot.turns.iter().find(|turn| turn.id == turn_id) else {
                continue;
            };
            let Some(holder) = delegating_session(snapshot, turn_id) else {
                continue;
            };
            let row_changes = self.row_changes(holder, session_id, turn, repaired);
            if row_changes.is_empty() {
                continue;
            }
            let report = row_changes
                .iter()
                .any(|change| matches!(change, SessionChange::SubagentStatusChanged { .. }))
                .then(|| self.settled_report(holder, session_id, turn, repaired))
                .flatten();
            if let Err(error) = self.commit(storage, holder, row_changes) {
                tracing::warn!(
                    %session_id,
                    delegating_session = %holder,
                    "a brokered Subagent's row did not follow its Turn: {error:#}"
                );
                continue;
            }
            if let Some(report) = report {
                self.hold_report(holder, report);
            }
        }
    }

    /// The Subagent Report `holder`'s Agent is owed now that the brokered
    /// Subagent `child`'s `turn` has settled, which its row is about to say:
    /// named as that row names it, with the outcome the Turn settled with,
    /// how long it worked, and the final Message it wrote there. A stretch
    /// the user stopped on its own is reported as stopped; one an interrupt
    /// from above stopped reports nothing (CONTEXT.md: Subagent Report). A
    /// Turn a restart settled failed at the moment it last showed work was
    /// timed by nothing and ended by nothing it said, so its Report carries
    /// neither a duration nor an excerpt (ADR 0029).
    fn settled_report(
        &self,
        holder: SessionId,
        child: SessionId,
        turn: &Turn,
        repaired: &[TurnId],
    ) -> Option<SubagentReport> {
        let outcome = match turn.status {
            TurnStatus::Active => return None,
            TurnStatus::Completed => SubagentReportOutcome::Completed,
            TurnStatus::Failed => SubagentReportOutcome::Failed,
            TurnStatus::Interrupted if self.stopped_by_ancestor(child, turn.id) => return None,
            TurnStatus::Interrupted => SubagentReportOutcome::Stopped,
        };
        let name =
            self.sessions[&holder].snapshot.activities.iter().find_map(
                |activity| match activity {
                    Activity::Subagent {
                        session_id,
                        status: ActivityStatus::Active,
                        name,
                        ..
                    } if *session_id == child => Some(name.clone()),
                    _ => None,
                },
            )?;
        let snapshot = &self.sessions[&child].snapshot;
        if repaired.contains(&turn.id) {
            return Some(SubagentReport::new(
                child,
                name,
                outcome,
                None,
                None,
                turn_failure(snapshot, turn),
            ));
        }
        let final_message =
            latest_agent_message(snapshot, turn.id).map(|message| message.content.as_str());
        Some(SubagentReport::new(
            child,
            name,
            outcome,
            turn.worked_ms(),
            final_message,
            turn_failure(snapshot, turn),
        ))
    }

    /// What `holder`'s open row for the brokered Subagent `child` needs to
    /// read as `turn` stands. A row already settled takes nothing more.
    fn row_changes(
        &self,
        holder: SessionId,
        child: SessionId,
        turn: &Turn,
        repaired: &[TurnId],
    ) -> Vec<SessionChange> {
        if self.is_deferred(holder) {
            return Vec::new();
        }
        let Some(record) = self.sessions.get(&holder) else {
            return Vec::new();
        };
        let mut changes = Vec::new();
        for activity in &record.snapshot.activities {
            let Activity::Subagent {
                id,
                status: ActivityStatus::Active,
                model,
                session_id,
                ..
            } = activity
            else {
                continue;
            };
            if *session_id != child {
                continue;
            }
            if let Some(agent) = &turn.agent
                && model.as_ref() != Some(&agent.selection.model)
            {
                changes.push(SessionChange::SubagentModelChanged {
                    activity_id: *id,
                    model: agent.selection.model.clone(),
                });
            }
            let status = match turn.status {
                TurnStatus::Active => continue,
                TurnStatus::Completed => ActivityStatus::Completed,
                TurnStatus::Failed => ActivityStatus::Failed,
                TurnStatus::Interrupted => ActivityStatus::Interrupted,
            };
            let duration_ms = if repaired.contains(&turn.id) {
                None
            } else {
                turn.worked_ms()
            };
            changes.push(SessionChange::SubagentStatusChanged {
                activity_id: *id,
                status,
                duration_ms,
            });
        }
        changes
    }

    /// Whether `subagent` is a brokered Subagent beneath `caller`: its own
    /// Session a brokered Subagent's, and `caller` somewhere up the line of
    /// Sessions that spawned it — its spawner, or its spawner's, at any depth.
    /// A brokered Subagent is its tree's to reach only from above: not from
    /// itself, nor a sibling, nor another tree. A Session whose history is
    /// still unread lies in no tree the caller's hydrated history reaches,
    /// since a tree hydrates whole, and a caller the store does not hold has
    /// nothing beneath it.
    pub(super) fn is_brokered_beneath(&self, subagent: SessionId, caller: SessionId) -> bool {
        !self.is_deferred(subagent)
            && self
                .sessions
                .get(&subagent)
                .is_some_and(|record| record.is_brokered_subagent())
            && self
                .ancestors(subagent)
                .skip(1)
                .any(|(spawner, _)| spawner == caller)
    }

    /// How the brokered Subagent `subagent`, which the caller has found held,
    /// stands `now`: see [`BrokeredSubagentReading`]. Its spawn opened its
    /// Session with a Turn its Delegation heads, which is a stretch of its
    /// work, so it always has one; were it ever without, it would read by its
    /// latest Turn, and without any, as still owed the work it was spawned
    /// for.
    fn brokered_subagent_reading(
        &self,
        subagent: SessionId,
        now: SessionTimestamp,
    ) -> BrokeredSubagentReading {
        let snapshot = &self.sessions[&subagent].snapshot;
        let Some(latest) = stretches_of_work(snapshot)
            .last()
            .copied()
            .or_else(|| snapshot.turns.last())
        else {
            return BrokeredSubagentReading {
                session_id: subagent,
                status: TurnStatus::Active,
                duration_ms: None,
                message: None,
                error: None,
            };
        };
        let duration_ms = match latest.status {
            TurnStatus::Active => latest
                .started_at
                .map(|started| now.0.saturating_sub(started.0)),
            TurnStatus::Completed | TurnStatus::Failed | TurnStatus::Interrupted => {
                self.settled_duration(subagent, snapshot, latest)
            }
        };
        BrokeredSubagentReading {
            session_id: subagent,
            status: latest.status,
            duration_ms,
            message: latest_agent_message(snapshot, latest.id)
                .map(|message| message.content.clone()),
            error: turn_failure(snapshot, latest).map(str::to_owned),
        }
    }

    /// How long a brokered Subagent's settled `turn` worked, as the row its
    /// stretch stands as says — the latest row leading into `subagent` in the
    /// Session whose Agent delegated it — since that is the time the reader
    /// of the delegating Transcript is shown: from the Turn's beginning to its
    /// settling, or unsaid where a restart settled it and nothing timed its
    /// end (ADR 0029). A Turn no Delegation opened stands as no row, and one
    /// whose row never followed it has none to say, so each is timed from the
    /// Turn itself.
    fn settled_duration(
        &self,
        subagent: SessionId,
        snapshot: &SessionSnapshot,
        turn: &Turn,
    ) -> Option<u64> {
        let row = delegating_session(snapshot, turn.id)
            .and_then(|holder| self.sessions.get(&holder))
            .and_then(|holder| {
                holder
                    .snapshot
                    .activities
                    .iter()
                    .rev()
                    .find_map(|activity| match activity {
                        Activity::Subagent {
                            session_id,
                            status,
                            duration_ms,
                            ..
                        } if *session_id == subagent => Some((*status, *duration_ms)),
                        _ => None,
                    })
            });
        match row {
            Some((status, duration_ms)) if status != ActivityStatus::Active => duration_ms,
            _ => turn.worked_ms(),
        }
    }
}

/// `count` in the width the Broker's caps are pinned in. A count past what that
/// width holds passes every cap there is, so it saturates rather than wraps.
fn cap_count(count: usize) -> u32 {
    u32::try_from(count).unwrap_or(u32::MAX)
}

impl SessionRecord {
    /// Whether this Session's own Agent is at work: its latest Turn has not
    /// Settled. A Subagent whose latest Turn has Settled is a settled one
    /// (CONTEXT.md: Subagent), whatever still works beneath it.
    fn is_at_work_itself(&self) -> bool {
        self.snapshot
            .turns
            .last()
            .is_some_and(|turn| turn.status == TurnStatus::Active)
    }
}

/// The latest Message the Agent of `snapshot`'s Session wrote in Turn
/// `turn_id` — its final one there, once the Turn has settled — as the Session
/// holds it, whole. A Message begun but still empty says nothing yet, so the
/// one before it stands. This is what `read_subagent` answers with, and what a
/// Subagent Report excerpts.
pub(crate) fn latest_agent_message(
    snapshot: &SessionSnapshot,
    turn_id: TurnId,
) -> Option<&Message> {
    snapshot.messages.iter().rev().find(|message| {
        message.turn_id == turn_id
            && message.role == MessageRole::Agent
            && !message.content.is_empty()
    })
}

/// What `turn` failed with, as its Transcript says: the text of the last
/// error row recorded in it, which is the row that settled it. `None` for a
/// Turn that has not failed, and for one that failed with no row saying why.
pub(crate) fn turn_failure<'a>(snapshot: &'a SessionSnapshot, turn: &Turn) -> Option<&'a str> {
    if turn.status != TurnStatus::Failed {
        return None;
    }
    snapshot
        .activities
        .iter()
        .rev()
        .find_map(|activity| match activity {
            Activity::Error { turn_id, text, .. } if *turn_id == turn.id => Some(text.as_str()),
            _ => None,
        })
}

/// The Session whose Agent delegated the stretch of work `turn_id` is: the
/// one its opening Delegation names. `None` for a Turn no Delegation opened.
pub(super) fn delegating_session(snapshot: &SessionSnapshot, turn_id: TurnId) -> Option<SessionId> {
    match &snapshot
        .messages
        .iter()
        .find(|message| message.turn_id == turn_id)?
        .role
    {
        MessageRole::Delegation(delegator) => Some(delegator.session_id),
        _ => None,
    }
}
