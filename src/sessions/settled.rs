//! A Session's own Settle: the reversible marker that sets it aside as done
//! for now.
//!
//! Distinct from the Settle a Turn or Activity goes through, which is terminal
//! and lives in [`super::settlement`]. This one is a judgement the user makes
//! about work rather than an outcome the Provider reached, so it is stored, it
//! is reversible, and the next Prompt undoes it without anybody asking.
//!
//! What is stored is only the user's own say-so, with the moment they said it.
//! Everything time-based — a Session that has sat untouched long enough to be
//! set aside on its own — is derived where it is read, so the server keeps no
//! timer and a client can classify a listing against whatever clock its tests
//! hand it.

use crate::protocol::{SessionCatalogChange, SessionId, SessionSummary};

use super::{SessionRecord, SessionStore};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SettleSessionError {
    SessionNotFound,
}

impl SessionStore {
    /// Sets a Session aside as done for now, or brings it back, and answers
    /// with the summary the change left standing.
    ///
    /// Asking for the state a Session is already in changes nothing: it neither
    /// re-stamps the moment the work was set aside — saying it twice does not
    /// make it more recent — nor announces a change that did not happen.
    ///
    /// Bringing a Session back also moves its last activity to now, because a
    /// user reaching for work they had set aside is the most recent thing to
    /// have happened to it. Without that, a client deriving settlement from
    /// idle would read the untouched activity of a Session set aside months ago
    /// and put it straight back on the shelf the user just took it off.
    pub(crate) fn settle(
        &self,
        session_id: SessionId,
        settled: bool,
    ) -> Result<SessionSummary, SettleSessionError> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let Some(record) = state.sessions.get(&session_id) else {
            return Err(SettleSessionError::SessionNotFound);
        };
        if record.summary.settled_at.is_some() == settled {
            return Ok(record.summary.clone());
        }
        let stamp = state.next_timestamp();
        let settled_at = settled.then_some(stamp);
        let record = state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock");
        record.summary.settled_at = settled_at;
        if !settled {
            record.summary.updated_at = stamp;
        }
        let summary = record.summary.clone();
        self.storage.summary_changed(summary.clone());
        state.publish_catalog_change(SessionCatalogChange::SettlementChanged {
            session_id,
            settled_at,
        });
        Ok(summary)
    }
}

impl SessionRecord {
    /// Brings this Session back from being set aside, because work has arrived
    /// for it, and answers with the change to announce — or `None` for a
    /// Session that was active all along.
    ///
    /// Called before the commit that carries that work, so the summary the
    /// commit persists is the active one; announcing is the caller's to do
    /// once the commit has landed.
    pub(super) fn reactivate(&mut self, session_id: SessionId) -> Option<SessionCatalogChange> {
        self.summary.settled_at.take()?;
        Some(SessionCatalogChange::SettlementChanged {
            session_id,
            settled_at: None,
        })
    }
}
