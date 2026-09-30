//! How a batch of changes becomes the Session's next snapshot: the commit that
//! projects, persists, and broadcasts one revision, plus the invariants the
//! store reads back off a snapshot.

use anyhow::anyhow;

use std::collections::HashMap;

use crate::protocol::{
    Cost, CostCoverage, CostRecord, CostTotal, Prompt, PromptId, PromptOrder, PromptStatus,
    SessionCatalogChange, SessionChange, SessionId, SessionRevision, SessionSnapshot,
    SessionStandingInputs, SessionStatus, SessionTimestamp, SessionUpdate, Turn, TurnId,
    TurnStatus, UsageTotal,
};
use crate::session_projection::apply_update;
use crate::storage::StorageSink;

use super::{SessionRecord, SessionStore, SessionStoreState};

impl SessionStore {
    /// Commits `changes` to the Session as one revision, without the Agent
    /// output gate [`SessionStore::publish_agent_output_changes`] applies.
    pub(crate) fn publish(
        &self,
        session_id: SessionId,
        changes: Vec<SessionChange>,
    ) -> anyhow::Result<SessionUpdate> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        state.commit(&self.storage, session_id, changes)
    }
}

impl SessionStoreState {
    /// Commits `changes` to one Session and then re-derives the Working and
    /// Monitoring readings above it. Every commit goes through here rather
    /// than reaching [`SessionRecord::commit`] directly, because a commit
    /// anywhere in a Subagent subtree — a child's Turn settling, a spawn
    /// opening one — can flip what the listed root's row says about live work,
    /// and only the state can see across Sessions.
    pub(super) fn commit(
        &mut self,
        storage: &StorageSink,
        session_id: SessionId,
        changes: Vec<SessionChange>,
    ) -> anyhow::Result<SessionUpdate> {
        self.commit_admission(storage, session_id, changes, None)
    }

    /// Commits a batch that admits `turn_start` to begin a Turn of its own.
    /// The admission moment is the commit's own, because that is when the
    /// Session began Working: the Prompt is owed a Turn from here until it is
    /// delivered, fails, or is withdrawn, whatever the Provider is doing.
    ///
    /// The admission is only recorded by a commit that lands. A batch that
    /// fails changes nothing about the Session, so it must leave nothing
    /// behind that would have it read as Working over a Prompt it never
    /// admitted.
    pub(super) fn commit_admission(
        &mut self,
        storage: &StorageSink,
        session_id: SessionId,
        changes: Vec<SessionChange>,
        turn_start: Option<PromptId>,
    ) -> anyhow::Result<SessionUpdate> {
        let committed = self.commit_admitted(storage, session_id, changes, turn_start);
        if committed.is_err()
            && let Some(prompt_id) = turn_start
            && let Some(record) = self.sessions.get_mut(&session_id)
        {
            record.turn_start_admissions.remove(&prompt_id);
        }
        committed
    }

    fn commit_admitted(
        &mut self,
        storage: &StorageSink,
        session_id: SessionId,
        mut changes: Vec<SessionChange>,
        turn_start: Option<PromptId>,
    ) -> anyhow::Result<SessionUpdate> {
        if self.is_deferred(session_id) {
            return Err(anyhow!("Session history must be hydrated before mutation"));
        }
        let tree_working = self.subscribed_tree_working(session_id);
        let updated_at = self.next_timestamp();
        if let Some(prompt_id) = turn_start
            && let Some(record) = self.sessions.get_mut(&session_id)
        {
            record.turn_start_admissions.insert(prompt_id, updated_at);
        }
        // Working and Monitoring are server derivations, and a wait on
        // Subagents is the Broker's to say. Callers can describe the Turn
        // transition that changes them, but cannot inject a competing clock.
        changes.retain(|change| {
            !matches!(
                change,
                SessionChange::SessionWorkingChanged { .. }
                    | SessionChange::SessionMonitoringChanged { .. }
                    | SessionChange::SessionWatchesChanged { .. }
                    | SessionChange::SessionWaitingOnSubagentsChanged { .. }
            )
        });
        let repaired = super::brokered::repaired_settlements(&changes);
        stamp_turn_timing(&mut changes, updated_at);
        stamp_cost_measurements(&mut changes, updated_at);
        if let Some(snapshot) = self
            .sessions
            .get(&session_id)
            .map(|record| &record.snapshot)
        {
            stamp_output_evidence(&mut changes, snapshot, updated_at);
        }
        let turn_settled = changes.iter().any(|change| match change {
            SessionChange::TurnAdded { turn } => turn.status.is_terminal(),
            SessionChange::TurnStatusChanged { status, .. } => status.is_terminal(),
            _ => false,
        });
        let projected = self
            .sessions
            .get(&session_id)
            .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?
            .project(session_id, &changes)?;
        let Liveness {
            working_since,
            monitoring_since,
        } = self.subtree_liveness_with(session_id, Some(&projected));
        let working_changed = projected.session.working_since != working_since;
        if working_changed {
            changes.push(SessionChange::SessionWorkingChanged { working_since });
        }
        // A Turn settling with a Watch still live begins Monitoring, and a
        // Continuation beginning ends it, in the revision that moved Working.
        let monitoring_changed = projected.session.monitoring_since != monitoring_since;
        if monitoring_changed {
            changes.push(SessionChange::SessionMonitoringChanged { monitoring_since });
        }
        let record = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?;
        let previous_standing = record.summary.standing_inputs.clone();
        let update = record.commit(storage, session_id, changes, updated_at)?;
        let standing_inputs = record.summary.standing_inputs.clone();
        // Whether this Session's own Interventions came or went, which its
        // entry in a subscribed tree says.
        let own_interventions_moved = previous_standing.pending_questionnaires
            != standing_inputs.pending_questionnaires
            || previous_standing.pending_approvals != standing_inputs.pending_approvals;
        if (turn_settled
            || previous_standing.pending_questionnaires != standing_inputs.pending_questionnaires
            || previous_standing.submitting_questionnaires
                != standing_inputs.submitting_questionnaires
            || previous_standing.pending_approvals != standing_inputs.pending_approvals
            || previous_standing.submitting_approvals != standing_inputs.submitting_approvals)
            && self.ancestry(session_id).announces(session_id)
        {
            self.publish_catalog_change(SessionCatalogChange::StandingInputsChanged {
                session_id,
                inputs: standing_inputs,
            });
        }
        if working_changed && self.ancestry(session_id).announces(session_id) {
            self.publish_catalog_change(SessionCatalogChange::WorkingChanged {
                session_id,
                working_since,
            });
        }
        if monitoring_changed && self.ancestry(session_id).announces(session_id) {
            self.publish_catalog_change(SessionCatalogChange::MonitoringChanged {
                session_id,
                monitoring_since,
            });
        }
        self.forget_spent_admissions(session_id, working_since);
        self.reconcile_liveness(storage, session_id);
        self.reconcile_usage(storage, session_id);
        self.reconcile_interventions(storage, session_id);
        if own_interventions_moved
            || self.moved_tree_working(tree_working)
            || update
                .changes
                .iter()
                .any(super::subagent_tree::moves_subagent_tree)
        {
            self.announce_subagent_tree(session_id);
        }
        // A brokered Subagent's row follows its Turn, in the Session whose
        // Agent delegated that Turn, once this commit has landed.
        self.follow_brokered_turns(storage, session_id, &update.changes, &repaired);
        Ok(update)
    }

