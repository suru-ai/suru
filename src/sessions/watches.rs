//! The Watches each Session's Agent left running, from which Monitoring is
//! derived (ADR 0030).
//!
//! A Watch starting or settling moves no Turn and writes no row: it is kept in
//! memory beside the Session it belongs to, and the Session's liveness is
//! re-derived across its ancestry whenever the table changes — and told to the
//! subscribers of its tree, whose entries say Monitoring too. Nothing here
//! outlives the process, because no Watch outlives the Provider process that
//! runs it.

use anyhow::anyhow;

use crate::protocol::{SessionId, SessionTimestamp, WatchSummary};
use crate::provider::ProviderWatchId;

use super::{
    SessionStore, SessionStoreState,
    projection::active_turn_id,
    sidekick_reports::{WatchEnd, watch_started},
};

/// One Watch still live in a Session.
#[derive(Debug)]
pub(super) struct LiveWatch {
    /// What the Watch is doing, in the words its Provider gave it: how the
    /// Working Indicator names what a Monitoring Session waits on, and read
    /// back when it settles, for the Watch Outcome that tells of it.
    pub(super) description: String,
    /// When the Server heard the Watch start, on the Session store's clock, so
    /// it orders against the Turn moments Monitoring is derived beside.
    pub(super) started_at: SessionTimestamp,
    /// The Sidekicks whose work left it running, each owed Sidekick Reports
    /// of what comes of it (CONTEXT.md: Sidekick Report).
    pub(super) sidekicks: Vec<SessionId>,
    /// Those of them whose own interrupt is stopping it, owed no telling
    /// that it ended.
    pub(super) stopped_by: Vec<SessionId>,
}

impl SessionStore {
    /// The Watches live in the Session's subtree that run over the Provider
    /// actor its conversation rides, which is what interrupting that Session
    /// asks that actor's Provider to stop. A Watch belongs to the Session
    /// whose Agent started it, so the subtree — not just the Session — is
    /// what Monitoring and its interrupt both reach (ADR 0030). A Subagent
    /// below it with an actor of its own runs its Watches, and those of
    /// everything below it, over that actor instead, where this Provider
    /// connection cannot reach them.
    pub(crate) fn live_watches(&self, session_id: SessionId) -> Vec<ProviderWatchId> {
        let state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let mut watches = state
            .actor_subtree(session_id)
            .into_iter()
            .filter_map(|current| state.sessions.get(&current))
            .flat_map(|record| record.watches.keys().cloned())
            .collect::<Vec<_>>();
        watches.sort_unstable();
        watches
    }

    /// The Sessions an interrupt of `session_id` reaches when it stops
    /// Watches: the Session and every Session below it riding the same
    /// Provider actor, whose Watch Outcomes still held for a wake the stop
    /// beats are dropped with them.
    pub(crate) fn watch_stop_sessions(&self, session_id: SessionId) -> Vec<SessionId> {
        self.state
            .lock()
            .expect("Session store lock is not poisoned")
            .actor_subtree(session_id)
    }

