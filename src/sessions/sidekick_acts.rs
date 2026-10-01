//! The Sessions each Sidekick has a hand in: its Subsessions, and every other
//! Session it has acted on — sent a Prompt, answered, interrupted, set aside,
//! or brought back (CONTEXT.md: Subagents Section). Reading a Session is no
//! act.
//!
//! Each act is recorded against the Sidekick's Session, naming the Session it
//! acted on — a Subagent's own, where it acted on one — and the moment of its
//! latest act on that Session. The record is kept across a stop, landing in
//! the same write as the change the act made where it made one, and is
//! forgotten when either Session is deleted. What the record is for is the
//! tree a Client is given for a Sidekick's Session: it lists, beneath the
//! Sidekick's own Subagents, the top-level Session heading each Session acted
//! on, each with its own Subagents beneath it, and a Subsession's tree is its
//! Sidekick's. A Session only acted on heads its own tree, unchanged, since
//! several Sidekicks may have acted on it.
//!
//! A listed Session is drawn from what was stored of it without reading its
//! history (ADR 0022): its summary and Turns, which a restored Session holds
//! from startup, and the rows recording the Subagents it spawned, read alone
//! (see [`SessionStore::prepare_subagent_tree`]). Its history is read only
//! when it is opened or acted on.
//!
//! None of it makes those Sessions the Sidekick's in any way that matters to
//! their work: nothing here rolls their Working, Usage or Cost up beneath it,
//! and nothing reaches them when its Session is interrupted or deleted.

use std::collections::{HashMap, HashSet};

use crate::protocol::{Activity, SessionId, SessionTimestamp};
use crate::sidekick::SidekickWorkspace;
use crate::storage::{StorageError, StoredSidekickAct};

use super::{SessionStore, SessionStoreState};

/// Every Sidekick's latest act on each Session of this Server it has acted
/// on, by the Sidekick's Session and then the Session acted on.
#[derive(Default)]
pub(super) struct SidekickActs {
    by_sidekick: HashMap<SessionId, HashMap<SessionId, SessionTimestamp>>,
}

impl SidekickActs {
    fn record(&mut self, sidekick: SessionId, session_id: SessionId, at: SessionTimestamp) {
        let acted_at = self
            .by_sidekick
            .entry(sidekick)
            .or_default()
            .entry(session_id)
            .or_insert(at);
        *acted_at = (*acted_at).max(at);
    }

    /// Each Session the Sidekick of `sidekick` acted on, with the moment of
    /// its latest act on it.
    fn of(&self, sidekick: SessionId) -> impl Iterator<Item = (SessionId, SessionTimestamp)> + '_ {
        self.by_sidekick
            .get(&sidekick)
            .into_iter()
            .flatten()
            .map(|(session_id, acted_at)| (*session_id, *acted_at))
    }

    /// Every act, as (the Sidekick's Session, the Session it acted on).
    fn all(&self) -> impl Iterator<Item = (SessionId, SessionId)> + '_ {
        self.by_sidekick.iter().flat_map(|(sidekick, acted_on)| {
            acted_on
                .keys()
                .map(move |session_id| (*sidekick, *session_id))
        })
    }

    /// Forgets every act of the Sidekick of `session_id`, and every act on
    /// it, as the deletion of its Session does.
    pub(super) fn forget(&mut self, session_id: SessionId) {
        self.by_sidekick.remove(&session_id);
        for acted_on in self.by_sidekick.values_mut() {
            acted_on.remove(&session_id);
        }
        self.by_sidekick.retain(|_, acted_on| !acted_on.is_empty());
    }

    /// The latest moment any act was recorded at.
    fn latest(&self) -> Option<SessionTimestamp> {
        self.by_sidekick
            .values()
            .flat_map(HashMap::values)
            .copied()
            .max()
    }
}