    /// Drops the admissions that can no longer move this Session's Working
    /// reading. An admission anchors Working from the moment its Prompt was
    /// admitted until the Turn it began has settled — and once nothing in the
    /// subtree is Working at all, every interval it holds is closed and behind
    /// the present, so no later Turn can merge with one. A Prompt withdrawn or
    /// cancelled where it stood is spent the moment it is, because no Turn
    /// will ever reference it. Keeping the rest would leave a Session's
    /// bookkeeping growing with every Turn it has ever run, and make the
    /// derivation walk it (ADR 0024).
    fn forget_spent_admissions(
        &mut self,
        session_id: SessionId,
        working_since: Option<SessionTimestamp>,
    ) {
        let Some(record) = self.sessions.get_mut(&session_id) else {
            return;
        };
        if working_since.is_none() {
            record.turn_start_admissions.clear();
            return;
        }
        let cancelled = record
            .snapshot
            .prompts
            .iter()
            .filter(|prompt| prompt.status == PromptStatus::Cancelled)
            .map(|prompt| prompt.id)
            .collect::<Vec<_>>();
        for prompt_id in cancelled {
            record.turn_start_admissions.remove(&prompt_id);
        }
    }

    /// Carries descendant availability to each ancestor without copying child
    /// Transcript content or making a child a catalog entry.
    fn reconcile_interventions(&mut self, storage: &StorageSink, session_id: SessionId) {
        let ancestry = self.ancestry(session_id);
        for current in &ancestry.sessions {
            let mut reading = Vec::new();
            for child in self.subtree(*current).into_iter().skip(1) {
                let child_record = &self.sessions[&child];
                let questionnaire_revision = child_record
                    .summary
                    .standing_inputs
                    .pending_questionnaires_revision;
                let approval_revision = child_record
                    .summary
                    .standing_inputs
                    .pending_approvals_revision;
                if questionnaire_revision.0 == 0 && approval_revision.0 == 0 {
                    continue;
                }
                let mut via = child;
                while let Some(parent) = self.sessions[&via].snapshot.session.parent {
                    if parent == *current {
                        break;
                    }
                    via = parent;
                }
                reading.push(crate::protocol::SubagentInterventions {
                    session_id: child,
                    via_session_id: via,
                    revision: std::cmp::max(questionnaire_revision, approval_revision),
                    submitting_questionnaires: child_record
                        .summary
                        .standing_inputs
                        .submitting_questionnaires
                        .clone(),
                    pending_questionnaires: child_record
                        .summary
                        .standing_inputs
                        .pending_questionnaires
                        .clone(),
                    submitting_approvals: child_record
                        .summary
                        .standing_inputs
                        .submitting_approvals
                        .clone(),
                    pending_approvals: child_record
                        .summary
                        .standing_inputs
                        .pending_approvals
                        .clone(),
                });
            }
            reading.sort_by_key(|entry| entry.session_id.as_uuid());
            let Some(record) = self.sessions.get_mut(current) else {
                continue;
            };
            if record.snapshot.subagent_interventions == reading {
                continue;
            }
            if let Err(error) = record.commit_derived(
                storage,
                *current,
                vec![SessionChange::SubagentInterventionsChanged {
                    subagent_interventions: reading,
                }],
            ) {
                tracing::warn!(session_id = %current, "Subagent intervention availability did not roll up: {error}");
                continue;
            }
            if ancestry.announces(*current) {
                let inputs = record.summary.standing_inputs.clone();
                self.publish_catalog_change(SessionCatalogChange::StandingInputsChanged {
                    session_id: *current,
                    inputs,
                });
            }
        }
    }

    /// Re-derives [`crate::protocol::Session::working_since`] and
    /// [`crate::protocol::Session::monitoring_since`] for the Session and
    /// every ancestor up to its listed root, and announces the root's readings
    /// where they flipped. Each Session carries its whole subtree's reading —
    /// the latest Turn, or any working Subagent below — so a listing keeps
    /// saying Working while Subagents outlive the Turn that spawned them (ADR
    /// 0015), and Monitoring while a Watch anywhere below is live and nothing
    /// is Working (ADR 0030). Only the root announces, because a Subagent's
    /// child Session rides no catalog stream.
    ///
    /// A commit calls this for the Session it landed in; a Watch starting or
    /// settling commits nothing, so the Watch table calls it directly. The
    /// Watches live below each Session ride the same revision as its
    /// Monitoring reading, so a reader told the Session is Monitoring is told
    /// what it is waiting on at once.
    pub(super) fn reconcile_liveness(&mut self, storage: &StorageSink, session_id: SessionId) {
        let ancestry = self.ancestry(session_id);
        for current in &ancestry.sessions {
            let reading = self.subtree_liveness_with(*current, None);
            let watches = self.subtree_watches(*current);
            let Some(record) = self.sessions.get_mut(current) else {
                continue;
            };
            let working_changed = record.snapshot.session.working_since != reading.working_since;
            let monitoring_changed =
                record.snapshot.session.monitoring_since != reading.monitoring_since;
            let mut changes = Vec::new();
            if working_changed {
                changes.push(SessionChange::SessionWorkingChanged {
                    working_since: reading.working_since,
                });
            }
            if monitoring_changed {
                changes.push(SessionChange::SessionMonitoringChanged {
                    monitoring_since: reading.monitoring_since,
                });
            }
            if record.snapshot.watches != watches {
                changes.push(SessionChange::SessionWatchesChanged { watches });
            }
            if changes.is_empty() {
                continue;
            }
            if let Err(error) = record.commit_derived(storage, *current, changes) {
                tracing::warn!(session_id = %current, "Working and Monitoring did not roll up: {error}");
            }
            if !ancestry.announces(*current) {
                continue;
            }
            if working_changed {
                self.publish_catalog_change(SessionCatalogChange::WorkingChanged {
                    session_id: *current,
                    working_since: reading.working_since,
                });
            }
            if monitoring_changed {
                self.publish_catalog_change(SessionCatalogChange::MonitoringChanged {
                    session_id: *current,
                    monitoring_since: reading.monitoring_since,
                });
            }
        }
    }

