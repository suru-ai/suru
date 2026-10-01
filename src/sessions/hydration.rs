//! The shared admission boundary between durable metadata and mutable histories.

use std::collections::HashSet;

use super::{
    SESSION_UPDATE_CAPACITY, SessionRecord, SessionStore, SessionStoreState,
    prompts::{PromptOrigin, PromptOwner},
};
use crate::{
    protocol::{
        Activity, ActivityStatus, PromptId, PromptOrder, SessionCatalogChange, SessionChange,
        SessionId,
    },
    storage::{PersistedSession, StorageError, StorageSink, UnreadableStoredSession},
};

use super::settlement::{OpenInterventions, TrailingCommandOutput, fail_turn_changes};

/// Why a Subagent's Turn found open at start failed, as its Transcript says it.
pub(super) const STOPPED_SUBAGENT_MESSAGE: &str =
    "The server stopped before this Subagent finished.";
/// Why any other Turn found open at start failed.
pub(super) const STOPPED_TURN_MESSAGE: &str = "The server stopped before this Turn finished.";

impl SessionStore {
    /// Restore the entire containing tree before exposing history or admitting
    /// work: reconciliation can mutate any ancestor or descendant. Waiting for
    /// another reader never occupies a blocking thread or the store lock.
    ///
    /// A tree hydrates exactly once, and this is where its history catches up
    /// with the process that loads it: stranded Prompts withdraw, stopped
    /// Turns settle, and unpinned Approval Postures follow the current
    /// Settings.
    pub(crate) async fn hydrate(&self, session_id: SessionId) -> Result<(), StorageError> {
        // A Sidekick's Session read back has its rows put right against the
        // Subsessions it began, which may have moved on without it — once the
        // tree is installed and its admission released, since that can read a
        // Subsession back too.
        for top_level in self.hydrate_tree(session_id).await? {
            self.reconcile_subsession_rows(top_level).await;
        }
        Ok(())
    }

