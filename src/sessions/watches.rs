//! The Watches each Session's Agent left running, from which Monitoring is
//! derived (ADR 0030).
//!
//! A Watch starting or settling moves no Turn and writes no row: it is kept in
//! memory beside the Session it belongs to, and the Session's liveness is
//! re-derived across its ancestry whenever the table changes. Nothing here
//! outlives the process, because no Watch outlives the Provider process that
//! runs it.

use anyhow::anyhow;

use crate::protocol::{SessionId, SessionTimestamp};
use crate::provider::ProviderWatchId;

use super::SessionStore;

/// One Watch still live in a Session.
#[derive(Debug)]
pub(super) struct LiveWatch {
    /// What the Watch is doing, in the words its Provider gave it — read back
    /// when it settles, for the Watch Outcome that tells of it.
    pub(super) description: String,
    /// When the Server heard the Watch start, on the Session store's clock, so
    /// it orders against the Turn moments Monitoring is derived beside.
    pub(super) started_at: SessionTimestamp,
}

impl SessionStore {
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
    }
}
