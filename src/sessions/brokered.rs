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

use std::fmt;

use crate::ansi::NormalizedText;
use crate::protocol::{
    Activity, ActivityStatus, AgentSelection, Message, MessageRole, SessionChange, SessionId,
    SessionSnapshot, SessionTimestamp, Turn, TurnId, TurnStatus,
};
use crate::storage::StorageSink;

use super::{
    SessionStore, SessionStoreState,
    posture::brokered_subagent_posture,
    projection::active_turn_id,
    subagents::{
        ChildSession, SubagentRoute, delegator, opening_brokered_subagent_row, stretches_of_work,
        subagent_title,
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
    Storage(String),
}

impl fmt::Display for BrokeredSpawnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CallerNotFound => formatter
                .write_str("the Session calling the Broker no longer exists on this Suru server"),
            Self::Storage(message) => write!(formatter, "Suru could not record it: {message}"),
        }
    }
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
        let record = state
            .sessions
            .get(&caller)
            .ok_or(BrokeredSpawnError::CallerNotFound)?;
        let holding = match active_turn_id(&record.snapshot)
            .map_err(|error| BrokeredSpawnError::Storage(error.to_string()))?
        {
            Some(working) => HoldingTurn::Working(working),
            None => HoldingTurn::Continuation(Box::new(holding_continuation(&record.snapshot))),
        };
        let delegator = match delegator(&state.sessions, caller).name {
            Some(name) => DelegatingAgent::Subagent { name },
            None => DelegatingAgent::Session {
                title: record.snapshot.title.clone(),
            },
        };
        let approval_posture = brokered_subagent_posture(
            record.snapshot.session.approval_posture,
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
        let row = opening_brokered_subagent_row(
            holding.turn_id(),
            spawn.name,
            spawn.description,
            spawned.session_id,
        );
        if let Err(error) = state.commit(&self.storage, caller, holding.holds(row)) {
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
        if state.is_deferred(caller) || !state.sessions.contains_key(&caller) {
            return Err(BrokeredReadError::CallerNotFound);
        }
        if !state.sessions.contains_key(&subagent) {
            return Err(BrokeredReadError::NoSuchSession);
        }
        if !state.is_brokered_beneath(subagent, caller) {
            return Err(BrokeredReadError::NotBrokeredBeneathCaller);
        }
        Ok(state.brokered_subagent_reading(subagent, SessionTimestamp::now()))
    }
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
            if turn.status == TurnStatus::Interrupted {
                // Whether the stretch was stopped on its own or from above
                // decides whether its delegating Agent hears of it.
                tracing::debug!(
                    %session_id,
                    %turn_id,
                    stopped_by_ancestor = self.stopped_by_ancestor(session_id, turn_id),
                    "a brokered Subagent's stretch of work was stopped"
                );
            }
            let row_changes = self.row_changes(holder, session_id, turn, repaired);
            if row_changes.is_empty() {
                continue;
            }
            if let Err(error) = self.commit(storage, holder, row_changes) {
                tracing::warn!(
                    %session_id,
                    delegating_session = %holder,
                    "a brokered Subagent's row did not follow its Turn: {error:#}"
                );
            }
        }
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

/// The Session whose Agent delegated the stretch of work `turn_id` is: the
/// one its opening Delegation names. `None` for a Turn no Delegation opened.
fn delegating_session(snapshot: &SessionSnapshot, turn_id: TurnId) -> Option<SessionId> {
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