impl SessionStore {
    /// Knows Sidekicks' Sessions by `workspace`, and takes up every act
    /// `acts` read back from storage. The server always chains this; a test
    /// that omits it holds no Sidekick, so every tree it reads is its own.
    pub(crate) fn with_sidekicks(
        self,
        workspace: SidekickWorkspace,
        acts: Vec<StoredSidekickAct>,
    ) -> Self {
        {
            let mut state = self
                .state
                .lock()
                .expect("Session store lock is not poisoned");
            for act in acts {
                state
                    .sidekick_acts
                    .record(act.sidekick, act.session_id, act.acted_at);
            }
            // The store minted every act's moment, so its clock resumes past
            // them as it does past every other moment it minted.
            state.last_timestamp = state.last_timestamp.max(state.sidekick_acts.latest());
            state.sidekick_workspace = Some(workspace);
        }
        self
    }

    /// Records that the Sidekick of `sidekick` just acted on `session_id`
    /// where the act changed nothing the Session stores — an interrupt,
    /// whose work its Provider settles later, or a Session set aside that
    /// already was — so the record is written on its own.
    pub(crate) fn record_sidekick_act(&self, sidekick: SessionId, session_id: SessionId) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        if let Some(act) = state.note_sidekick_act(sidekick, session_id) {
            self.storage.record_sidekick_act(act);
        }
    }

    /// Brings into memory what the tree `session_id` belongs to needs to be
    /// drawn: the history of `session_id` itself, which a reader is opening,
    /// and, for each Session of the tree not yet read — the Sidekick's
    /// Session heading it where `session_id` is a Subsession, each Session
    /// that Sidekick has a hand in, and every Subagent beneath them — the
    /// stored rows recording the Subagents it spawned, and nothing else of
    /// its history.
    pub(crate) async fn prepare_subagent_tree(
        &self,
        session_id: SessionId,
    ) -> Result<(), StorageError> {
        self.hydrate(session_id).await?;
        let (repository, unread) = {
            let state = self
                .state
                .lock()
                .expect("Session store lock is not poisoned");
            let Some(top_level) = state.top_level_of(session_id) else {
                return Ok(());
            };
            let head = state.tree_head(top_level);
            let roots = std::iter::once(head)
                .chain(
                    state
                        .sessions_beneath(head)
                        .into_iter()
                        .map(|(listed, _, _)| listed),
                )
                .collect::<Vec<_>>();
            let unread = state
                .subtrees_of(&roots)
                .into_iter()
                .filter(|id| state.is_deferred(*id) && !state.stored_subagent_rows.contains_key(id))
                .collect::<Vec<_>>();
            let Some(deferred) = state.deferred.as_ref() else {
                return Ok(());
            };
            (deferred.repository.clone(), unread)
        };
        if unread.is_empty() {
            return Ok(());
        }
        let mut rows = repository.subagent_rows(unread.clone()).await?;
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        for id in unread {
            // One read meanwhile holds its rows in its history instead.
            if state.is_deferred(id) {
                let rows = rows.remove(&id).unwrap_or_default();
                state.stored_subagent_rows.insert(id, rows);
            }
        }
        Ok(())
    }
}

impl SessionStoreState {
    /// Records that the Sidekick of `sidekick` acted on `session_id` just
    /// now, answering the record to store: the Session acted on stands beneath
    /// the Sidekick's in its tree from now on — by the top-level Session
    /// heading its tree, where it is a Subagent's — ordered by this act.
    /// Nothing is recorded where either Session is no longer held, or where
    /// the Session acted on is in the Sidekick's own tree.
    pub(super) fn note_sidekick_act(
        &mut self,
        sidekick: SessionId,
        session_id: SessionId,
    ) -> Option<StoredSidekickAct> {
        let top_level = self.top_level_of(session_id)?;
        if top_level == sidekick || !self.sessions.contains_key(&sidekick) {
            return None;
        }
        let acted_at = self.next_timestamp();
        self.sidekick_acts.record(sidekick, session_id, acted_at);
        self.announce_tree_headed_by(sidekick);
        Some(StoredSidekickAct {
            sidekick,
            session_id,
            acted_at,
        })
    }