    /// Reads the tree `session_id` belongs to back from storage and installs
    /// it, answering the top-level Sessions it installed: none where the tree
    /// was held already.
    async fn hydrate_tree(&self, session_id: SessionId) -> Result<Vec<SessionId>, StorageError> {
        // Active trees need no admission gate and never wait behind another
        // tree's first read. Recheck after the gate to coalesce waiting readers.
        if !self
            .state
            .lock()
            .expect("Session store lock is not poisoned")
            .is_deferred(session_id)
        {
            return Ok(Vec::new());
        }
        let _admission = self.hydration.lock().await;
        let (repository, root, pending) = {
            let state = self
                .state
                .lock()
                .expect("Session store lock is not poisoned");
            if !state.is_deferred(session_id) {
                return Ok(Vec::new());
            }
            let Some(deferred) = &state.deferred else {
                return Ok(Vec::new());
            };
            let mut root = session_id;
            let mut visited = std::collections::HashSet::new();
            while visited.insert(root) {
                let Some(Some(parent)) = deferred.parents.get(&root) else {
                    break;
                };
                root = *parent;
            }
            let mut children = std::collections::HashMap::<SessionId, Vec<SessionId>>::new();
            for (&id, &parent) in &deferred.parents {
                if let Some(parent) = parent {
                    children.entry(parent).or_default().push(id);
                }
            }
            let mut tree = vec![root];
            let mut visit = 0;
            visited.clear();
            visited.insert(root);
            while visit < tree.len() {
                let parent = tree[visit];
                for &child in children.get(&parent).into_iter().flatten() {
                    if visited.insert(child) {
                        tree.push(child);
                    }
                }
                visit += 1;
            }
            let pending = tree
                .into_iter()
                .filter(|id| deferred.summaries.contains_key(id))
                .collect::<Vec<_>>();
            (deferred.repository.clone(), root, pending)
        };
        // Read without the state lock. Deletion can finish during I/O; the
        // existence check below is the linearization point for installation.
        let mut histories = Vec::with_capacity(pending.len());
        for id in pending {
            let loaded = repository.session(id).await;
            if let Err(error) = &loaded
                && !matches!(error, StorageError::InvalidSession { .. })
            {
                return loaded.map(|_| Vec::new());
            }
            histories.push((id, loaded));
        }
        // Install a tree atomically. Cancellation or an I/O failure before
        // here leaves every record deferred, ready for the next access to retry.
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let mut invalidated = false;
        let mut hydrated = Vec::new();
        for (id, loaded) in histories {
            if !state.is_deferred(id) || !state.sessions.contains_key(&id) {
                continue;
            }
            // Its Subagent rows are its history's to give from here on.
            state.stored_subagent_rows.remove(&id);
            match loaded {
                Ok(Some(mut persisted)) => {
                    let record = state.sessions.get(&id).expect("existence checked");
                    // Preserve grouping learned without opening this history.
                    persisted.snapshot.session.workspace =
                        record.snapshot.session.workspace.clone();
                    persisted.snapshot.session.checkout = record.snapshot.session.checkout.clone();
                    persisted.snapshot.revision = record.snapshot.revision;
                    // Preserve the derived subtree reading and startup summary.
                    persisted.summary = record.summary.clone();
                    persisted.snapshot.session.working_since =
                        record.snapshot.session.working_since;
                    persisted.snapshot.session.monitoring_since =
                        record.snapshot.session.monitoring_since;
                    persisted.snapshot.watches = record.snapshot.watches.clone();
                    persisted.snapshot.subagent_usage = record.snapshot.subagent_usage;
                    persisted.snapshot.total_cost = record.snapshot.total_cost;
                    persisted.snapshot.own_cost = record.snapshot.own_cost;

                    let record = restored_record(persisted, &mut state.prompts);
                    state.sessions.insert(id, record);
                    hydrated.push(id);
                    state
                        .deferred
                        .as_mut()
                        .expect("deferred repository exists")
                        .summaries
                        .remove(&id);
                }
                Ok(None) | Err(StorageError::InvalidSession { .. }) => {
                    let reason = loaded
                        .as_ref()
                        .err()
                        .map_or_else(|| "missing from storage".to_owned(), ToString::to_string);
                    tracing::warn!(session_id = %id, %reason, "invalidating unreadable persisted Session");
                    let mut unreadable = state
                        .deferred
                        .as_mut()
                        .expect("deferred repository exists")
                        .summaries
                        .remove(&id)
                        .expect("deferred Session exists");
                    let record = state.sessions.remove(&id).expect("deferred record exists");
                    unreadable.workspace = Some(record.summary.session.workspace.clone());
                    state.unreadable_sessions.insert(
                        id,
                        UnreadableStoredSession {
                            summary: unreadable,
                            checkout: record.summary.session.checkout.clone(),
                            execution_directory: Some(
                                record.summary.session.execution_directory.clone(),
                            ),
                        },
                    );
                    invalidated = true;
                }
                Err(error) => return Err(error),
            }
        }
        if invalidated {
            state.restore_projections();
            if state
                .sessions
                .get(&root)
                .is_some_and(|record| !record.snapshot.session.is_subagent())
                || state.unreadable_sessions.contains_key(&root) && !state.is_stored_child(root)
            {
                state
                    .publish_catalog_change(SessionCatalogChange::Invalidated { session_id: root });
            }
        }
        // Queue complete recovered snapshots, including corrected subtree
        // readings, before releasing the lock that admits any mutation.
        for &id in &hydrated {
            let record = &state.sessions[&id];
            self.storage.hydrated(PersistedSession {
                summary: record.summary.clone(),
                snapshot: record.snapshot.clone(),
                resume_states: record.resume_states.clone(),
                subagent_identity: record.subagent_identity.clone(),
                brokered: record.brokered,
            })?;
        }
        // A Prompt owed a Turn is owed it by the process that admitted it, and
        // this history has just outlived that process. Withdraw what nothing
        // is left to deliver, in the same lock that first made it readable, so
        // no reader ever sees it as a Message still waiting on an Agent
        // (ADR 0024).
        state.withdraw_stranded_prompts(&self.storage, hydrated.clone());
        state.settle_stopped_turns(&self.storage, hydrated.clone());
        // The Settings that shaped a stored unpinned posture may not be the
        // ones this process opened with; the history catches up here, once,
        // and nothing is owed to a Provider because a deferred Session never
        // has one.
        state.adopt_hydrated_postures(&self.storage, &hydrated, &self.settings.borrow().settings);
        Ok(hydrated
            .into_iter()
            .filter(|id| {
                state
                    .sessions
                    .get(id)
                    .is_some_and(|record| !record.snapshot.session.is_subagent())
            })
            .collect())
    }

