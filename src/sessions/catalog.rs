//! The one voice the Session catalog speaks with: every change to the body of
//! work — made, deleted, retitled, set aside, working — goes through here, so
//! the revisions a client checks for continuity are minted in one place.
//!
//! It is shared between [`super::SessionStoreState`], which announces the
//! changes it decides itself, and every [`super::SessionRecord`], whose commit
//! is where a Turn's liveness is derived and which cannot reach the state that
//! would otherwise own the sender. Every caller already holds the store lock,
//! which is what keeps the revisions sequential; the lock in here is only what
//! lets two owners hold the same counter.

use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

use crate::protocol::{SessionCatalogChange, SessionCatalogRevision, SessionCatalogUpdate};

use super::SESSION_UPDATE_CAPACITY;

/// Where a Session catalog change is announced from, cloned to everyone with
/// changes to announce.
#[derive(Clone)]
pub(super) struct SessionCatalogPublisher {
    inner: Arc<Mutex<CatalogChannel>>,
}

struct CatalogChannel {
    revision: SessionCatalogRevision,
    updates: broadcast::Sender<SessionCatalogUpdate>,
}

impl SessionCatalogPublisher {
    pub(super) fn new() -> Self {
        let (updates, _) = broadcast::channel(SESSION_UPDATE_CAPACITY);
        Self {
            inner: Arc::new(Mutex::new(CatalogChannel {
                revision: SessionCatalogRevision::INITIAL,
                updates,
            })),
        }
    }

    /// Announces one change under the next revision.
    pub(super) fn publish(&self, change: SessionCatalogChange) {
        let mut channel = self
            .inner
            .lock()
            .expect("Session catalog publisher lock is not poisoned");
        channel.revision = SessionCatalogRevision(
            channel
                .revision
                .0
                .checked_add(1)
                .expect("Session catalog revision space is not exhausted"),
        );
        let revision = channel.revision;
        let _ = channel
            .updates
            .send(SessionCatalogUpdate { revision, change });
    }

    /// The revision in force and a receiver opened at it, taken together so a
    /// subscriber misses nothing between the two.
    pub(super) fn subscribe(
        &self,
    ) -> (
        SessionCatalogRevision,
        broadcast::Receiver<SessionCatalogUpdate>,
    ) {
        let channel = self
            .inner
            .lock()
            .expect("Session catalog publisher lock is not poisoned");
        (channel.revision, channel.updates.subscribe())
    }
}