    /// Re-derives what every Session from this one up to its listed root has
    /// consumed, and announces the root's total when it moved, mirroring
    /// [`Self::reconcile_working`]. A Session's own Turns are its own
    /// business, but the total a surface states for it carries its whole
    /// Subagent subtree — and a child's Usage lands in the child's Turns,
    /// where only the state can see it — so each ancestor is told what its
    /// subtree consumed as a change on its own stream.
    pub(super) fn reconcile_usage(&mut self, storage: &StorageSink, session_id: SessionId) {
        let ancestry = self.ancestry(session_id);
        for current in &ancestry.sessions {
            let delegated = self.subagent_usage(*current);
            let total_cost = self.cost_total(*current);
            let own_cost = self.own_cost(*current);
            let Some(record) = self.sessions.get_mut(current) else {
                continue;
            };
            let mut changes = Vec::new();
            if record.snapshot.subagent_usage != delegated {
                // The roll-up is the server's own derivation rather than
                // Agent output, so it goes straight to the record's commit:
                // the gate output passes through has no Turn to check it
                // against. A commit that cannot land leaves the Session on
                // the reading it already had, said out loud because a total
                // quietly frozen is worse than a total that moved late.
                changes.push(SessionChange::SubagentUsageChanged {
                    subagent_usage: delegated,
                });
            }
            if record.snapshot.total_cost != total_cost || record.snapshot.own_cost != own_cost {
                changes.push(SessionChange::TotalCostChanged {
                    total_cost,
                    own_cost,
                });
            }
            if !changes.is_empty()
                && let Err(error) = record.commit_derived(storage, *current, changes)
            {
                tracing::warn!(session_id = %current, "Session Usage did not roll up: {error}");
            }
            let reading = record.snapshot.total_usage();
            if record.summary.total_usage == reading && record.summary.own_cost == own_cost {
                continue;
            }
            record.summary.total_usage = reading;
            record.summary.own_cost = own_cost;
            if ancestry.announces(*current) {
                self.publish_catalog_change(SessionCatalogChange::UsageChanged {
                    session_id: *current,
                    total_usage: reading,
                    own_cost,
                });
            }
        }
    }

    /// Everything this Session's Subagents have consumed, to any depth, and
    /// `None` where they have reported nothing. The Session's own Turns are
    /// left out — the walk skips the Session it starts from — because they
    /// are already in the snapshot every reader holds.
    fn subagent_usage(&self, session_id: SessionId) -> Option<UsageTotal> {
        self.sessions
            .values()
            .filter(|record| record.snapshot.session.parent == Some(session_id))
            .filter_map(|record| record.summary.total_usage)
            .reduce(UsageTotal::saturating_add)
    }

    /// The tree Cost of this Session: its own and every descendant's, native
    /// and brokered, to any depth, and nothing above it.
    pub(super) fn cost_total(&self, session_id: SessionId) -> Option<CostTotal> {
        self.cost_of(&self.subtree(session_id))
    }

    /// The own Cost of this Session: the Provider's account of its own
    /// conversation, read by the same Cost Coverage applied to its Turns
    /// alone, so each reporting lifetime's latest cumulative report counts
    /// once rather than once per Turn that carried one.
    pub(super) fn own_cost(&self, session_id: SessionId) -> Option<CostTotal> {
        self.cost_of(&[session_id])
    }