    /// Prompt IDs are globally unique, including Prompts in deferred Sessions.
    /// Locate their owner without reading any Prompt content, then hydrate only
    /// that tree before the existing conflict/retry rules inspect the Prompt.
    pub(crate) async fn hydrate_prompt_owner(
        &self,
        prompt_id: PromptId,
    ) -> Result<(), StorageError> {
        let repository = {
            let state = self
                .state
                .lock()
                .expect("Session store lock is not poisoned");
            if state.prompts.contains_key(&prompt_id) {
                return Ok(());
            }
            state
                .deferred
                .as_ref()
                .map(|deferred| deferred.repository.clone())
        };
        if let Some(repository) = repository
            && let Some(id) = repository.prompt_session(prompt_id).await?
        {
            self.hydrate(id).await?;
            // A malformed owner's identity must still prevent re-use from
            // replacing its durable Prompt row in another Session.
            if !self.knows_prompt(prompt_id) {
                let state = self
                    .state
                    .lock()
                    .expect("Session store lock is not poisoned");
                if state.unreadable_sessions.contains_key(&id) {
                    return Err(StorageError::InvalidSession {
                        session_id: id.to_string(),
                        message: "Prompt belongs to an unreadable Session".to_owned(),
                    });
                }
            }
        }
        Ok(())
    }
}

impl SessionStoreState {
    pub(super) fn is_deferred(&self, id: SessionId) -> bool {
        self.deferred
            .as_ref()
            .is_some_and(|deferred| deferred.summaries.contains_key(&id))
    }

    pub(super) fn is_stored_child(&self, id: SessionId) -> bool {
        self.deferred
            .as_ref()
            .is_some_and(|deferred| deferred.child_ids.contains(&id))
    }

    /// Settles every Turn persisted still open. A Turn is run by the process
    /// that began it, and this history has outlived that process: no Provider
    /// survives a stop, so nothing is left to finish the Turn, and reading it
    /// as live would keep its whole ancestry Working forever. It fails the way
    /// a lost Provider connection fails it, at the moment it last showed work,
    /// and a Subagent's open row settles with it (ADR 0029) — in its parent,
    /// and wherever a resume added one: a sibling Subagent that sent the
    /// resume holds that row in its own Transcript (ADR 0031).
    ///
    /// Deferred Sessions are passed over: hydrating one is what brings it
    /// here. A parent still deferred keeps its row until its own hydration.
    pub(super) fn settle_stopped_turns(
        &mut self,
        storage: &StorageSink,
        sessions: impl IntoIterator<Item = SessionId>,
    ) {
        let sessions = sessions.into_iter().collect::<Vec<_>>();
        let mut stopped_subagents = HashSet::new();
        let mut parents = Vec::new();
        for &session_id in &sessions {
            if self.is_deferred(session_id) {
                continue;
            }
            let Some(record) = self.sessions.get(&session_id) else {
                continue;
            };
            let subagent = record.snapshot.session.is_subagent();
            let parent = record.snapshot.session.parent;
            let open = record
                .snapshot
                .turns
                .iter()
                .filter(|turn| !turn.status.is_terminal())
                .map(|turn| (turn.id, turn.last_output_at.or(turn.started_at)))
                .collect::<Vec<_>>();
            if open.is_empty() {
                continue;
            }
            for (turn_id, settled_at) in open {
                let Some(record) = self.sessions.get(&session_id) else {
                    break;
                };
                let message = if subagent {
                    STOPPED_SUBAGENT_MESSAGE
                } else {
                    STOPPED_TURN_MESSAGE
                };
                let changes = fail_turn_changes(
                    &record.snapshot,
                    turn_id,
                    TrailingCommandOutput::new(),
                    message.to_owned(),
                    settled_at,
                    OpenInterventions::Abandoned,
                );
                if let Err(error) = self.commit(storage, session_id, changes) {
                    tracing::warn!(
                        session_id = %session_id,
                        "Turn left open by a stop could not be settled: {error}"
                    );
                }
            }
            let Some(parent) = parent else {
                continue;
            };
            stopped_subagents.insert(session_id);
            parents.push(parent);
        }
        if stopped_subagents.is_empty() {
            return;
        }
        let mut visited = HashSet::new();
        for holder in sessions.into_iter().chain(parents) {
            if visited.insert(holder) {
                self.settle_stopped_subagent_rows(storage, holder, &stopped_subagents);
            }
        }
    }

