//! Keeping a Remote in view for the trees listing its Sessions.
//!
//! A Sidekick's tree lists the Sessions it began or acted on at a Remote, and
//! what each is doing is the Remote's to say (see
//! [`crate::sessions::SessionStore::remote_read`]). While some Client watches
//! such a tree, this Server keeps the Remote in view as a Client keeping a
//! Remote in view under Everywhere does: it follows the Remote's catalog of
//! Sessions through the Pairing, reads the Remote's listing whenever it begins
//! following it or loses its place in it, and takes up every change after.
//! A Remote that does not answer — or stops answering partway — is held as
//! silent, so its Sessions stand in the tree with nothing stale beside them,
//! and is tried again after the retry interval for as long as a tree lists
//! it. A Remote whose Pairing has ended takes its Sessions out of the tree
//! with it. Once no Client watches a tree listing the Remote, it is let go of,
//! and read afresh when one does again.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::StreamExt;
use tokio::sync::Notify;

use super::{OriginRefusal, SessionOperations};
use crate::protocol::{
    SESSION_CATALOG_SNAPSHOT_EVENT, SESSION_CATALOG_UPDATED_EVENT, SessionCatalogChange,
    SessionCatalogSnapshot, SessionCatalogUpdate, SessionId,
};

/// Where the Session API streams its catalog of Sessions.
const CATALOG_EVENTS_PATH: &str = "/v1/session-events";

/// The Remotes this Server keeps in view, by name, each with what asks it to
/// read the Remote's listing again.
#[derive(Clone)]
pub(crate) struct RemoteWatches {
    running: Arc<Mutex<HashMap<String, Arc<Notify>>>>,
    /// How long a Remote that does not answer waits before it is tried
    /// again.
    retry: Duration,
    /// How often a Remote kept in view looks for whether any Client still
    /// watches a tree listing it.
    check: Duration,
}

impl RemoteWatches {
    pub(crate) fn new(retry: Duration, check: Duration) -> Self {
        Self {
            running: Arc::default(),
            retry,
            check,
        }
    }
}

/// How following a Remote ended.
enum Followed {
    /// No Client watches a tree listing it any more.
    Unwatched,
    /// It did not answer, or stopped answering.
    Silent,
    /// Its Pairing has ended.
    Unpaired,
}

impl SessionOperations {
    /// Keeps in view every Remote holding a Session the tree `session_id`
    /// belongs to lists, once some Client watches that tree: each such
    /// Session joins the tree as soon as its Remote is read.
    pub(crate) fn keep_remotes_in_view(&self, session_id: SessionId) {
        for remote in self.sessions.remotes_listed_with(session_id) {
            self.keep_remote_in_view(&remote, false);
        }
    }

    /// Keeps the Remote `remote` in view where some Client watches a tree
    /// listing it, reading its listing again where `reread` — a Session
    /// newly acted on there must be read before it can stand in a tree.
    pub(super) fn keep_remote_in_view(&self, remote: &str, reread: bool) {
        let mut running = self
            .remote_watches
            .running
            .lock()
            .expect("Remote watch lock is not poisoned");
        if let Some(asked) = running.get(remote) {
            if reread {
                asked.notify_one();
            }
            return;
        }
        if !self.sessions.is_remote_watched(remote) {
            return;
        }
        let asked = Arc::new(Notify::new());
        running.insert(remote.to_owned(), asked.clone());
        tokio::spawn(self.clone().keep_in_view(remote.to_owned(), asked));
    }

