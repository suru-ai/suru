//! Returning trees nothing uses to the deferred state their histories start
//! the process in (ADR 0022), so a Server holds in memory only the histories
//! in use rather than every one opened since it started.
//!
//! A tree is evicted whole, as it hydrates whole, and only once nothing holds
//! any Session of it: no reader subscribed to it or to a tree listing it, no
//! work running or owed in it — a Turn, a Prompt waiting for one, a Watch, a
//! wait on Subagents, a Report waiting to be told or owed to a Sidekick of it
//! — nothing reading it back would move, and everything it owed storage
//! landed. Reading it in again then changes nothing it held: its history is
//! what storage holds, and what this process knew of it beyond its history
//! stays on the record that remains (see `SessionRecord::keep_runtime_of`).
//!
//! A Provider actor outlives the Turns it runs, idle and connected, until
//! something stops it, and a Session whose history waits in storage has none
//! (ADR 0022): it could take no word the actor sent, and could tell it no
//! posture. So a tree's actors are stopped before it is evicted, as a
//! restart stops them, and the conversation resumes from its stored Resume
//! State at the tree's next Turn, under the Settings in force then.

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    pin::Pin,
    time::{Duration, Instant},
};

use tokio::sync::watch;

use crate::{
    protocol::{
        Activity, ActivityStatus, PromptStatus, SessionId, SessionStatus, UnreadableSessionSummary,
    },
    storage::Unsaved,
};

use super::{SessionStore, SessionStoreState, hydration::recovery_moves};

/// How long a tree goes unused before its histories are evicted.
pub(crate) const SESSION_IDLE_EVICTION: Duration = Duration::from_secs(10 * 60);

/// How often the store looks for trees gone unused that long.
pub(crate) const SESSION_EVICTION_INTERVAL: Duration = Duration::from_secs(60);

/// What finishes once every Provider actor a release stopped has stopped.
pub(crate) type Released = Pin<Box<dyn Future<Output = ()> + Send>>;

/// The Provider actors running the Sessions this store holds, as eviction
/// stops those of the trees it evicts.
pub(crate) trait ProviderActors: Send + Sync {
    /// Stops the actor each of `owners` owns, if one runs, taking it out of
    /// reach at once so any later command starts a fresh one; answers with
    /// what finishes once each has stopped. Called under the store's lock, so
    /// no Prompt can be admitted to those Sessions between the store finding
    /// them idle and their actors being taken out of reach: it must not call
    /// back into the store, nor wait.
    fn release(&self, owners: &[SessionId]) -> Released;
}