    /// Whether `session_id` is a Sidekick's Session: a top-level Session of
    /// the Sidekick Workspace.
    pub(super) fn is_sidekicks(&self, session_id: SessionId) -> bool {
        self.sidekick_workspace.as_ref().is_some_and(|workspace| {
            self.sessions
                .get(&session_id)
                .is_some_and(|record| workspace.is_sidekicks(&record.snapshot.session))
        })
    }

    /// The Session heading the tree a Client is given for the top-level
    /// Session `top_level`: its Sidekick's, for a Subsession whose Sidekick's
    /// Session is still held, and its own otherwise.
    pub(super) fn tree_head(&self, top_level: SessionId) -> SessionId {
        self.sessions
            .get(&top_level)
            .and_then(|record| record.summary.session.sidekick())
            .filter(|sidekick| self.is_sidekicks(*sidekick))
            .unwrap_or(top_level)
    }

    /// The Sessions standing beneath the Sidekick's Session `sidekick`, the
    /// one acted on most recently first: the held top-level Session heading
    /// each Session it acted on, by the moment of its latest act on any of
    /// them, and each Subsession it began, by that act or — where none was
    /// recorded — the moment it was begun; and whether it is a Subsession.
    /// Nothing stands beneath a Session that is not a Sidekick's.
    pub(super) fn sessions_beneath(
        &self,
        sidekick: SessionId,
    ) -> Vec<(SessionId, SessionTimestamp, bool)> {
        if !self.is_sidekicks(sidekick) {
            return Vec::new();
        }
        let mut beneath = HashMap::<SessionId, (SessionTimestamp, bool)>::new();
        for (acted_on, acted_at) in self.sidekick_acts.of(sidekick) {
            let Some(top_level) = self.top_level_of(acted_on) else {
                continue;
            };
            let (latest, _) = beneath.entry(top_level).or_insert((acted_at, false));
            *latest = (*latest).max(acted_at);
        }
        for subsession in self.subsessions_of(sidekick) {
            let begun_at = self.sessions[&subsession].summary.created_at;
            beneath
                .entry(subsession)
                .and_modify(|(_, began)| *began = true)
                .or_insert((begun_at, true));
        }
        let mut beneath = beneath
            .into_iter()
            .filter(|(session_id, _)| *session_id != sidekick)
            .map(|(session_id, (acted_at, began))| (session_id, acted_at, began))
            .collect::<Vec<_>>();
        beneath.sort_by_key(|(session_id, acted_at, _)| {
            (
                std::cmp::Reverse(*acted_at),
                std::cmp::Reverse(session_id.as_uuid()),
            )
        });
        beneath
    }

    /// Every tree that lists the top-level Session `top_level`, by the
    /// Session heading it: its own, its Sidekick's where it is a Subsession,
    /// and that of each Sidekick that acted on it or on a Subagent beneath
    /// it.
    pub(super) fn trees_listing(&self, top_level: SessionId) -> Vec<SessionId> {
        let mut trees = vec![top_level];
        let sidekick = self
            .sessions
            .get(&top_level)
            .and_then(|record| record.summary.session.sidekick());
        let acting = self
            .sidekick_acts
            .all()
            .filter(|(_, acted_on)| {
                *acted_on == top_level || self.top_level_of(*acted_on) == Some(top_level)
            })
            .map(|(sidekick, _)| sidekick);
        for tree in sidekick.into_iter().chain(acting) {
            if !trees.contains(&tree) {
                trees.push(tree);
            }
        }
        trees
    }

    /// Each of `roots` and every Session beneath it, at any depth.
    fn subtrees_of(&self, roots: &[SessionId]) -> Vec<SessionId> {
        let mut children = HashMap::<SessionId, Vec<SessionId>>::new();
        for (id, record) in &self.sessions {
            if let Some(parent) = record.snapshot.session.parent {
                children.entry(parent).or_default().push(*id);
            }
        }
        let mut visited = roots.iter().copied().collect::<HashSet<_>>();
        let mut walk = roots.to_vec();
        let mut at = 0;
        while at < walk.len() {
            for child in children.get(&walk[at]).into_iter().flatten() {
                if visited.insert(*child) {
                    walk.push(*child);
                }
            }
            at += 1;
        }
        walk
    }