    /// Follows the Remote `remote` for as long as some Client watches a tree
    /// listing it, trying it again after the retry interval while it does
    /// not answer.
    async fn keep_in_view(self, remote: String, asked: Arc<Notify>) {
        loop {
            match self.follow(&remote, &asked).await {
                Followed::Unpaired => {
                    self.sessions.remote_unpaired(&remote);
                    if self.let_go_unless_watched(&remote, true) {
                        return;
                    }
                }
                Followed::Silent => {
                    self.sessions.remote_silent(&remote);
                    if self.let_go_unless_watched(&remote, false) {
                        return;
                    }
                    tokio::select! {
                        () = tokio::time::sleep(self.remote_watches.retry) => {}
                        () = asked.notified() => {}
                    }
                }
                Followed::Unwatched => {
                    if self.let_go_unless_watched(&remote, false) {
                        return;
                    }
                }
            }
        }
    }

    /// Lets go of the Remote `remote` where no Client watches a tree listing
    /// it, answering whether it did. Decided under the same lock a Client's
    /// new interest takes, so interest arriving now is never lost.
    fn let_go_unless_watched(&self, remote: &str, unpaired: bool) -> bool {
        let mut running = self
            .remote_watches
            .running
            .lock()
            .expect("Remote watch lock is not poisoned");
        if !unpaired && self.sessions.is_remote_watched(remote) {
            return false;
        }
        running.remove(remote);
        self.sessions.remote_unwatched(remote);
        true
    }

    /// Follows the Remote `remote`'s catalog of Sessions, reading its listing
    /// when it begins, when asked, and whenever it loses its place in the
    /// catalog, until no Client watches a tree listing it or the Remote stops
    /// answering.
    async fn follow(&self, remote: &str, asked: &Notify) -> Followed {
        let refused = |refusal| match refusal {
            OriginRefusal::UnknownRemote(_) => Followed::Unpaired,
            OriginRefusal::Silent(_) => Followed::Silent,
        };
        let mut events = match self.remotes.events(remote, CATALOG_EVENTS_PATH).await {
            Ok(events) => events,
            Err(refusal) => return refused(refusal),
        };
        let opened = tokio::time::timeout(self.remotes.timeout(), events.next()).await;
        let Ok(Some(Ok(opened))) = opened else {
            return Followed::Silent;
        };
        let Some(mut revision) = (opened.event == SESSION_CATALOG_SNAPSHOT_EVENT)
            .then(|| serde_json::from_str::<SessionCatalogSnapshot>(&opened.data).ok())
            .flatten()
            .map(|snapshot| snapshot.revision)
        else {
            return Followed::Silent;
        };
        if let Err(refusal) = self.read_remote(remote).await {
            return refused(refusal);
        }
        let mut check = tokio::time::interval(self.remote_watches.check);
        check.reset();
        loop {
            tokio::select! {
                event = events.next() => {
                    let Some(Ok(event)) = event else {
                        return Followed::Silent;
                    };
                    if event.event != SESSION_CATALOG_UPDATED_EVENT {
                        continue;
                    }
                    let Ok(update) = serde_json::from_str::<SessionCatalogUpdate>(&event.data)
                    else {
                        return Followed::Silent;
                    };
                    let in_place = update.revision.immediately_follows(revision);
                    revision = update.revision;
                    // A Session begun there may be one a Sidekick here is
                    // about to name, and a change missed leaves the reading
                    // behind, so either reads the listing again.
                    if !in_place || matches!(update.change, SessionCatalogChange::Created { .. }) {
                        if let Err(refusal) = self.read_remote(remote).await {
                            return refused(refusal);
                        }
                    } else {
                        self.sessions.remote_changed(remote, update.change);
                    }
                }
                () = asked.notified() => {
                    if let Err(refusal) = self.read_remote(remote).await {
                        return refused(refusal);
                    }
                }
                _ = check.tick() => {
                    if !self.sessions.is_remote_watched(remote) {
                        return Followed::Unwatched;
                    }
                }
            }
        }
    }

    /// Reads what the Remote `remote` holds now into every tree listing it.
    async fn read_remote(&self, remote: &str) -> Result<(), OriginRefusal> {
        let listed = self.remotes.sessions_of(remote).await?;
        self.sessions.remote_read(remote, listed);
        Ok(())
    }
}