impl SessionStore {
    /// Every `interval` until `shutdown`, evicts each tree that has gone
    /// unused for `idle`, stopping its Provider actors through `actors` (see
    /// [`Self::evict_idle_trees`]).
    pub(crate) fn evict_idle_sessions(
        &self,
        idle: Duration,
        interval: Duration,
        actors: std::sync::Arc<dyn ProviderActors>,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let sessions = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval.max(Duration::from_millis(1)));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // The first tick is at once, and nothing has been unused for any
            // time yet.
            ticker.tick().await;
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown.changed() => break,
                    _ = ticker.tick() => {}
                }
                sessions.evict_idle_trees(idle, actors.as_ref()).await;
            }
        });
    }

    /// Returns each tree every Session of which has gone unused for `idle`,
    /// and that nothing holds, to the deferred state, answering with the
    /// top-level Sessions heading those evicted. A tree something holds is
    /// marked used now, so it is evicted only once it has gone `idle`
    /// without anything holding it.
    ///
    /// The trees found idle have their Provider actors stopped through
    /// `actors` first, taken out of reach under the store's lock so no work
    /// admitted after can reach one, and waited for with the lock released,
    /// so anything a stopping actor still says lands while the tree is held.
    /// A tree anything used meanwhile stays, its actor stopped as a restart
    /// would have stopped it.
    ///
    /// What the trees owe storage is then saved, under the store's lock, so
    /// nothing can move them between the save and their eviction. A tree
    /// still owing anything — one that could not be encoded — stays, and
    /// while storage has yet to take everything the writer was handed,
    /// nothing is evicted, since reading a tree back would find it short. The
    /// hydration gate is held while trees are judged and evicted, so no tree
    /// is evicted while one is being read in.
    pub(crate) async fn evict_idle_trees(
        &self,
        idle: Duration,
        actors: &dyn ProviderActors,
    ) -> Vec<SessionId> {
        let (candidates, released, judged_at) = {
            let _admission = self.hydration.lock().await;
            let mut state = self
                .state
                .lock()
                .expect("Session store lock is not poisoned");
            if state.deferred.is_none() {
                // Nothing could read an evicted history back.
                return Vec::new();
            }
            let judged_at = Instant::now();
            let candidates = state.idle_trees(idle, judged_at, true);
            if candidates.is_empty() {
                return Vec::new();
            }
            let owners = candidates
                .iter()
                .flatten()
                .filter(|session_id| state.sessions[*session_id].owns_provider_actor())
                .copied()
                .collect::<Vec<_>>();
            (candidates, actors.release(&owners), judged_at)
        };
        released.await;

        let _admission = self.hydration.lock().await;
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        // Only a tree standing exactly as it was judged, and that nothing
        // used since, is evicted.
        let still_idle = state
            .idle_trees(Duration::ZERO, judged_at, false)
            .into_iter()
            .map(|tree| tree.into_iter().collect::<HashSet<_>>())
            .collect::<Vec<_>>();
        let idle_trees = candidates
            .into_iter()
            .filter(|tree| {
                still_idle.contains(&tree.iter().copied().collect())
                    && tree
                        .iter()
                        .all(|session_id| state.sessions[session_id].used_at <= judged_at)
            })
            .collect::<Vec<_>>();
        if idle_trees.is_empty() {
            return Vec::new();
        }
        let members = idle_trees.iter().flatten().copied().collect::<HashSet<_>>();
        let taken = state.take_saves(Some(&members));
        match self.storage.save(taken.saves) {
            Ok(true) => {}
            Ok(false) => {
                tracing::debug!(
                    "storage has yet to take what it was handed, so no tree is evicted"
                );
                return Vec::new();
            }
            Err(error) => {
                tracing::warn!("no tree is evicted while storage cannot be reached: {error}");
                return Vec::new();
            }
        }
        // One that could not be encoded still owes its save, and its history
        // is the only copy of what storage lacks.
        let idle_trees = idle_trees
            .into_iter()
            .filter(|tree| {
                tree.iter()
                    .all(|session_id| !state.sessions[session_id].unsaved.owes_save())
            })
            .collect::<Vec<_>>();
        for tree in &idle_trees {
            state.evict_tree(tree);
        }
        if !idle_trees.is_empty() {
            tracing::debug!(
                trees = idle_trees.len(),
                "evicted Session trees gone unused"
            );
        }
        idle_trees.into_iter().map(|tree| tree[0]).collect()
    }
}

impl SessionStoreState {
    /// Every held tree nothing holds whose Sessions have all gone unused
    /// for `idle` at `now`, each its top-level Session first. Where `mark`
    /// says so, every Session of a tree something holds is marked used now.
    fn idle_trees(&mut self, idle: Duration, now: Instant, mark: bool) -> Vec<Vec<SessionId>> {
        let mut children = HashMap::<SessionId, Vec<SessionId>>::new();
        for (&session_id, record) in &self.sessions {
            if let Some(parent) = record.snapshot.session.parent {
                children.entry(parent).or_default().push(session_id);
            }
        }
        let top_levels = self
            .sessions
            .iter()
            .filter(|(session_id, record)| {
                record.snapshot.session.parent.is_none() && !self.is_deferred(**session_id)
            })
            .map(|(session_id, _)| *session_id)
            .collect::<Vec<_>>();
        let mut idle_trees = Vec::new();
        for top_level in top_levels {
            let tree = tree_of(top_level, &children);
            if self.holds_tree(top_level, &tree) {
                if mark {
                    for session_id in &tree {
                        self.mark_used(*session_id);
                    }
                }
                continue;
            }
            if tree.iter().all(|session_id| {
                now.saturating_duration_since(self.sessions[session_id].used_at) >= idle
            }) {
                idle_trees.push(tree);
            }
        }
        idle_trees
    }
}

/// `top_level` and every Session beneath it, the top-level Session first.
fn tree_of(top_level: SessionId, children: &HashMap<SessionId, Vec<SessionId>>) -> Vec<SessionId> {
    let mut tree = vec![top_level];
    let mut visited = HashSet::from([top_level]);
    let mut visit = 0;
    while visit < tree.len() {
        for &child in children.get(&tree[visit]).into_iter().flatten() {
            if visited.insert(child) {
                tree.push(child);
            }
        }
        visit += 1;
    }
    tree
}

