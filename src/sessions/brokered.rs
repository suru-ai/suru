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

use std::fmt;

use crate::ansi::NormalizedText;
use crate::protocol::{
    Activity, ActivityId, ActivityStatus, AgentSelection, MessageRole, SessionChange, SessionId,
    SessionSnapshot, Turn, TurnId, TurnStatus,
};
use crate::storage::StorageSink;

use super::{
    SessionStore, SessionStoreState,
    posture::brokered_subagent_posture,
    projection::active_turn_id,
    subagents::{ChildSession, SubagentRoute, delegator, subagent_title},
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
    /// The calling Session has no Turn working to hold the Subagent's row.
    NoWorkingTurn,
    Storage(String),
}

impl fmt::Display for BrokeredSpawnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CallerNotFound => formatter
                .write_str("the Session calling the Broker no longer exists on this Suru server"),
            Self::NoWorkingTurn => formatter.write_str(
                "the Session calling the Broker has no Turn working to hold the Subagent's row; \
                 spawn it from within a working Turn",
            ),
            Self::Storage(message) => write!(formatter, "Suru could not record it: {message}"),
        }
    }
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
    /// the child; a caller with no Turn working is refused, and nothing is
    /// spawned. Nothing here reaches a Provider: whoever spawned it starts
    /// the child's actor and delivers the Delegation.
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
        let delegating_turn = active_turn_id(&record.snapshot)
            .map_err(|error| BrokeredSpawnError::Storage(error.to_string()))?
            .ok_or(BrokeredSpawnError::NoWorkingTurn)?;
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
        let row = SessionChange::ActivityAdded {
            activity: Activity::Subagent {
                id: ActivityId::new(),
                turn_id: delegating_turn,
                status: ActivityStatus::Active,
                name: spawn.name,
                description: spawn.description,
                // Unknown until the child's own Provider confirms one.
                model: None,
                session_id: spawned.session_id,
                duration_ms: None,
            },
        };
        if let Err(error) = state.commit(&self.storage, caller, vec![row]) {
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
                turn.settled_at
                    .zip(turn.started_at)
                    .and_then(|(settled, started)| settled.0.checked_sub(started.0))
            };
            changes.push(SessionChange::SubagentStatusChanged {
                activity_id: *id,
                status,
                duration_ms,
            });
        }
        changes
    }
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