    /// Settles every row `holder` holds for one of the `stopped` Subagents, if
    /// it still stands open, the way a lost Provider connection would have:
    /// Failed, with no duration, since the Provider never reported the
    /// Subagent's stretch settling.
    fn settle_stopped_subagent_rows(
        &mut self,
        storage: &StorageSink,
        holder: SessionId,
        stopped: &HashSet<SessionId>,
    ) {
        if self.is_deferred(holder) {
            return;
        }
        let Some(record) = self.sessions.get(&holder) else {
            return;
        };
        let rows = record
            .snapshot
            .activities
            .iter()
            .filter_map(|activity| match activity {
                Activity::Subagent {
                    id,
                    status: ActivityStatus::Active,
                    session_id,
                    ..
                } if stopped.contains(session_id) => Some(SessionChange::SubagentStatusChanged {
                    activity_id: *id,
                    status: ActivityStatus::Failed,
                    duration_ms: None,
                }),
                _ => None,
            })
            .collect::<Vec<_>>();
        if rows.is_empty() {
            return;
        }
        if let Err(error) = self.commit(storage, holder, rows) {
            tracing::warn!(
                session_id = %holder,
                "Subagent row left open by a stop could not be settled: {error}"
            );
        }
    }
}

/// Both eager in-memory fixtures and durable hydration use the same recovery
/// rules for Prompt identity/order and historical Questionnaire availability.
pub(super) fn restored_record(
    mut persisted: PersistedSession,
    prompts: &mut std::collections::HashMap<PromptId, PromptOwner>,
) -> SessionRecord {
    recover_interventions(&mut persisted);
    let next_prompt_order = persisted
        .snapshot
        .prompts
        .iter()
        .map(|prompt| prompt.admission_order.0)
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .map(PromptOrder)
        .expect("persisted Prompt admission order space is not exhausted");
    for prompt in &persisted.snapshot.prompts {
        prompts.insert(
            prompt.id,
            PromptOwner {
                session_id: persisted.snapshot.session.id,
                text: prompt.text.clone(),
                skill_invocations: prompt.skill_invocations.clone(),
                attachments: prompt.attachments.clone(),
                agent_selection: None,
                author: prompt.author.clone(),
                origin: PromptOrigin::Admission(prompt.delivery),
            },
        );
    }
    let (updates, _) = tokio::sync::broadcast::channel(SESSION_UPDATE_CAPACITY);
    SessionRecord {
        context_fill_order: None,
        snapshot: persisted.snapshot,
        summary: persisted.summary,
        resume_states: persisted.resume_states,
        subagent_identity: persisted.subagent_identity,
        // A brokered Subagent's Session keeps the actor of its own it was
        // spawned with, so a restart routes its Provider work as before.
        brokered: persisted.brokered,
        updates,
        next_prompt_order,
        steer_targets: Default::default(),
        turn_start_admissions: Default::default(),
        selection_operations: Default::default(),
        viewed_operations: Default::default(),
        selection_retry_prompt: None,
        watches: Default::default(),
        subagent_waits: Vec::new(),
        work_interrupted_at: None,
        stopped_by_ancestor: None,
        held_reports: Default::default(),
        acts_to_store: Vec::new(),
        sidekicks_owed: Vec::new(),
    }
}

/// Reads every Approval and Questionnaire the last process left waiting as
/// abandoned: the process that could have answered is gone. The same reading a
/// stopping server writes for the Turn it settles ([`OpenInterventions`]).
fn recover_interventions(persisted: &mut PersistedSession) {
    for activity in &mut persisted.snapshot.activities {
        match activity {
            Activity::Approval { outcome, .. } => {
                *outcome = OpenInterventions::Abandoned.approval_outcome(*outcome);
            }
            Activity::Questionnaire { outcome, .. } => {
                *outcome = OpenInterventions::Abandoned.questionnaire_outcome(*outcome);
            }
            _ => {}
        }
    }
}
