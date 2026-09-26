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

use super::{SessionStore, SessionStoreState};

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
}

impl SessionStore {
    /// The Watches live anywhere in the Session's subtree, which is what
    /// interrupting that Session asks its Provider to stop. A Watch belongs to
    /// the Session whose Agent started it, so the subtree — not just the
    /// Session — is what Monitoring and its interrupt both reach (ADR 0030).
    pub(crate) fn live_watches(&self, session_id: SessionId) -> Vec<ProviderWatchId> {
        let state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let mut watches = state
            .subtree(session_id)
            .into_iter()
            .filter_map(|current| state.sessions.get(&current))
            .flat_map(|record| record.watches.keys().cloned())
            .collect::<Vec<_>>();
        watches.sort_unstable();
        watches
    }

    /// The Sessions an interrupt of `session_id` reaches when it stops
    /// Watches: the Session and every Session below it, whose Watch Outcomes
    /// still held for a wake the stop beats are dropped with them.
    pub(crate) fn subtree_sessions(&self, session_id: SessionId) -> Vec<SessionId> {
        self.state
            .lock()
            .expect("Session store lock is not poisoned")
            .subtree(session_id)
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
        state
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?
            .watches
            .entry(watch_id)
            .or_insert(LiveWatch {
                description,
                started_at,
            });
        state.reconcile_liveness(&self.storage, session_id);
        state.announce_subagent_tree(session_id);
        Ok(())
    }

    /// Forgets a Watch that settled, however it ended, and re-derives the
    /// Session's liveness, answering with the Watch's description so its
    /// Watch Outcome can say which Watch it was. A Watch the Session never
    /// recorded — or one already forgotten — leaves everything as it was and
    /// has no description to give.
    pub(crate) fn settle_watch(
        &self,
        session_id: SessionId,
        watch_id: &ProviderWatchId,
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
        Some(settled.description)
    }

    /// Forgets every Watch in the Session's subtree, because the Provider
    /// connection they ran over is gone and none of them will ever report
    /// settling. Each is lost, which wakes nothing, so all that moves is the
    /// Monitoring they held up.
    pub(crate) fn lose_watches(&self, session_id: SessionId) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let watched = state
            .subtree(session_id)
            .into_iter()
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
        state.announce_subagent_tree(session_id);
    }
}

impl SessionStoreState {
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