    /// Applies each Provider-declared coverage window to the frozen Cost
    /// records of `sessions`, a Session and some of what lies beneath it. A
    /// subtree report is cumulative only inside its reporting lifetime, and
    /// receipt order alone never proves that late descendant evidence falls
    /// outside a report.
    fn cost_of(&self, sessions: &[SessionId]) -> Option<CostTotal> {
        let mut records = Vec::new();
        for &owner in sessions {
            let Some(session) = self.sessions.get(&owner) else {
                continue;
            };
            for turn in &session.snapshot.turns {
                records.extend(cost_records(owner, turn));
            }
        }

        let mut lifetime_starts = HashMap::<String, SessionTimestamp>::new();
        let mut latest_subtree = HashMap::<(SessionId, String), ScopedCost>::new();
        for record in records
            .iter()
            .filter(|record| matches!(record.record.coverage, CostCoverage::SessionSubtree { .. }))
        {
            let CostCoverage::SessionSubtree { reporting_lifetime } = &record.record.coverage
            else {
                unreachable!();
            };
            if let Some(started_at) = record.turn_started_at {
                lifetime_starts
                    .entry(reporting_lifetime.clone())
                    .and_modify(|current| *current = (*current).min(started_at))
                    .or_insert(started_at);
            }
            let key = (record.owner, reporting_lifetime.clone());
            if latest_subtree
                .get(&key)
                .is_none_or(|current| current.record.recorded_at < record.record.recorded_at)
            {
                latest_subtree.insert(key, record.clone());
            }
        }
        let aggregates = latest_subtree.into_values().collect::<Vec<_>>();
        let mut overlap_is_partial = false;
        let included = aggregates
            .iter()
            .filter(|candidate| {
                let candidate_interval = cost_interval(candidate, &lifetime_starts);
                !aggregates.iter().any(|cover| {
                    if cover.owner == candidate.owner || !self.covers(cover.owner, candidate.owner)
                    {
                        return false;
                    }
                    let (Some(candidate_interval), Some(cover_interval)) =
                        (candidate_interval, cost_interval(cover, &lifetime_starts))
                    else {
                        overlap_is_partial = true;
                        return true;
                    };
                    if !intervals_overlap(candidate_interval, cover_interval) {
                        return false;
                    }
                    if !interval_contains(cover_interval, candidate_interval) {
                        overlap_is_partial = true;
                    }
                    true
                })
            })
            .collect::<Vec<_>>();

        let mut total = None;
        let mut partial = overlap_is_partial;
        for aggregate in &included {
            total = Some(add_cost(total, aggregate.record.cost));
            partial |= aggregate.record.is_partial;
        }

        for record in records
            .iter()
            .filter(|record| record.record.coverage == CostCoverage::Turn)
        {
            let Some(started_at) = record.turn_started_at else {
                total = Some(add_cost(total, record.record.cost));
                partial = true;
                continue;
            };
            let interval = (started_at, record.record.recorded_at);
            let overlapping = included
                .iter()
                .copied()
                .filter(|aggregate| {
                    self.covers(aggregate.owner, record.owner)
                        && cost_interval(aggregate, &lifetime_starts)
                            .is_some_and(|coverage| intervals_overlap(interval, coverage))
                })
                .max_by_key(|aggregate| aggregate.record.recorded_at);
            if let Some(aggregate) = overlapping {
                let coverage = cost_interval(aggregate, &lifetime_starts)
                    .expect("an overlapping aggregate has an interval");
                partial |= !interval_contains(coverage, interval);
            } else {
                total = Some(add_cost(total, record.record.cost));
                partial |= record.record.is_partial;
            }
        }

        if total.is_some() {
            for &owner in sessions {
                for turn in &self.sessions[&owner].snapshot.turns {
                    let covering = turn.started_at.and_then(|started_at| {
                        included
                            .iter()
                            .copied()
                            .filter(|aggregate| {
                                self.covers(aggregate.owner, owner)
                                    && cost_interval(aggregate, &lifetime_starts).is_some_and(
                                        |coverage| {
                                            coverage.0 <= started_at && started_at <= coverage.1
                                        },
                                    )
                            })
                            .max_by_key(|aggregate| aggregate.record.recorded_at)
                    });
                    if let Some(aggregate) = covering {
                        partial |= work_exceeds_coverage(
                            turn.last_output_at,
                            turn.status == TurnStatus::Active,
                            aggregate.owner == owner && aggregate.turn_id == turn.id,
                            aggregate.record.recorded_at,
                        );
                    } else if turn.cost.is_none() || turn.status == TurnStatus::Active {
                        partial = true;
                    }
                }
            }
        }

        total.map(|cost| CostTotal {
            cost,
            is_partial: partial,
        })
    }

    /// Whether a whole-tree amount `reporter`'s Provider reported can cover
    /// `worker`'s work: only where `worker` is `reporter` or a native
    /// Subagent beneath it, reached without crossing a brokered Session. A
    /// brokered Subagent's work is metered by a Provider actor of its own
    /// (ADR 0035), so no report from above it ever holds that work (ADR
    /// 0039).
    fn covers(&self, reporter: SessionId, mut worker: SessionId) -> bool {
        loop {
            if reporter == worker {
                return true;
            }
            let Some(record) = self.sessions.get(&worker) else {
                return false;
            };
            if record.is_brokered_subagent() {
                return false;
            }
            let Some(parent) = record.snapshot.session.parent else {
                return false;
            };
            worker = parent;
        }
    }

    /// This Session and every Subagent Session below it, to any depth, each
    /// reached after the Session that spawned it — so a walk in reverse
    /// answers the deepest first.
    pub(super) fn subtree(&self, session_id: SessionId) -> Vec<SessionId> {
        let mut walk = vec![session_id];
        let mut visit = 0;
        while visit < walk.len() {
            let current = walk[visit];
            walk.extend(
                self.sessions
                    .iter()
                    .inspect(|_| {
                        // Include whole-map child searches if restoration ever
                        // regresses to using the live traversal again.
                        #[cfg(test)]
                        super::restoration_tests::record_work(1);
                    })
                    .filter(|(_, record)| record.snapshot.session.parent == Some(current))
                    .map(|(child_id, _)| *child_id),
            );
            visit += 1;
        }
        walk
    }

    /// This Session and every ancestor above it, in the order a derivation
    /// climbs them. Both readings a listing carries are derived over a whole
    /// Subagent subtree, so a commit anywhere below can move what the row at
    /// the top of the walk says.
    fn ancestry(&self, session_id: SessionId) -> Ancestry {
        let mut sessions = vec![session_id];
        loop {
            let Some(record) = self
                .sessions
                .get(sessions.last().expect("the walk begins at one Session"))
            else {
                return Ancestry {
                    sessions,
                    listed: false,
                };
            };
            match record.snapshot.session.parent {
                None => {
                    return Ancestry {
                        sessions,
                        listed: true,
                    };
                }
                Some(parent) if self.sessions.contains_key(&parent) => sessions.push(parent),
                // A child severed from its parent joins no listing, so the
                // walk ends on a Session no row stands for.
                Some(_) => {
                    return Ancestry {
                        sessions,
                        listed: false,
                    };
                }
            }
        }
    }

    /// When the uninterrupted live-work interval below this Session began.
    /// Turn intervals are merged with the intervals this Session's own
    /// admissions open across the whole subtree, so a Prompt admitted to begin
    /// a Turn anchors Working before that Turn exists, and a parent Turn that
    /// overlaps a surviving Subagent keeps anchoring it after the parent
    /// Settles. Only the merged interval that is still open matters.
    pub(super) fn subtree_working_since(&self, session_id: SessionId) -> Option<SessionTimestamp> {
        self.subtree_liveness_with(session_id, None).working_since
    }