impl SessionStoreState {
    /// Whether anything holds the tree `top_level` heads, whose Sessions are
    /// `tree`, so that it must stay in memory. When in doubt, it holds.
    fn holds_tree(&self, top_level: SessionId, tree: &[SessionId]) -> bool {
        // A reader following this tree, or a Sidekick's tree listing it,
        // reads it as it is held; a tree read from storage draws differently.
        if self
            .trees_listing(top_level)
            .iter()
            .any(|tree| self.subagent_trees.is_watched(*tree))
        {
            return true;
        }
        let members = tree.iter().copied().collect::<HashSet<_>>();
        // Reports owed to a Sidekick of this tree are told to it as it is
        // held, by Sessions here or elsewhere, or by a Remote.
        if self.sessions.values().any(|record| {
            record
                .sidekick_work
                .iter()
                .any(|work| members.contains(&work.sidekick))
        }) || tree
            .iter()
            .any(|session_id| self.remote_reports.owes_sidekick(*session_id))
        {
            return true;
        }
        tree.iter().any(|session_id| {
            let Some(record) = self.sessions.get(session_id) else {
                return true;
            };
            let snapshot = &record.snapshot;
            let session = &snapshot.session;
            self.is_deferred(*session_id)
                // A subscriber reads its updates.
                || record.updates.receiver_count() > 0
                // Work runs, or is owed, somewhere in its subtree.
                || session.working_since.is_some()
                || session.monitoring_since.is_some()
                || session.status != SessionStatus::Idle
                || snapshot.turns.iter().any(|turn| !turn.status.is_terminal())
                || snapshot
                    .prompts
                    .iter()
                    .any(|prompt| prompt.status == PromptStatus::Pending)
                || snapshot.activities.iter().any(|activity| {
                    matches!(
                        activity,
                        Activity::Subagent {
                            status: ActivityStatus::Active,
                            ..
                        }
                    ) || recovery_moves(activity)
                })
                || !snapshot.watches.is_empty()
                || !record.watches.is_empty()
                || snapshot.waiting_on_subagents.is_some()
                || !record.subagent_waits.is_empty()
                || !record.held_reports.is_empty()
                || !record.sidekick_work.is_empty()
                || !record.steer_targets.is_empty()
                || !record.turn_start_admissions.is_empty()
                || record.selection_retry_prompt.is_some()
                || self.resumable_preparations.contains(session_id)
                // Interventions stand open, here or reported from below.
                || !snapshot.pending_approvals.is_empty()
                || !snapshot.submitting_approvals.is_empty()
                || !snapshot.subagent_interventions.is_empty()
                // A posture is still being applied to its Provider.
                || session.approval_posture.as_ref().is_some_and(|posture| {
                    posture.application == crate::protocol::ApprovalPostureApplication::Applying
                })
                // Its Provider may still owe output a Continuation answers.
                || (record.owns_provider_actor()
                    && self.owes_continuation_to_brokered_subagents(*session_id))
        })
    }

    /// Drops the histories of `tree`, whose saves have all landed, leaving
    /// each Session as one whose history waits in storage: its listing, its
    /// Turns, and its derived readings stay, as at startup.
    fn evict_tree(&mut self, tree: &[SessionId]) {
        for &session_id in tree {
            let Some(record) = self.sessions.get_mut(&session_id) else {
                continue;
            };
            let snapshot = &mut record.snapshot;
            snapshot.prompts = Vec::new();
            snapshot.messages = Vec::new();
            snapshot.activities = Vec::new();
            snapshot.transcript = Vec::new();
            snapshot.attachments = Vec::new();
            record.unsaved = Unsaved::unheld();
            let parent = snapshot.session.parent;
            let summary = UnreadableSessionSummary {
                id: session_id,
                title: record.summary.title.clone(),
                created_at: record.summary.created_at,
                updated_at: record.summary.updated_at,
                workspace: Some(record.summary.session.workspace.clone()),
            };
            // Its Prompts' owners stay indexed as this process admitted them:
            // a retry of the request that created the Session or admitted one
            // of them is answered as it would have been before the eviction,
            // once the history it reads is read back in.
            let deferred = self
                .deferred
                .as_mut()
                .expect("a tree is evicted only where storage can read it back");
            deferred.summaries.insert(session_id, summary);
            deferred.parents.insert(session_id, parent);
            if parent.is_some() {
                deferred.child_ids.insert(session_id);
            }
        }
    }
}