    /// Records a Watch the Session's Agent left running and re-derives the
    /// Session's liveness: once nothing in its tree is Working, the Watch
    /// keeps it Monitoring. A start repeating a live Watch's identity keeps
    /// the moment the first one was heard, so Monitoring never restarts under
    /// a reader.
    pub(crate) fn start_watch(
        &self,
        session_id: SessionId,
        watch_id: ProviderWatchId,
        description: String,
    ) -> anyhow::Result<()> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        if state.is_deferred(session_id) {
            return Err(anyhow!("Session history must be hydrated before mutation"));
        }
        let started_at = state.next_timestamp();
        let record = state
            .sessions
            .get(&session_id)
            .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?;
        if !record.watches.contains_key(&watch_id) {
            let active = active_turn_id(&record.snapshot)?;
            let sidekicks = watch_started(&mut *state, session_id, active, None);
            if let Some(record) = state.sessions.get_mut(&session_id) {
                record.watches.insert(
                    watch_id,
                    LiveWatch {
                        description,
                        started_at,
                        sidekicks,
                        stopped_by: Vec::new(),
                    },
                );
            }
        }
        state.reconcile_liveness(&self.storage, session_id);
        state.announce_subagent_tree(session_id);
        Ok(())
    }

    /// Forgets a Watch that settled, however it ended — waking its Agent,
    /// where `woke_agent` — and re-derives the Session's liveness, answering
    /// with the Watch's description so its Watch Outcome can say which Watch
    /// it was. A Sidekick whose work left it running hears what its ending
    /// owes (CONTEXT.md: Sidekick Report). A Watch the Session never recorded
    /// — or one already forgotten — leaves everything as it was and has no
    /// description to give.
    pub(crate) fn settle_watch(
        &self,
        session_id: SessionId,
        watch_id: &ProviderWatchId,
        woke_agent: bool,
    ) -> Option<String> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let settled = state
            .sessions
            .get_mut(&session_id)?
            .watches
            .remove(watch_id)?;
        state.reconcile_liveness(&self.storage, session_id);
        state.announce_subagent_tree(session_id);
        let end = if woke_agent {
            WatchEnd::Woke
        } else {
            WatchEnd::Silent
        };
        state.follow_ended_watch(session_id, end, &settled.stopped_by);
        Some(settled.description)
    }

    /// Forgets every Watch that ran over the Provider actor `owner` owns —
    /// its own and those of every Session below it riding that actor —
    /// because the connection they ran over is gone and none of them will
    /// ever report settling. Each is lost, which wakes nothing, so all that
    /// moves is the Monitoring they held up — and what a Sidekick whose work
    /// left one running is owed the telling of, as of a wake one already
    /// settled had yet to bring. A Subagent below with an actor of its own
    /// keeps its Watches: they run over another connection.
    pub(crate) fn lose_watches(&self, owner: SessionId) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let riding = state.actor_subtree(owner);
        let watched = riding
            .iter()
            .copied()
            .filter(|current| {
                state
                    .sessions
                    .get(current)
                    .is_some_and(|record| !record.watches.is_empty())
            })
            .collect::<Vec<_>>();
        for current in watched {
            if let Some(record) = state.sessions.get_mut(&current) {
                record.watches.clear();
            }
            state.reconcile_liveness(&self.storage, current);
        }
        state.announce_subagent_tree(owner);
        for current in riding {
            state.follow_ended_watch(current, WatchEnd::Lost, &[]);
        }
    }

    /// Takes up that the Sidekick of `sidekick` is itself interrupting
    /// `session_id`, which stops the Watches of it and of every Session
    /// below it: of each its own work left running, it is owed no telling
    /// that it ended waking no one, having ended it. One that settles of
    /// itself first, waking its Agent, is the Sidekick's still.
    pub(crate) fn sidekick_stops_watches(&self, sidekick: SessionId, session_id: SessionId) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        for current in state.subtree(session_id) {
            let Some(record) = state.sessions.get_mut(&current) else {
                continue;
            };
            for watch in record.watches.values_mut() {
                if watch.sidekicks.contains(&sidekick) && !watch.stopped_by.contains(&sidekick) {
                    watch.stopped_by.push(sidekick);
                }
            }
        }
    }

    /// Takes up that the interrupt [`Self::sidekick_stops_watches`] marked
    /// Watches for stopped none of them — it failed, or only withdrew a
    /// Prompt — so the Sidekick is owed the telling of their ending still.
    pub(crate) fn sidekick_stopped_no_watches(&self, sidekick: SessionId, session_id: SessionId) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        for current in state.subtree(session_id) {
            let Some(record) = state.sessions.get_mut(&current) else {
                continue;
            };
            for watch in record.watches.values_mut() {
                watch.stopped_by.retain(|held| *held != sidekick);
            }
        }
    }
}

impl SessionStoreState {
    /// The Watches the Session's own Agent left running that are live,
    /// earliest first: none of those of the Sessions below it.
    pub(super) fn own_watches(&self, session_id: SessionId) -> Vec<WatchSummary> {
        let mut watches = self
            .sessions
            .get(&session_id)
            .into_iter()
            .flat_map(|record| record.watches.values())
            .map(|watch| WatchSummary {
                description: watch.description.clone(),
                started_at: watch.started_at,
            })
            .collect::<Vec<_>>();
        watches.sort_by(|left, right| {
            left.started_at
                .cmp(&right.started_at)
                .then_with(|| left.description.cmp(&right.description))
        });
        watches
    }

    /// What each Watch the work of the Sidekick of `sidekick` left running
    /// in `session_id` or a Session below it, live still, is doing, earliest
    /// first.
    pub(super) fn watches_left_by(
        &self,
        session_id: SessionId,
        sidekick: SessionId,
    ) -> Vec<String> {
        let mut watches = self
            .subtree(session_id)
            .into_iter()
            .filter_map(|current| self.sessions.get(&current))
            .flat_map(|record| record.watches.values())
            .filter(|watch| watch.sidekicks.contains(&sidekick))
            .collect::<Vec<_>>();
        watches.sort_by_key(|watch| watch.started_at);
        watches
            .into_iter()
            .map(|watch| watch.description.clone())
            .collect()
    }

    /// What a reader viewing the Session is told it waits on: every Watch
    /// live anywhere in its subtree, earliest first, the way Monitoring rolls
    /// up through the tree.
    pub(super) fn subtree_watches(&self, session_id: SessionId) -> Vec<WatchSummary> {
        let mut watches = self
            .subtree(session_id)
            .into_iter()
            .filter_map(|current| self.sessions.get(&current))
            .flat_map(|record| record.watches.values())
            .map(|watch| WatchSummary {
                description: watch.description.clone(),
                started_at: watch.started_at,
            })
            .collect::<Vec<_>>();
        watches.sort_by(|left, right| {
            left.started_at
                .cmp(&right.started_at)
                .then_with(|| left.description.cmp(&right.description))
        });
        watches
    }
}