    /// The subtree's Working and Monitoring readings with the Session at the
    /// head projected through its pending commit. This lets the Session's own
    /// transitions ride the same revision as the Turn transition that caused
    /// them; only ancestors of a changed child need a separate derived
    /// revision.
    ///
    /// Monitoring is what is left when nothing is Working and a Watch anywhere
    /// in the subtree is live. It breaks Working's continuity rather than
    /// extending it, so it counts from whichever came later: when Working last
    /// ended, or when the earliest live Watch started. A Watch the Agent
    /// started mid-Turn therefore reads Monitoring only from that Turn's
    /// settle, and Working after a Watch wakes the Agent counts afresh from
    /// the Continuation it begins.
    fn subtree_liveness_with(
        &self,
        session_id: SessionId,
        projected: Option<&SessionSnapshot>,
    ) -> Liveness {
        let subtree = self.subtree(session_id);
        let last_working = self.last_working_component(&subtree, session_id, projected);
        let working_since = last_working
            .and_then(|(started_at, settled_at)| settled_at.is_none().then_some(started_at));
        let earliest_watch = subtree
            .iter()
            .filter_map(|current| self.sessions.get(current))
            .flat_map(|record| record.watches.values())
            .map(|watch| watch.started_at)
            .min();
        let monitoring_since = match (working_since, earliest_watch) {
            (None, Some(watch_started_at)) => Some(
                last_working
                    .and_then(|(_, settled_at)| settled_at)
                    .map_or(watch_started_at, |ended_at| ended_at.max(watch_started_at)),
            ),
            _ => None,
        };
        Liveness {
            working_since,
            monitoring_since,
        }
    }

    /// The last uninterrupted live-work interval across the subtree, as when
    /// it began and — where it has — when it ended. Turn intervals are merged
    /// with the intervals admissions open, so the last component's end is
    /// when Working last ended anywhere below.
    fn last_working_component(
        &self,
        subtree: &[SessionId],
        session_id: SessionId,
        projected: Option<&SessionSnapshot>,
    ) -> Option<(SessionTimestamp, Option<SessionTimestamp>)> {
        let mut intervals = Vec::new();
        for &current in subtree {
            let Some(record) = self.sessions.get(&current) else {
                continue;
            };
            let snapshot = if current == session_id {
                projected.unwrap_or(&record.snapshot)
            } else {
                &record.snapshot
            };
            intervals.extend(owed_turn_intervals(snapshot, &record.turn_start_admissions));
            intervals.extend(snapshot.turns.iter().filter_map(|turn| {
                let started_at = turn.started_at?;
                let settled_at = if turn.status.is_terminal() {
                    Some(turn.settled_at?)
                } else {
                    None
                };
                Some((started_at, settled_at))
            }));
        }
        intervals.sort_unstable_by_key(|(started_at, _)| *started_at);

        let mut component: Option<(SessionTimestamp, Option<SessionTimestamp>)> = None;
        for (started_at, settled_at) in intervals {
            component = match component {
                Some((component_started_at, component_settled_at))
                    if component_settled_at.is_none_or(|end| started_at <= end) =>
                {
                    let end = match (component_settled_at, settled_at) {
                        (None, _) | (_, None) => None,
                        (Some(left), Some(right)) => Some(left.max(right)),
                    };
                    Some((component_started_at, end))
                }
                _ => Some((started_at, settled_at)),
            };
        }

        component
    }
}

/// A Session subtree's two live readings, derived together because Monitoring
/// is defined by the absence of Working and counts from where Working ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Liveness {
    working_since: Option<SessionTimestamp>,
    monitoring_since: Option<SessionTimestamp>,
}

#[derive(Clone)]
struct ScopedCost {
    owner: SessionId,
    turn_id: TurnId,
    turn_started_at: Option<SessionTimestamp>,
    record: CostRecord,
}

fn cost_records(owner: SessionId, turn: &Turn) -> Vec<ScopedCost> {
    let Some(cost) = turn.cost else {
        return Vec::new();
    };
    let (Some(basis), Some(details)) = (turn.cost_basis, turn.cost_details.as_ref()) else {
        return Vec::new();
    };
    let mut records = details
        .prior
        .iter()
        .cloned()
        .map(|record| ScopedCost {
            owner,
            turn_id: turn.id,
            turn_started_at: turn.started_at,
            record,
        })
        .collect::<Vec<_>>();
    records.push(ScopedCost {
        owner,
        turn_id: turn.id,
        turn_started_at: turn.started_at,
        record: CostRecord {
            cost,
            basis,
            coverage: details.coverage.clone(),
            recorded_at: details.recorded_at,
            is_partial: details.is_partial,
        },
    });
    records
}

fn reporting_lifetime(record: &CostRecord) -> Option<&str> {
    match &record.coverage {
        CostCoverage::Turn => None,
        CostCoverage::SessionSubtree { reporting_lifetime } => Some(reporting_lifetime),
    }
}

fn cost_interval(
    cost: &ScopedCost,
    lifetime_starts: &HashMap<String, SessionTimestamp>,
) -> Option<(SessionTimestamp, SessionTimestamp)> {
    let lifetime = reporting_lifetime(&cost.record)?;
    Some((*lifetime_starts.get(lifetime)?, cost.record.recorded_at))
}

pub(super) fn intervals_overlap(
    left: (SessionTimestamp, SessionTimestamp),
    right: (SessionTimestamp, SessionTimestamp),
) -> bool {
    left.0 <= right.1 && right.0 <= left.1
}

pub(super) fn interval_contains(
    coverage: (SessionTimestamp, SessionTimestamp),
    interval: (SessionTimestamp, SessionTimestamp),
) -> bool {
    coverage.0 <= interval.0 && coverage.1 >= interval.1
}

pub(super) fn work_exceeds_coverage(
    last_output_at: Option<SessionTimestamp>,
    active: bool,
    is_report_carrier: bool,
    coverage_end: SessionTimestamp,
) -> bool {
    last_output_at.is_some_and(|output| output > coverage_end) || active && !is_report_carrier
}

fn add_cost(total: Option<Cost>, cost: Cost) -> Cost {
    total.map_or(cost, |total| total.saturating_add(cost))
}

fn stamp_cost_measurements(changes: &mut [SessionChange], updated_at: SessionTimestamp) {
    for change in changes {
        if let SessionChange::TurnUsageChanged {
            cost,
            cost_recorded_at,
            ..
        } = change
            && cost.is_some()
        {
            *cost_recorded_at = Some(updated_at);
        }
    }
}

