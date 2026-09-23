//! The live reading of the tree a top-level Session heads: that Session and
//! every Subagent's Session beneath the one that spawned it, to any depth.
//!
//! The tree is read off the Subagent rows each Session's Transcript already
//! carries — the rows the Provider-neutral Subagent orchestration writes on a
//! spawn, a settle, and an update — so every Provider is covered without a
//! word of its own. A subscribed tree is read again after each commit that
//! could move it and compared with what its subscribers last heard, so the
//! changes they are told about are exactly the difference, however the rows
//! came to move.

use std::collections::{HashMap, HashSet};

use tokio::sync::broadcast;

use crate::protocol::{
    Activity, SessionChange, SessionId, SubagentTreeChange, SubagentTreeEntry,
    SubagentTreeRevision, SubagentTreeSnapshot, SubagentTreeTopLevel, SubagentTreeUpdate,
};

use super::{SESSION_UPDATE_CAPACITY, SessionStore, SessionStoreState};

/// A subscription's opening snapshot, with a receiver opened at its revision.
pub(crate) struct SubagentTreeFeed {
    pub(crate) snapshot: SubagentTreeSnapshot,
    pub(crate) updates: broadcast::Receiver<SubagentTreeUpdate>,
}

/// The tree's reading without a revision, which is what two readings are
/// compared by.
#[derive(Clone, Debug, Eq, PartialEq)]
struct SubagentTree {
    top_level: SubagentTreeTopLevel,
    subagents: Vec<SubagentTreeEntry>,
}

/// Every tree some client is subscribed to, keyed by the top-level Session
/// heading it. A tree nobody listens to is forgotten, so a commit anywhere
/// else costs nothing more than finding that out.
#[derive(Default)]
pub(super) struct SubagentTreePublisher {
    trees: HashMap<SessionId, TreeChannel>,
}

struct TreeChannel {
    revision: SubagentTreeRevision,
    updates: broadcast::Sender<SubagentTreeUpdate>,
    /// The tree as its subscribers last heard it.
    announced: SubagentTree,
}

impl SubagentTreePublisher {
    /// Forgets a tree outright, ending every subscription to it. Used when the
    /// top-level Session heading it is gone.
    pub(super) fn forget(&mut self, top_level: SessionId) {
        self.trees.remove(&top_level);
    }
}

impl SessionStore {
    /// Subscribes to the tree `session_id` belongs to, whichever Session in it
    /// that is. `None` when no such Session is held, or its history still
    /// waits to be read.
    pub(crate) fn subscribe_subagent_tree(
        &self,
        session_id: SessionId,
    ) -> Option<SubagentTreeFeed> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        if state.is_deferred(session_id) {
            return None;
        }
        let top_level = state.top_level_of(session_id)?;
        let tree = state.read_subagent_tree(top_level)?;
        // An existing subscription is brought up to this reading before
        // another joins it, so the joiner's snapshot and the revision it opens
        // at agree with what everyone else has heard.
        state.subagent_trees.announce(top_level, tree.clone());
        let channel = state
            .subagent_trees
            .trees
            .entry(top_level)
            .or_insert_with(|| TreeChannel {
                revision: SubagentTreeRevision::INITIAL,
                updates: broadcast::channel(SESSION_UPDATE_CAPACITY).0,
                announced: tree.clone(),
            });
        Some(SubagentTreeFeed {
            snapshot: SubagentTreeSnapshot {
                revision: channel.revision,
                top_level: tree.top_level,
                subagents: tree.subagents,
            },
            updates: channel.updates.subscribe(),
        })
    }
}

impl SessionStoreState {
    /// Tells the subscribers of the tree `session_id` belongs to whatever a
    /// commit to it moved. Cheap when nobody subscribes to that tree.
    pub(super) fn announce_subagent_tree(&mut self, session_id: SessionId) {
        if self.subagent_trees.trees.is_empty() {
            return;
        }
        let Some(top_level) = self.top_level_of(session_id) else {
            return;
        };
        if !self.subagent_trees.trees.contains_key(&top_level) {
            return;
        }
        match self.read_subagent_tree(top_level) {
            Some(tree) => self.subagent_trees.announce(top_level, tree),
            None => self.subagent_trees.forget(top_level),
        }
    }

    /// The top-level Session heading the tree `session_id` belongs to, or
    /// `None` when the Session is not held or its line up to a top-level
    /// Session is broken.
    fn top_level_of(&self, session_id: SessionId) -> Option<SessionId> {
        let mut current = session_id;
        let mut visited = HashSet::new();
        loop {
            if !visited.insert(current) {
                return None;
            }
            match self.sessions.get(&current)?.snapshot.session.parent {
                None => return Some(current),
                Some(parent) => current = parent,
            }
        }
    }

    /// Reads the tree `top_level` heads, depth-first in spawn order.
    fn read_subagent_tree(&self, top_level: SessionId) -> Option<SubagentTree> {
        let record = self.sessions.get(&top_level)?;
        let mut subagents = Vec::new();
        let mut visited = HashSet::from([top_level]);
        // Each frame is a spawner's Subagents still to visit, nearest first.
        let mut stack = vec![self.spawned_by(top_level).into_iter()];
        while let Some(frame) = stack.last_mut() {
            let Some(entry) = frame.next() else {
                stack.pop();
                continue;
            };
            let child = entry.session_id;
            subagents.push(entry);
            if visited.insert(child) {
                stack.push(self.spawned_by(child).into_iter());
            }
        }
        Some(SubagentTree {
            top_level: SubagentTreeTopLevel {
                session_id: top_level,
                title: record.snapshot.title.clone(),
            },
            subagents,
        })
    }

