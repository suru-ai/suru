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
    storage::{PersistedSession, StorageError, StorageSink, UnreadableStoredSession, Unsaved},
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
    /// A tree hydrates once each time its history is read in — at its first
    /// access, and again at the first access after it was evicted for going
    /// unused (see [`SessionStore::evict_idle_trees`]) — and this is where
    /// its history catches up with the process that loads it: stranded
    /// Prompts withdraw, stopped Turns settle, and unpinned Approval
    /// Postures follow the current Settings. A tree is evicted only once
    /// none of that would move it, so reading it in again changes nothing
    /// it held. An access to a tree already held marks it used, so it is not
    /// evicted from under the access that just entered.
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
        {
            let mut state = self
                .state
                .lock()
                .expect("Session store lock is not poisoned");
            if !state.is_deferred(session_id) {
                state.mark_used(session_id);
                return Ok(Vec::new());
            }
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
                    let held = state.sessions.remove(&id).expect("existence checked");
                    // Preserve grouping learned without opening this history.
                    persisted.snapshot.session.workspace = held.snapshot.session.workspace.clone();
                    persisted.snapshot.session.checkout = held.snapshot.session.checkout.clone();
                    persisted.snapshot.revision = held.snapshot.revision;
                    // Preserve the derived subtree reading and startup summary.
                    persisted.summary = held.summary.clone();
                    persisted.snapshot.session.working_since = held.snapshot.session.working_since;
                    persisted.snapshot.session.monitoring_since =
                        held.snapshot.session.monitoring_since;
                    persisted.snapshot.watches = held.snapshot.watches.clone();
                    persisted.snapshot.subagent_usage = held.snapshot.subagent_usage;
                    persisted.snapshot.total_cost = held.snapshot.total_cost;
                    persisted.snapshot.own_cost = held.snapshot.own_cost;
                    persisted.snapshot.pending_approvals_revision =
                        held.snapshot.pending_approvals_revision;

                    let (mut record, recovered_activities) =
                        restored_record(persisted, &mut state.prompts);
                    // Storage holds the history as it was read, save for
                    // what recovery moved, which lands with whatever the
                    // Session next owes and never on its own (ADR 0022).
                    record.unsaved =
                        Unsaved::hydrated(&record.snapshot, recovered_activities.iter().copied());
                    record.keep_runtime_of(held);
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
        // A Prompt owed a Turn is owed it by the process that admitted it, and
        // this history has just outlived that process. Withdraw what nothing
        // is left to deliver, in the same lock that first made it readable, so
        // no reader ever sees it as a Message still waiting on an Agent
        // (ADR 0024).
        state.withdraw_stranded_prompts(&self.storage, hydrated.clone());
        state.settle_stopped_turns(&self.storage, hydrated.clone());
        // The Settings that shaped a stored unpinned posture may not be the
        // ones in force now; the history catches up here, each time it is
        // read in, and nothing is owed to a Provider because a deferred
        // Session never has one: none survives a restart, and eviction stops
        // a tree's actors before it lets the tree go.
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
        let (known, repository) = {
            let state = self
                .state
                .lock()
                .expect("Session store lock is not poisoned");
            (
                state.prompts.get(&prompt_id).map(|owner| owner.session_id),
                state
                    .deferred
                    .as_ref()
                    .map(|deferred| deferred.repository.clone()),
            )
        };
        // An owner this process admitted stays indexed while its history is
        // evicted, and the conflict and retry rules read that history.
        if let Some(owner) = known {
            return self.hydrate(owner).await;
        }
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
///
/// Answers with the record and the positions of the Activities its recovery
/// moved, which storage still holds as they were. The record owes storage
/// nothing until it moves, and its first save then writes it whole, since
/// what storage holds of it is not known here; hydration, which does know,
/// says so instead.
pub(super) fn restored_record(
    mut persisted: PersistedSession,
    prompts: &mut std::collections::HashMap<PromptId, PromptOwner>,
) -> (SessionRecord, Vec<usize>) {
    let recovered_activities = recover_interventions(&mut persisted);
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
    // An owner already indexed is the one this process admitted the Prompt
    // with, kept while the history was evicted, and knows more of how it
    // was admitted than the history does.
    for prompt in &persisted.snapshot.prompts {
        prompts.entry(prompt.id).or_insert_with(|| PromptOwner {
            session_id: persisted.snapshot.session.id,
            text: prompt.text.clone(),
            skill_invocations: prompt.skill_invocations.clone(),
            attachments: prompt.attachments.clone(),
            agent_selection: None,
            author: prompt.author.clone(),
            origin: PromptOrigin::Admission(prompt.delivery),
        });
    }
    let (updates, _) = tokio::sync::broadcast::channel(SESSION_UPDATE_CAPACITY);
    let record = SessionRecord {
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
        unsaved: Unsaved::handed_over(),
        used_at: std::time::Instant::now(),
        sidekick_work: Vec::new(),
    };
    (record, recovered_activities)
}

impl SessionRecord {
    /// Takes over what `held`, the record this one's history was read in
    /// over, kept of this process's own dealings with the Session, which no
    /// history holds: the operations it already answered, how far its Prompt
    /// admissions reached, the interrupts that stood its work down, and its
    /// subscribers' channel. A record read in for the first time takes over
    /// only defaults; one read in again after its tree was evicted for going
    /// unused takes over everything as it stood, so reading the history in
    /// again changes nothing this process knew of it. Everything a tree can
    /// be evicted holding is carried here; what it cannot is empty in both.
    fn keep_runtime_of(&mut self, held: SessionRecord) {
        let SessionRecord {
            context_fill_order,
            snapshot: _,
            summary: _,
            updates,
            next_prompt_order,
            steer_targets,
            turn_start_admissions,
            selection_operations,
            viewed_operations,
            selection_retry_prompt,
            resume_states: _,
            subagent_identity: _,
            brokered: _,
            watches,
            subagent_waits,
            work_interrupted_at,
            stopped_by_ancestor,
            held_reports,
            unsaved: _,
            used_at: _,
            sidekick_work,
        } = held;
        self.context_fill_order = context_fill_order;
        self.updates = updates;
        self.next_prompt_order = self.next_prompt_order.max(next_prompt_order);
        self.steer_targets = steer_targets;
        self.turn_start_admissions = turn_start_admissions;
        self.selection_operations = selection_operations;
        self.viewed_operations = viewed_operations;
        self.selection_retry_prompt = selection_retry_prompt;
        self.watches = watches;
        self.subagent_waits = subagent_waits;
        self.work_interrupted_at = work_interrupted_at;
        self.stopped_by_ancestor = stopped_by_ancestor;
        self.held_reports = held_reports;
        self.sidekick_work = sidekick_work;
    }
}

/// Reads every Approval and Questionnaire the last process left waiting as
/// abandoned: the process that could have answered is gone. The same reading a
/// stopping server writes for the Turn it settles ([`OpenInterventions`]).
/// Answers with the positions of those it moved.
fn recover_interventions(persisted: &mut PersistedSession) -> Vec<usize> {
    let mut recovered = Vec::new();
    for (position, activity) in persisted.snapshot.activities.iter_mut().enumerate() {
        let moved = match activity {
            Activity::Approval { outcome, .. } => {
                let abandoned = OpenInterventions::Abandoned.approval_outcome(*outcome);
                std::mem::replace(outcome, abandoned) != abandoned
            }
            Activity::Questionnaire { outcome, .. } => {
                let abandoned = OpenInterventions::Abandoned.questionnaire_outcome(*outcome);
                std::mem::replace(outcome, abandoned) != abandoned
            }
            _ => false,
        };
        if moved {
            recovered.push(position);
        }
    }
    recovered
}

/// Whether reading `activity` back from storage would move it, as
/// [`recover_interventions`] reads an Approval or Questionnaire still waiting.
pub(super) fn recovery_moves(activity: &Activity) -> bool {
    match activity {
        Activity::Approval { outcome, .. } => {
            OpenInterventions::Abandoned.approval_outcome(*outcome) != *outcome
        }
        Activity::Questionnaire { outcome, .. } => {
            OpenInterventions::Abandoned.questionnaire_outcome(*outcome) != *outcome
        }
        _ => false,
    }
}