fn stamp_output_evidence(
    changes: &mut Vec<SessionChange>,
    snapshot: &SessionSnapshot,
    updated_at: SessionTimestamp,
) {
    let mut observed = Vec::new();
    for (index, change) in changes.iter().enumerate() {
        let turn_id = match change {
            SessionChange::MessageAdded { message }
                if message.role == crate::protocol::MessageRole::Agent
                    && !message.content.is_empty() =>
            {
                Some(message.turn_id)
            }
            SessionChange::MessageContentAppended {
                message_id,
                content,
            } if !content.is_empty() => changes[..index]
                .iter()
                .rev()
                .find_map(|change| match change {
                    SessionChange::MessageAdded { message } if message.id == *message_id => {
                        Some(message.turn_id)
                    }
                    _ => None,
                })
                .or_else(|| {
                    snapshot
                        .messages
                        .iter()
                        .find(|message| message.id == *message_id)
                        .map(|message| message.turn_id)
                }),
            // An Error is Suru's own account of a failure, not output the
            // Turn produced, so it is no evidence the Turn was still at work.
            SessionChange::ActivityAdded { activity }
                if !matches!(activity, crate::protocol::Activity::Error { .. }) =>
            {
                Some(activity.turn_id())
            }
            SessionChange::CommandOutputAppended {
                activity_id,
                content,
            }
            | SessionChange::ReasoningContentAppended {
                activity_id,
                content,
            }
            | SessionChange::ToolCallOutputAppended {
                activity_id,
                content,
            } if !content.is_empty() => activity_turn_id(&changes[..index], snapshot, *activity_id),
            SessionChange::FileChangeUpdated { activity_id, .. }
            | SessionChange::ToolCallInputChanged { activity_id, .. }
            | SessionChange::SubagentDescriptionChanged { activity_id, .. } => {
                activity_turn_id(&changes[..index], snapshot, *activity_id)
            }
            SessionChange::TurnUsageChanged {
                turn_id,
                usage,
                cost_coverage,
                ..
            } if !matches!(cost_coverage, Some(CostCoverage::SessionSubtree { .. }))
                && snapshot
                    .turns
                    .iter()
                    .find(|turn| turn.id == *turn_id)
                    .and_then(|turn| turn.usage.as_ref())
                    != Some(usage) =>
            {
                Some(*turn_id)
            }
            _ => None,
        };
        if let Some(turn_id) = turn_id
            && !observed.contains(&turn_id)
        {
            observed.push(turn_id);
        }
    }
    changes.extend(
        observed
            .into_iter()
            .map(|turn_id| SessionChange::TurnOutputObserved {
                turn_id,
                observed_at: updated_at,
            }),
    );
}

fn activity_turn_id(
    preceding: &[SessionChange],
    snapshot: &SessionSnapshot,
    activity_id: crate::protocol::ActivityId,
) -> Option<TurnId> {
    preceding
        .iter()
        .rev()
        .find_map(|change| match change {
            SessionChange::ActivityAdded { activity } if activity.id() == activity_id => {
                Some(activity.turn_id())
            }
            _ => None,
        })
        .or_else(|| {
            snapshot
                .activities
                .iter()
                .find(|activity| activity.id() == activity_id)
                .map(crate::protocol::Activity::turn_id)
        })
}

/// The Sessions one derivation climbs, from where a commit landed up to the
/// Session a listing holds a row for. That row is the only one an
/// announcement can move, and a walk that ended on a child severed from its
/// parent reached no row at all.
struct Ancestry {
    sessions: Vec<SessionId>,
    listed: bool,
}

impl Ancestry {
    /// Whether a reading that moved on this Session is one the catalog
    /// announces: the walk reached a listed root, and this is it.
    fn announces(&self, session_id: SessionId) -> bool {
        self.listed && self.sessions.last() == Some(&session_id)
    }
}

impl SessionRecord {
    pub(super) fn commit(
        &mut self,
        storage: &StorageSink,
        session_id: SessionId,
        changes: Vec<SessionChange>,
        updated_at: SessionTimestamp,
    ) -> anyhow::Result<SessionUpdate> {
        let update = self.publish(session_id, changes)?;
        self.summary.updated_at = updated_at;
        self.store_and_broadcast(storage, update)
    }

    /// Commits changes the server derived rather than a Provider or a reader
    /// drove — the Subagent roll-up, the Working and Monitoring roll-up, and
    /// Title derivation. It moves the revision
    /// and persists like any commit, but leaves `updated_at` alone: a Session's
    /// own moment of last movement is about its own work. A client holding the
    /// change holds the whole of it.
    pub(super) fn commit_derived(
        &mut self,
        storage: &StorageSink,
        session_id: SessionId,
        changes: Vec<SessionChange>,
    ) -> anyhow::Result<SessionUpdate> {
        let update = self.publish(session_id, changes)?;
        self.store_and_broadcast(storage, update)
    }

    /// Puts one committed revision where everyone reading the Session will
    /// find it: durable storage first, then every attached client.
    fn store_and_broadcast(
        &mut self,
        storage: &StorageSink,
        update: SessionUpdate,
    ) -> anyhow::Result<SessionUpdate> {
        storage.updated(self.summary.clone(), &update)?;
        let _ = self.updates.send(update.clone());
        Ok(update)
    }

