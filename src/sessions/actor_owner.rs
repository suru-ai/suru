//! Which Session owns the Provider actor a Session's conversation runs on.
//!
//! A Provider actor holds one Provider connection and every conversation that
//! runs over it. A top-level Session owns the actor it runs on. A native
//! Subagent's conversation runs over the connection of the Agent whose
//! Provider spawned it, so its Session owns no actor and reaches its Provider
//! through the nearest ancestor that does. A brokered Subagent runs on a
//! Provider of the delegating Agent's choosing, so its Session will own an
//! actor of its own (ADR 0035).
//!
//! Everything Suru sends a Provider on a Session's behalf — a Decision, an
//! Answer, an Approval Posture, an interrupt, a Watch stop — is routed through
//! [`SessionStoreState::actor_owner_of`], and so is the posture a native
//! Subagent inherits. The tree's top-level Session is a different question,
//! asked for display alone.

use crate::protocol::SessionId;

use super::{SessionRecord, SessionStore, SessionStoreState};

impl SessionRecord {
    /// Whether this Session owns the Provider actor its conversation runs on:
    /// a top-level Session always does, and a Subagent's Session does only
    /// when it was given an actor of its own.
    pub(super) fn owns_provider_actor(&self) -> bool {
        self.snapshot.session.parent.is_none() || self.own_provider_actor
    }
}

impl SessionStore {
    /// The Session whose Provider actor holds `session_id`'s conversation; see
    /// [`SessionStoreState::actor_owner_of`].
    pub(crate) fn actor_owner(&self, session_id: SessionId) -> Option<SessionId> {
        self.state
            .lock()
            .expect("Session store lock is not poisoned")
            .actor_owner_of(session_id)
    }
}

impl SessionStoreState {
    /// The Session whose Provider actor holds `session_id`'s conversation: the
    /// nearest ancestor-or-self that owns a Provider actor. A top-level
    /// Session answers itself, and so does a Subagent's Session with an actor
    /// of its own; a native Subagent's answers the Session whose actor its
    /// Provider spawned it on, at whatever depth.
    ///
    /// `None` when the lineage leaves the store before reaching an owner:
    /// `session_id` is not held, its line reaches a parent the store does not
    /// hold, or the line loops back on itself. Restoration keeps readable
    /// children whose parent is missing, and readable cyclic components, but
    /// promotes neither into a top-level Session, so no actor can have run
    /// their conversations: nothing of them is left to reach, and they have
    /// no owner whose Approval Posture to inherit.
    pub(super) fn actor_owner_of(&self, session_id: SessionId) -> Option<SessionId> {
        let mut current = session_id;
        // A line longer than the store holds Sessions has looped back on
        // itself, which bounds the walk without remembering where it has been.
        for _ in 0..=self.sessions.len() {
            let record = self.sessions.get(&current)?;
            if record.owns_provider_actor() {
                return Some(current);
            }
            current = record
                .snapshot
                .session
                .parent
                .expect("a Session that owns no Provider actor is a Subagent's");
        }
        None
    }

    /// `session_id` and every Session below it whose conversation rides the
    /// same Provider actor as its own, each reached after the Session that
    /// spawned it: what that actor's Provider can reach of the subtree. The
    /// walk stops at any Subagent that owns an actor of its own, because that
    /// Subagent and everything below it ride that actor or one deeper, over
    /// another Provider connection.
    pub(super) fn actor_subtree(&self, session_id: SessionId) -> Vec<SessionId> {
        let mut walk = vec![session_id];
        let mut visit = 0;
        while visit < walk.len() {
            let current = walk[visit];
            walk.extend(
                self.sessions
                    .iter()
                    .filter(|(_, record)| {
                        record.snapshot.session.parent == Some(current)
                            && !record.owns_provider_actor()
                    })
                    .map(|(child_id, _)| *child_id),
            );
            visit += 1;
        }
        walk
    }
}

#[cfg(test)]
mod tests {
    use crate::protocol::SessionId;
    use crate::sessions::{SessionStore, restoration_tests::persisted};
    use crate::storage::{PersistedSession, RestoredSessions, StorageRepository, StorageWriter};

    /// A store holding `readable` as the process that restored them would,
    /// with its writer so the test can stop it.
    async fn restored(
        directory: &std::path::Path,
        readable: Vec<PersistedSession>,
    ) -> (SessionStore, StorageWriter) {
        let repository = StorageRepository::open(directory).await.unwrap();
        let (writer, sink) = StorageWriter::spawn(repository, &[]);
        let store = SessionStore::new(
            RestoredSessions {
                readable,
                ..Default::default()
            },
            sink,
            Vec::new(),
            Default::default(),
        );
        (store, writer)
    }

    /// Gives a Subagent's Session a Provider actor of its own, as a brokered
    /// Subagent's will be given one.
    fn give_own_actor(store: &SessionStore, session_id: SessionId) {
        store
            .state
            .lock()
            .unwrap()
            .sessions
            .get_mut(&session_id)
            .expect("the Session is held")
            .own_provider_actor = true;
    }