    /// The rows recording the Subagents `spawner` spawned, in the order its
    /// Transcript holds them: read from its history where that is held, and
    /// otherwise from the rows read alone for its tree, or none where they
    /// were not.
    pub(super) fn subagent_rows_of(&self, spawner: SessionId) -> Vec<&Activity> {
        if self.is_deferred(spawner) {
            return self
                .stored_subagent_rows
                .get(&spawner)
                .into_iter()
                .flatten()
                .collect();
        }
        self.sessions
            .get(&spawner)
            .into_iter()
            .flat_map(|record| &record.snapshot.activities)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use diesel::{Connection, SqliteConnection, connection::SimpleConnection};

    use super::super::restoration_tests::persisted;
    use super::*;
    use crate::storage::{StorageRepository, StorageWriter};

    fn act(sidekick: SessionId, session_id: SessionId, acted_at: u64) -> StoredSidekickAct {
        StoredSidekickAct {
            sidekick,
            session_id,
            acted_at: SessionTimestamp(acted_at),
        }
    }

    fn database(directory: &std::path::Path) -> SqliteConnection {
        SqliteConnection::establish(
            directory
                .join("suru.db")
                .to_str()
                .expect("the fixture's path is UTF-8"),
        )
        .expect("open the database")
    }

    #[tokio::test]
    async fn an_act_riding_a_change_lands_with_it_after_the_sidekicks_session_it_names() {
        let directory = tempfile::tempdir().expect("create a data root");
        let repository = StorageRepository::open(directory.path())
            .await
            .expect("open storage");
        let (writer, sink) = StorageWriter::spawn(repository.clone(), &[]);
        let sidekick = persisted(directory.path(), None);
        let subsession = persisted(directory.path(), None);
        let begun = act(
            sidekick.snapshot.session.id,
            subsession.snapshot.session.id,
            3,
        );
        // Neither Session has landed yet when the act riding the second one's
        // creation is flushed.
        sink.created(sidekick, Vec::new());
        sink.created(subsession, vec![begun.clone()]);
        writer.shutdown().await.expect("stop the writer");

        assert_eq!(
            repository.sidekick_acts().await.expect("read the acts"),
            [begun]
        );
    }

    #[tokio::test]
    async fn an_act_whose_write_fails_is_kept_and_written_once_storage_takes_it() {
        let directory = tempfile::tempdir().expect("create a data root");
        let repository = StorageRepository::open(directory.path())
            .await
            .expect("open storage");
        let (writer, sink) = StorageWriter::spawn(repository.clone(), &[]);
        let sidekick = persisted(directory.path(), None);
        let target = persisted(directory.path(), None);
        let interrupted = act(sidekick.snapshot.session.id, target.snapshot.session.id, 3);
        let (session, revision) = (target.snapshot.session.clone(), target.snapshot.revision);
        sink.created(sidekick, Vec::new());
        sink.created(target, Vec::new());

        // Storage refuses the act: the table it is written to is gone.
        database(directory.path())
            .batch_execute("ALTER TABLE sidekick_acts RENAME TO sidekick_acts_away;")
            .expect("take the table away");
        sink.record_sidekick_act(interrupted.clone());
        // Commands are taken in order, so once this one is answered the act
        // has been tried, and failed.
        sink.location_changed(session, revision)
            .expect("the writer goes on after the failure");
        database(directory.path())
            .batch_execute("ALTER TABLE sidekick_acts_away RENAME TO sidekick_acts;")
            .expect("give the table back");

        writer.shutdown().await.expect("stop the writer");
        assert_eq!(
            repository.sidekick_acts().await.expect("read the acts"),
            [interrupted],
            "the act was kept, and written once storage took it"
        );
    }
}
