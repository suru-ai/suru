//! Output a Provider owes to brokered Subagents.
//!
//! A Turn settles at its Provider's own boundary while the Subagents it
//! delegated to work on (ADR 0015), and output that Provider sends after that,
//! with no Turn active, is owed to them: it begins a Continuation rather than
//! being discarded as stray (CONTEXT.md: Continuation). A Provider actor
//! knows the native Subagents riding its own connection. A brokered Subagent
//! runs on an actor of its own (ADR 0035), so its working and its settling
//! reach the delegating Agent's actor through nothing but the store, which
//! answers for them here — whichever path settled the stretch, and whether or
//! not a Subagent Report told the delegating Agent of it.

use std::collections::HashSet;

use crate::protocol::{Activity, SessionId, SessionTimestamp, Turn, TurnStatus};

use super::{
    SessionRecord, SessionStore, SessionStoreState, brokered::delegating_session,
    subagents::stretches_of_work,
};

impl SessionStore {
    /// Whether output arriving over the Provider actor `owner` owns, while no
    /// Turn of `owner` is active, is owed a Continuation of `owner` on account
    /// of the brokered Subagents its Agents delegated to; see
    /// [`SessionStoreState::owes_continuation_to_brokered_subagents`].
    pub(crate) fn owes_continuation_to_brokered_subagents(&self, owner: SessionId) -> bool {
        self.state
            .lock()
            .expect("Session store lock is not poisoned")
            .owes_continuation_to_brokered_subagents(owner)
    }
}

impl SessionStoreState {
    /// Whether output arriving over the Provider actor `owner` owns, while no
    /// Turn of `owner` is active, is owed a Continuation of `owner` on account
    /// of brokered Subagents: some stretch of brokered work an Agent on that
    /// actor delegated — `owner`'s own, or a native Subagent's riding it —
    /// still works, or settled since the output owed for it was last
    /// answered, so what its settling provokes has yet to find a Turn to land
    /// in. That is the brokered half of what the actor asks of its own native
    /// routes, and it is answered the way theirs is: by `owner`'s latest
    /// stretch of work beginning, since output from then on has that Turn to
    /// land in, and by an interrupt reaching the actor's work, since whatever
    /// its Provider sends after that is the interrupted stream trailing on.
    ///
    /// A Continuation begun only to hold a spawn's row answers nothing: its
    /// Agent did no work there. A stretch an interrupt from above stopped —
    /// working still, the stop on its way, or settled by it — owes nothing
    /// either: the Agent that delegated it was interrupted too, and hears
    /// nothing of it (CONTEXT.md: Subagent Report).
    pub(super) fn owes_continuation_to_brokered_subagents(&self, owner: SessionId) -> bool {
        let Some(record) = self.sessions.get(&owner) else {
            return false;
        };
        let owed_since = stretches_of_work(&record.snapshot)
            .last()
            .and_then(|turn| turn.started_at)
            .max(record.work_interrupted_at);
        let delegators = self.actor_subtree(owner);
        let delegated = delegators
            .iter()
            .filter_map(|delegator| self.sessions.get(delegator))
            .flat_map(|delegator| &delegator.snapshot.activities)
            .filter_map(|activity| match activity {
                Activity::Subagent {
                    brokered: true,
                    session_id,
                    ..
                } => Some(*session_id),
                _ => None,
            })
            .collect::<HashSet<_>>();
        delegated
            .into_iter()
            .filter_map(|subagent| self.sessions.get(&subagent))
            .any(|subagent| {
                subagent.snapshot.turns.iter().any(|turn| {
                    delegating_session(&subagent.snapshot, turn.id)
                        .is_some_and(|delegator| delegators.contains(&delegator))
                        && subagent.stretch_owes_output(turn, owed_since)
                })
            })
    }

    /// Records that an interrupt has just reached the work of the Provider
    /// actor `owner` owns, standing down whatever output its Provider still
    /// owed brokered Subagents: from here on a stretch owes any only by
    /// working, or settling later, without that interrupt having stopped it.
    /// The moment is minted from the store's own clock, so every settle it is
    /// measured against is ordered with it.
    pub(super) fn record_work_interrupted(&mut self, owner: SessionId) {
        let interrupted_at = self.next_timestamp();
        if let Some(record) = self.sessions.get_mut(&owner) {
            record.work_interrupted_at = Some(interrupted_at);
        }
    }
}

impl SessionRecord {
    /// Whether `turn`, a stretch of this brokered Subagent's work, still owes
    /// its delegating Agent output: it works, or it settled after `since` —
    /// unless an interrupt from above stopped it. With no `since` to measure
    /// against, only a stretch still working owes any.
    fn stretch_owes_output(&self, turn: &Turn, since: Option<SessionTimestamp>) -> bool {
        let stopped_from_above = self.stopped_by_ancestor == Some(turn.id)
            && matches!(turn.status, TurnStatus::Active | TurnStatus::Interrupted);
        if stopped_from_above {
            return false;
        }
        match turn.status {
            TurnStatus::Active => true,
            TurnStatus::Completed | TurnStatus::Failed | TurnStatus::Interrupted => turn
                .settled_at
                .zip(since)
                .is_some_and(|(settled, since)| settled > since),
        }
    }
}