    #[tokio::test]
    async fn every_session_of_a_tree_with_one_actor_rides_its_top_level_sessions() {
        let directory = tempfile::tempdir().unwrap();
        let top_level = persisted(directory.path(), None);
        let top_level_id = top_level.snapshot.session.id;
        let subagent = persisted(directory.path(), Some(top_level_id));
        let subagent_id = subagent.snapshot.session.id;
        let nested = persisted(directory.path(), Some(subagent_id));
        let nested_id = nested.snapshot.session.id;
        let (store, writer) = restored(directory.path(), vec![top_level, subagent, nested]).await;

        for session_id in [top_level_id, subagent_id, nested_id] {
            assert_eq!(
                store.actor_owner(session_id),
                Some(top_level_id),
                "{session_id} rides the top-level Session's actor"
            );
        }
        writer.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_subagent_with_an_actor_of_its_own_owns_it_for_everything_below_it() {
        let directory = tempfile::tempdir().unwrap();
        let top_level = persisted(directory.path(), None);
        let top_level_id = top_level.snapshot.session.id;
        let native = persisted(directory.path(), Some(top_level_id));
        let native_id = native.snapshot.session.id;
        let owning = persisted(directory.path(), Some(native_id));
        let owning_id = owning.snapshot.session.id;
        let below = persisted(directory.path(), Some(owning_id));
        let below_id = below.snapshot.session.id;
        let (store, writer) =
            restored(directory.path(), vec![top_level, native, owning, below]).await;
        give_own_actor(&store, owning_id);

        assert_eq!(store.actor_owner(top_level_id), Some(top_level_id));
        assert_eq!(
            store.actor_owner(native_id),
            Some(top_level_id),
            "a native Subagent above the owner still rides the top-level Session's actor"
        );
        assert_eq!(
            store.actor_owner(owning_id),
            Some(owning_id),
            "a Subagent with an actor of its own owns it"
        );
        assert_eq!(
            store.actor_owner(below_id),
            Some(owning_id),
            "a Subagent below it rides its actor, not the top-level Session's"
        );
        writer.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn an_actors_reach_stops_at_a_subagent_with_an_actor_of_its_own() {
        let directory = tempfile::tempdir().unwrap();
        let top_level = persisted(directory.path(), None);
        let top_level_id = top_level.snapshot.session.id;
        let native = persisted(directory.path(), Some(top_level_id));
        let native_id = native.snapshot.session.id;
        let owning = persisted(directory.path(), Some(native_id));
        let owning_id = owning.snapshot.session.id;
        let below = persisted(directory.path(), Some(owning_id));
        let below_id = below.snapshot.session.id;
        let (store, writer) =
            restored(directory.path(), vec![top_level, native, owning, below]).await;

        assert_eq!(
            store.state.lock().unwrap().actor_subtree(top_level_id),
            [top_level_id, native_id, owning_id, below_id],
            "with one actor, it reaches the whole subtree"
        );
        give_own_actor(&store, owning_id);
        {
            let state = store.state.lock().unwrap();
            assert_eq!(
                state.actor_subtree(top_level_id),
                [top_level_id, native_id],
                "the top-level Session's actor stops where another actor's reach begins"
            );
            assert_eq!(
                state.actor_subtree(native_id),
                [native_id],
                "a native Subagent's reach is its actor's"
            );
            assert_eq!(
                state.actor_subtree(owning_id),
                [owning_id, below_id],
                "the owning Subagent's actor reaches everything below it"
            );
        }
        writer.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_lineage_that_leaves_the_store_reaches_no_actor() {
        let directory = tempfile::tempdir().unwrap();
        let orphan = persisted(directory.path(), Some(SessionId::new()));
        let orphan_id = orphan.snapshot.session.id;
        let below_orphan = persisted(directory.path(), Some(orphan_id));
        let below_orphan_id = below_orphan.snapshot.session.id;
        let mut cycle = persisted(directory.path(), None);
        let other = persisted(directory.path(), Some(cycle.snapshot.session.id));
        cycle.snapshot.session.parent = Some(other.snapshot.session.id);
        cycle.summary.session.parent = cycle.snapshot.session.parent;
        let (cycle_id, other_id) = (cycle.snapshot.session.id, other.snapshot.session.id);
        let (store, writer) =
            restored(directory.path(), vec![orphan, below_orphan, cycle, other]).await;

        assert_eq!(
            store.actor_owner(orphan_id),
            None,
            "a Subagent whose parent is missing"
        );
        assert_eq!(
            store.actor_owner(below_orphan_id),
            None,
            "a Subagent below one whose parent is missing"
        );
        assert_eq!(store.actor_owner(cycle_id), None, "a cyclic lineage");
        assert_eq!(store.actor_owner(other_id), None, "a cyclic lineage");
        assert_eq!(
            store.actor_owner(SessionId::new()),
            None,
            "a Session the store does not hold"
        );
        writer.shutdown().await.unwrap();
    }
}