    /// The Subagents `spawner`'s Transcript records, in the order they
    /// spawned.
    fn spawned_by(&self, spawner: SessionId) -> Vec<SubagentTreeEntry> {
        let Some(record) = self.sessions.get(&spawner) else {
            return Vec::new();
        };
        record
            .snapshot
            .activities
            .iter()
            .filter_map(|activity| match activity {
                Activity::Subagent {
                    status,
                    name,
                    description,
                    session_id,
                    duration_ms,
                    ..
                } => Some((*session_id, name, description, *status, *duration_ms)),
                _ => None,
            })
            .zip(0..)
            .map(
                |((session_id, name, description, status, duration_ms), spawn_order)| {
                    SubagentTreeEntry {
                        session_id,
                        parent_session_id: spawner,
                        spawn_order,
                        name: name.clone(),
                        title: description.clone(),
                        status,
                        duration_ms,
                    }
                },
            )
            .collect()
    }
}

impl SubagentTreePublisher {
    /// Announces the difference between what a tree's subscribers last heard
    /// and `tree`, one change per revision. A tree whose subscribers have all
    /// gone is forgotten instead.
    fn announce(&mut self, top_level: SessionId, tree: SubagentTree) {
        let Some(channel) = self.trees.get_mut(&top_level) else {
            return;
        };
        if channel.updates.receiver_count() == 0 {
            self.trees.remove(&top_level);
            return;
        }
        if channel.announced == tree {
            return;
        }
        for change in tree_changes(&channel.announced, &tree) {
            channel.revision = SubagentTreeRevision(
                channel
                    .revision
                    .0
                    .checked_add(1)
                    .expect("Subagent tree revision space is not exhausted"),
            );
            let _ = channel.updates.send(SubagentTreeUpdate {
                revision: channel.revision,
                change,
            });
        }
        channel.announced = tree;
    }
}

/// What moved between two readings of one tree, in the order a reader applying
/// them needs: a spawner always joins before what it spawns.
fn tree_changes(before: &SubagentTree, after: &SubagentTree) -> Vec<SubagentTreeChange> {
    let mut changes = Vec::new();
    if before.top_level.title != after.top_level.title {
        changes.push(SubagentTreeChange::TopLevelRetitled {
            title: after.top_level.title.clone(),
        });
    }
    let known = before
        .subagents
        .iter()
        .map(|entry| (entry.session_id, entry))
        .collect::<HashMap<_, _>>();
    for entry in &after.subagents {
        let Some(previous) = known.get(&entry.session_id) else {
            changes.push(SubagentTreeChange::SubagentSpawned {
                entry: entry.clone(),
            });
            continue;
        };
        if previous.name != entry.name || previous.title != entry.title {
            changes.push(SubagentTreeChange::SubagentRetitled {
                session_id: entry.session_id,
                name: entry.name.clone(),
                title: entry.title.clone(),
            });
        }
        if previous.status != entry.status || previous.duration_ms != entry.duration_ms {
            changes.push(SubagentTreeChange::SubagentSettled {
                session_id: entry.session_id,
                status: entry.status,
                duration_ms: entry.duration_ms,
            });
        }
    }
    changes
}

/// Whether a committed change could move what a tree says, so a commit that
/// carries none leaves every subscribed tree unread.
pub(super) fn moves_subagent_tree(change: &SessionChange) -> bool {
    matches!(
        change,
        SessionChange::ActivityAdded {
            activity: Activity::Subagent { .. }
        } | SessionChange::SubagentStatusChanged { .. }
            | SessionChange::SubagentDescriptionChanged { .. }
            | SessionChange::TitleChanged { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ActivityStatus;

    fn entry(session_id: SessionId, parent: SessionId, spawn_order: u32) -> SubagentTreeEntry {
        SubagentTreeEntry {
            session_id,
            parent_session_id: parent,
            spawn_order,
            name: "Explore".to_owned(),
            title: "Map the seams".to_owned(),
            status: ActivityStatus::Active,
            duration_ms: None,
        }
    }

    #[test]
    fn a_reading_that_moves_several_things_announces_each_in_tree_order() {
        let top = SessionId::new();
        let child = SessionId::new();
        let grandchild = SessionId::new();
        let before = SubagentTree {
            top_level: SubagentTreeTopLevel {
                session_id: top,
                title: "Delegate".to_owned(),
            },
            subagents: vec![entry(child, top, 0)],
        };
        let mut settled = entry(child, top, 0);
        settled.status = ActivityStatus::Completed;
        settled.duration_ms = Some(12);
        settled.title = "Mapped the seams".to_owned();
        let after = SubagentTree {
            top_level: SubagentTreeTopLevel {
                session_id: top,
                title: "Delegate the mapping".to_owned(),
            },
            subagents: vec![settled, entry(grandchild, child, 0)],
        };

        assert_eq!(
            tree_changes(&before, &after),
            vec![
                SubagentTreeChange::TopLevelRetitled {
                    title: "Delegate the mapping".to_owned(),
                },
                SubagentTreeChange::SubagentRetitled {
                    session_id: child,
                    name: "Explore".to_owned(),
                    title: "Mapped the seams".to_owned(),
                },
                SubagentTreeChange::SubagentSettled {
                    session_id: child,
                    status: ActivityStatus::Completed,
                    duration_ms: Some(12),
                },
                SubagentTreeChange::SubagentSpawned {
                    entry: entry(grandchild, child, 0),
                },
            ]
        );
        assert!(tree_changes(&after, &after).is_empty());
    }
}
