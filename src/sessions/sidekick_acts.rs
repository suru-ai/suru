//! The Sessions each Sidekick has a hand in: its Subsessions, and every other
//! Session it has acted on — sent a Prompt, answered, interrupted, set aside,
//! or brought back (CONTEXT.md: Subagents Section). Reading a Session is no
//! act.
//!
//! Each act is recorded against the Sidekick's Session with the moment of
//! the latest act on that Session, kept across a stop, and forgotten when
//! either Session is deleted. What the record is for is the tree a Client is
//! given for a Sidekick's Session: it lists them beneath the Sidekick's own
//! Subagents, each with its own Subagents beneath it, and a Subsession's tree
//! is its Sidekick's. A Session only acted on heads its own tree, unchanged,
//! since several Sidekicks may have acted on it.
//!
//! None of it makes those Sessions the Sidekick's in any way that matters to
//! their work: nothing here rolls their Working, Usage or Cost up beneath it,
//! and nothing reaches them when its Session is interrupted or deleted.

use std::collections::HashMap;

use crate::protocol::{SessionId, SessionTimestamp};
use crate::sidekick::SidekickWorkspace;
use crate::storage::{StorageError, StoredSidekickAct};

use super::{SessionStore, SessionStoreState};

/// Every Sidekick's latest act on each Session of this Server it has acted
/// on, by the Sidekick's Session.
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

    /// The Sidekicks' Sessions whose Sidekick acted on `session_id`.
    fn acting_on(&self, session_id: SessionId) -> impl Iterator<Item = SessionId> + '_ {
        self.by_sidekick
            .iter()
            .filter(move |(_, acted_on)| acted_on.contains_key(&session_id))
            .map(|(sidekick, _)| *sidekick)
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

    /// Records that the Sidekick of `sidekick` acted on `session_id` just
    /// now, so the Session stands beneath the Sidekick's in its tree, ordered
    /// by this act. An act on a Subagent's Session is one on the tree it
    /// belongs to, and is recorded against the top-level Session heading it,
    /// since only a top-level Session stands beneath a Sidekick. Nothing is
    /// recorded where either Session is no longer held.
    pub(crate) fn record_sidekick_act(&self, sidekick: SessionId, session_id: SessionId) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let Some(top_level) = state.top_level_of(session_id) else {
            return;
        };
        if top_level == sidekick || !state.sessions.contains_key(&sidekick) {
            return;
        }
        let acted_at = state.next_timestamp();
        state.sidekick_acts.record(sidekick, top_level, acted_at);
        self.storage.record_sidekick_act(StoredSidekickAct {
            sidekick,
            session_id: top_level,
            acted_at,
        });
        state.announce_tree_headed_by(sidekick);
    }

    /// Reads back from storage every Session the tree `session_id` belongs
    /// to spans: its own, the Sidekick's Session heading it where it is a
    /// Subsession's, and every Session that Sidekick has a hand in. One that
    /// cannot be read costs only the Subagents its entry would have had
    /// beneath it.
    pub(crate) async fn hydrate_subagent_tree(
        &self,
        session_id: SessionId,
    ) -> Result<(), StorageError> {
        self.hydrate(session_id).await?;
        let head = {
            let state = self
                .state
                .lock()
                .expect("Session store lock is not poisoned");
            state
                .top_level_of(session_id)
                .map(|top_level| state.tree_head(top_level))
        };
        let Some(head) = head else {
            return Ok(());
        };
        self.hydrate(head).await?;
        let beneath = self
            .state
            .lock()
            .expect("Session store lock is not poisoned")
            .sessions_beneath(head);
        for (listed, _, _) in beneath {
            if let Err(error) = self.hydrate(listed).await {
                tracing::warn!(
                    sidekick = %head,
                    session = %listed,
                    "a Session a Sidekick has a hand in could not be read: {error}"
                );
            }
        }
        Ok(())
    }
}

impl SessionStoreState {
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
    /// one acted on most recently first: each held top-level Session it acted
    /// on, by the moment of its latest act, and each Subsession it began, by
    /// that act or — where none was recorded — the moment it was begun; and
    /// whether it is a Subsession. Nothing stands beneath a Session that is
    /// not a Sidekick's.
    pub(super) fn sessions_beneath(
        &self,
        sidekick: SessionId,
    ) -> Vec<(SessionId, SessionTimestamp, bool)> {
        if !self.is_sidekicks(sidekick) {
            return Vec::new();
        }
        let mut beneath = self
            .sidekick_acts
            .of(sidekick)
            .map(|(session_id, acted_at)| (session_id, (acted_at, false)))
            .collect::<HashMap<_, _>>();
        for subsession in self.subsessions_of(sidekick) {
            let begun_at = self.sessions[&subsession].summary.created_at;
            beneath
                .entry(subsession)
                .and_modify(|(_, began)| *began = true)
                .or_insert((begun_at, true));
        }
        let mut beneath = beneath
            .into_iter()
            .filter(|(session_id, _)| {
                *session_id != sidekick
                    && self
                        .sessions
                        .get(session_id)
                        .is_some_and(|record| !record.snapshot.session.is_subagent())
            })
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
    /// and that of each Sidekick that acted on it.
    pub(super) fn trees_listing(&self, top_level: SessionId) -> Vec<SessionId> {
        let mut trees = vec![top_level];
        let sidekick = self
            .sessions
            .get(&top_level)
            .and_then(|record| record.summary.session.sidekick());
        for tree in sidekick
            .into_iter()
            .chain(self.sidekick_acts.acting_on(top_level))
        {
            if !trees.contains(&tree) {
                trees.push(tree);
            }
        }
        trees
    }
}