    fn publish(
        &mut self,
        session_id: SessionId,
        changes: Vec<SessionChange>,
    ) -> anyhow::Result<SessionUpdate> {
        let revision = SessionRevision(
            self.snapshot
                .revision
                .0
                .checked_add(1)
                .ok_or_else(|| anyhow!("Session revision is exhausted"))?,
        );
        let mut changes = changes
            .into_iter()
            .filter(|change| !matches!(change, SessionChange::SessionStatusChanged { .. }))
            .collect::<Vec<_>>();
        let terminal_turns = changes
            .iter()
            .filter_map(|change| match change {
                SessionChange::TurnStatusChanged {
                    turn_id, status, ..
                } if status.is_terminal() => Some(*turn_id),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut update = SessionUpdate {
            session_id,
            revision,
            changes: changes.clone(),
        };
        let mut next = self.snapshot.clone();
        apply_update(&mut next, &update)?;
        let status = derived_session_status(&next, &self.turn_start_admissions)?;
        if next.session.status != status {
            next.session.status = status;
            changes.push(SessionChange::SessionStatusChanged { status });
            update.changes = changes;
        }
        self.next_prompt_order = next
            .prompts
            .iter()
            .map(|prompt| prompt.admission_order.0)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .map(PromptOrder)
            .ok_or_else(|| anyhow!("Prompt admission order space is exhausted"))?;
        self.steer_targets
            .retain(|_, turn_id| !terminal_turns.contains(turn_id));
        // A wait on Subagents that reached the Broker before its Turn did is
        // spent in the Turn this revision begins, told in the same revision.
        if let Some(waiting_on_subagents) = self.attach_subagent_waits(&next)
            && next.waiting_on_subagents != waiting_on_subagents
        {
            next.waiting_on_subagents = waiting_on_subagents;
            update
                .changes
                .push(SessionChange::SessionWaitingOnSubagentsChanged {
                    waiting_on_subagents,
                });
        }
        self.snapshot = next;
        self.summary.session = self.snapshot.session.clone();
        self.summary.title.clone_from(&self.snapshot.title);
        self.summary.icon.clone_from(&self.snapshot.icon);
        self.summary
            .standing_inputs
            .subagent_interventions
            .clone_from(&self.snapshot.subagent_interventions);
        self.summary.standing_inputs.latest_turn =
            SessionStandingInputs::from_turns(&self.snapshot.turns).latest_turn;
        // `apply_update` is the shared projection boundary for Approval
        // availability. The catalog copies that typed reading instead of
        // independently deriving lifecycle rules from Activities.
        self.summary
            .standing_inputs
            .pending_approvals
            .clone_from(&self.snapshot.pending_approvals);
        self.summary
            .standing_inputs
            .submitting_approvals
            .clone_from(&self.snapshot.submitting_approvals);
        self.summary.standing_inputs.pending_approvals_revision =
            self.snapshot.pending_approvals_revision;
        let pending =
            self.snapshot
                .activities
                .iter()
                .filter_map(|activity| match activity {
                    crate::protocol::Activity::Questionnaire {
                        questionnaire,
                        outcome,
                        turn_id,
                        ..
                    } if outcome.is_answerable()
                        && self.snapshot.turns.iter().any(|turn| {
                            turn.id == *turn_id && turn.status == TurnStatus::Active
                        }) =>
                    {
                        Some(questionnaire.id)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
        let submitting = self
            .snapshot
            .activities
            .iter()
            .filter_map(|activity| match activity {
                crate::protocol::Activity::Questionnaire {
                    questionnaire,
                    outcome: crate::protocol::QuestionnaireOutcome::Submitting,
                    ..
                } => Some(questionnaire.id),
                _ => None,
            })
            .collect::<Vec<_>>();
        if self.summary.standing_inputs.pending_questionnaires != pending
            || self.summary.standing_inputs.submitting_questionnaires != submitting
        {
            self.summary.standing_inputs.submitting_questionnaires = submitting;
            self.summary.standing_inputs.pending_questionnaires = pending;
            self.summary.standing_inputs.pending_questionnaires_revision = self.snapshot.revision;
        }
        // `session.working_since` and `session.monitoring_since` are
        // deliberately left alone here: they carry the whole subtree's
        // reading, which only the state can derive, so
        // [`SessionStoreState::reconcile_liveness`] maintains them after every
        // commit.
        Ok(update)
    }

    /// Projects one pending batch without mutating, storing, or broadcasting
    /// it. Working derivation needs to see the stamped Turn state before the
    /// real commit so its canonical clock can join that same revision.
    fn project(
        &self,
        session_id: SessionId,
        changes: &[SessionChange],
    ) -> anyhow::Result<SessionSnapshot> {
        let revision = SessionRevision(
            self.snapshot
                .revision
                .0
                .checked_add(1)
                .ok_or_else(|| anyhow!("Session revision is exhausted"))?,
        );
        let mut projected = self.snapshot.clone();
        apply_update(
            &mut projected,
            &SessionUpdate {
                session_id,
                revision,
                changes: changes.to_vec(),
            },
        )?;
        Ok(projected)
    }
}

/// Stamps the commit's own timestamp onto the Turn timing the changes carry:
/// a Turn starts when the commit that delivers its opening Prompt lands, and
/// settles when the commit that settles it lands. Minting timestamps is the
/// store's job, so the change builders leave both absent and the commit fills
/// them in — including for a Turn that arrives already settled, which starts
/// and settles in the one commit.
fn stamp_turn_timing(changes: &mut [SessionChange], committed_at: SessionTimestamp) {
    for change in changes {
        match change {
            SessionChange::TurnAdded { turn } => {
                turn.started_at = Some(committed_at);
                if turn.status.is_terminal() {
                    turn.settled_at = Some(committed_at);
                }
            }
            // A settlement that already carries its moment is a Turn found open
            // at the next start, settled at the last time it showed work; every
            // live settlement leaves it to the commit's clock (ADR 0029).
            SessionChange::TurnStatusChanged {
                status, settled_at, ..
            } if status.is_terminal() => {
                settled_at.get_or_insert(committed_at);
            }
            _ => {}
        }
    }
}

pub(super) fn active_turn_id(snapshot: &SessionSnapshot) -> anyhow::Result<Option<TurnId>> {
    let mut active = snapshot
        .turns
        .iter()
        .filter(|turn| turn.status == TurnStatus::Active)
        .map(|turn| turn.id);
    let first = active.next();
    if active.next().is_some() {
        return Err(anyhow!("Session cannot contain more than one active Turn"));
    }
    Ok(first)
}

/// A Session is Active while it is running a Turn and while it still owes one
/// to a Prompt it has admitted but not delivered: both are the Session at
/// work, and a surface reading the status to explain Working must find one
/// either side of the delivery that joins them (ADR 0024).
pub(super) fn derived_session_status(
    snapshot: &SessionSnapshot,
    turn_start_admissions: &HashMap<PromptId, SessionTimestamp>,
) -> anyhow::Result<SessionStatus> {
    Ok(
        if active_turn_id(snapshot)?.is_some() || owes_a_turn(snapshot, turn_start_admissions) {
            SessionStatus::Active
        } else {
            SessionStatus::Idle
        },
    )
}

/// Whether any Prompt admitted to begin a Turn is still waiting to be
/// delivered.
pub(super) fn owes_a_turn(
    snapshot: &SessionSnapshot,
    turn_start_admissions: &HashMap<PromptId, SessionTimestamp>,
) -> bool {
    undelivered_turn_starts(snapshot, turn_start_admissions)
        .next()
        .is_some()
}

/// Every Prompt this Session was admitted to begin a Turn with and has not
/// delivered, earliest admission first.
pub(super) fn undelivered_turn_starts<'a>(
    snapshot: &'a SessionSnapshot,
    turn_start_admissions: &'a HashMap<PromptId, SessionTimestamp>,
) -> impl Iterator<Item = &'a Prompt> {
    let mut owed = snapshot
        .prompts
        .iter()
        .filter(|prompt| {
            prompt.status == PromptStatus::Pending && turn_start_admissions.contains_key(&prompt.id)
        })
        .collect::<Vec<_>>();
    owed.sort_unstable_by_key(|prompt| prompt.admission_order);
    owed.into_iter()
}

/// The Working intervals this Session's own admissions contribute: one open
/// interval for a Prompt still owed its Turn, and one closed at the Turn's
/// start for a Prompt whose Turn has begun — which is what keeps Working
/// continuous across a delivery instead of restarting it at the Turn. A
/// Prompt that ended without a Turn — withdrawn, or cancelled where it stood —
/// contributes nothing, so the Session stops Working the moment it does.
fn owed_turn_intervals<'a>(
    snapshot: &'a SessionSnapshot,
    turn_start_admissions: &'a HashMap<PromptId, SessionTimestamp>,
) -> impl Iterator<Item = (SessionTimestamp, Option<SessionTimestamp>)> {
    turn_start_admissions
        .iter()
        .filter_map(|(prompt_id, admitted_at)| {
            let prompt = snapshot
                .prompts
                .iter()
                .find(|prompt| prompt.id == *prompt_id)?;
            if prompt.status == PromptStatus::Pending {
                return Some((*admitted_at, None));
            }
            let started_at = snapshot
                .turns
                .iter()
                .find(|turn| turn.prompt_id == Some(*prompt_id))?
                .started_at?;
            Some((*admitted_at, Some(started_at)))
        })
}

#[cfg(test)]
mod tests {
    use crate::{
        protocol::{
            AdmitPromptRequest, CreateSessionRequest, InitialPrompt, PromptDelivery, PromptId,
        },
        sessions::{DeliveredTurnStatus, ProviderTurnOutcome, SessionStore, StoreOutcome},
        storage::{StorageRepository, StorageWriter},
    };

    /// Admissions are bookkeeping the Working derivation reads, not a record
    /// the Session keeps: each one is forgotten as soon as it can no longer
    /// move that reading, so a long-lived Session's bookkeeping is the size of
    /// the work in front of it rather than the work behind it.
    #[tokio::test]
    async fn admissions_are_forgotten_once_the_work_they_anchor_is_over() {
        let data_dir = tempfile::tempdir().expect("create isolated data directory");
        let execution_directory = tempfile::tempdir().expect("create valid Workspace");
        let repository = StorageRepository::open(data_dir.path())
            .await
            .expect("open Session repository");
        let (_writer, storage) = StorageWriter::spawn(repository, &[]);
        let store = SessionStore::new(Default::default(), storage, Vec::new(), Default::default());
        let first = PromptId::new();
        let StoreOutcome::Created(snapshot) = store
            .create(CreateSessionRequest {
                preparation_id: None,
                agent_selection: None,
                execution_directory: crate::protocol::ExecutionDirectory {
                    path: execution_directory.path().to_owned(),
                },
                prompt: InitialPrompt {
                    id: first,
                    text: "Map the provider seams".to_owned(),
                    skill_invocations: Vec::new(),
                    attachments: Vec::new(),
                },
            })
            .expect("create Session")
        else {
            panic!("a fresh Prompt creates a Session")
        };
        let session_id = snapshot.session.id;
        let admissions = |store: &SessionStore| {
            store
                .state
                .lock()
                .expect("Session store lock is not poisoned")
                .sessions[&session_id]
                .turn_start_admissions
                .len()
        };
        let admitted_at = snapshot
            .session
            .working_since
            .expect("the created Session owes a Turn");
        assert_eq!(admissions(&store), 1, "the Prompt is owed a Turn");

        for prompt_id in [first, PromptId::new(), PromptId::new()] {
            if prompt_id != first {
                let StoreOutcome::Created(_) = store
                    .admit(
                        session_id,
                        AdmitPromptRequest {
                            delivery: PromptDelivery::Steer,
                            prompt: InitialPrompt {
                                id: prompt_id,
                                text: "And again".to_owned(),
                                skill_invocations: Vec::new(),
                                attachments: Vec::new(),
                            },
                        },
                        Vec::new(),
                    )
                    .expect("admit a follow-up Prompt")
                else {
                    panic!("a fresh Prompt is admitted")
                };
                assert_eq!(admissions(&store), 1, "one Turn is owed at a time");
            }
            let delivered = store
                .deliver_prompt(session_id, prompt_id, None, DeliveredTurnStatus::Active)
                .expect("deliver the Prompt")
                .expect("the Prompt was still owed a Turn");
            let working = store
                .snapshot(session_id)
                .and_then(|snapshot| snapshot.session.working_since)
                .expect("a delivered Prompt leaves its Turn working");
            if prompt_id == first {
                assert_eq!(
                    working, admitted_at,
                    "delivery continues the Working its admission began"
                );
            }
            assert_eq!(
                admissions(&store),
                1,
                "the admission still anchors the Turn it began"
            );
            store
                .finish_provider_turn(
                    session_id,
                    delivered.turn_id,
                    ProviderTurnOutcome::Completed {
                        trailing_output: Default::default(),
                    },
                )
                .expect("settle the Turn");
            assert_eq!(
                store.snapshot(session_id).and_then(|s| s.working_since()),
                None,
                "a settled Turn leaves nothing working"
            );
            assert_eq!(
                admissions(&store),
                0,
                "and nothing left to anchor it with either"
            );
        }
    }
}
