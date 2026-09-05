//! How a batch of changes becomes the Session's next snapshot: the commit that
//! projects, persists, and broadcasts one revision, plus the invariants the
//! store reads back off a snapshot.

use anyhow::anyhow;

use crate::protocol::{
    PromptOrder, SessionCatalogChange, SessionChange, SessionId, SessionRevision, SessionSnapshot,
    SessionStandingInputs, SessionStatus, SessionTimestamp, SessionUpdate, TurnId, TurnStatus,
    UsageTotal,
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
    /// Commits `changes` to one Session and then re-derives the Working
    /// reading above it. Every commit goes through here rather than reaching
    /// [`SessionRecord::commit`] directly, because a commit anywhere in a
    /// Subagent subtree — a child's Turn settling, a spawn opening one — can
    /// flip what the listed root's row says about live work, and only the
    /// state can see across Sessions.
    pub(super) fn commit(
        &mut self,
        storage: &StorageSink,
        session_id: SessionId,
        mut changes: Vec<SessionChange>,
    ) -> anyhow::Result<SessionUpdate> {
        let updated_at = self.next_timestamp();
        // Working is a server derivation. Callers can describe the Turn
        // transition that changes it, but cannot inject a competing clock.
        changes.retain(|change| !matches!(change, SessionChange::SessionWorkingChanged { .. }));
        stamp_turn_timing(&mut changes, updated_at);
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
        let working_since = self.subtree_working_since_with(session_id, Some(&projected));
        let working_changed = projected.session.working_since != working_since;
        if working_changed {
            changes.push(SessionChange::SessionWorkingChanged { working_since });
        }
        let record = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?;
        let previous_standing = record.summary.standing_inputs.clone();
        let update = record.commit(storage, session_id, changes, updated_at)?;
        let standing_inputs = record.summary.standing_inputs.clone();
        if (turn_settled
            || previous_standing.pending_questionnaires != standing_inputs.pending_questionnaires
            || previous_standing.submitting_questionnaires
                != standing_inputs.submitting_questionnaires)
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
        self.reconcile_working(storage, session_id);
        self.reconcile_usage(storage, session_id);
        self.reconcile_questionnaires(storage, session_id);
        Ok(update)
    }

    /// Carries descendant availability to each ancestor without copying child
    /// Transcript content or making a child a catalog entry.
    fn reconcile_questionnaires(&mut self, storage: &StorageSink, session_id: SessionId) {
        let ancestry = self.ancestry(session_id);
        for current in &ancestry.sessions {
            let mut reading = Vec::new();
            for child in self.subtree(*current).into_iter().skip(1) {
                let child_record = &self.sessions[&child];
                if child_record
                    .summary
                    .standing_inputs
                    .pending_questionnaires_revision
                    .0
                    == 0
                {
                    continue;
                }
                let mut via = child;
                while let Some(parent) = self.sessions[&via].snapshot.session.parent {
                    if parent == *current {
                        break;
                    }
                    via = parent;
                }
                reading.push(crate::protocol::SubagentQuestionnaires {
                    session_id: child,
                    via_session_id: via,
                    revision: child_record
                        .summary
                        .standing_inputs
                        .pending_questionnaires_revision,
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
                });
            }
            reading.sort_by_key(|entry| entry.session_id.as_uuid());
            let Some(record) = self.sessions.get_mut(current) else {
                continue;
            };
            if record.snapshot.subagent_questionnaires == reading {
                continue;
            }
            if let Err(error) = record.commit_derived(
                storage,
                *current,
                vec![SessionChange::SubagentQuestionnairesChanged {
                    subagent_questionnaires: reading,
                }],
            ) {
                tracing::warn!(session_id = %current, "Subagent Questionnaire availability did not roll up: {error}");
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

    /// Re-derives [`crate::protocol::Session::working_since`] for the Session
    /// and every ancestor up to its listed root, and announces the root's
    /// reading when it flipped. Each Session carries its whole subtree's
    /// reading — the latest Turn, or any working Subagent below —
    /// so a listing keeps saying Working while Subagents outlive the Turn
    /// that spawned them (ADR 0015). Only the root announces, because a
    /// Subagent's child Session rides no catalog stream.
    pub(super) fn reconcile_working(&mut self, storage: &StorageSink, session_id: SessionId) {
        let ancestry = self.ancestry(session_id);
        for current in &ancestry.sessions {
            let reading = self.subtree_working_since(*current);
            let Some(record) = self.sessions.get_mut(current) else {
                continue;
            };
            if record.snapshot.session.working_since == reading {
                continue;
            }
            if let Err(error) = record.commit_derived(
                storage,
                *current,
                vec![SessionChange::SessionWorkingChanged {
                    working_since: reading,
                }],
            ) {
                tracing::warn!(session_id = %current, "Working did not roll up: {error}");
            }
            if ancestry.announces(*current) {
                self.publish_catalog_change(SessionCatalogChange::WorkingChanged {
                    session_id: *current,
                    working_since: reading,
                });
            }
        }
    }

    /// Restores the canonical Working interval on every Session in a subtree
    /// without spending revisions or writing the derivation back to storage.
    /// Turn timing is durable, so the uninterrupted interval can be rebuilt
    /// after restart even though Working itself has no database column.
    pub(super) fn restore_working(&mut self, root: SessionId) {
        for session_id in self.subtree(root) {
            let reading = self.subtree_working_since(session_id);
            let Some(record) = self.sessions.get_mut(&session_id) else {
                continue;
            };
            record.snapshot.session.working_since = reading;
            record.summary.session.working_since = reading;
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
            let Some(record) = self.sessions.get_mut(current) else {
                continue;
            };
            if record.snapshot.subagent_usage != delegated {
                // The roll-up is the server's own derivation rather than
                // Agent output, so it goes straight to the record's commit:
                // the gate output passes through has no Turn to check it
                // against. A commit that cannot land leaves the Session on
                // the reading it already had, said out loud because a total
                // quietly frozen is worse than a total that moved late.
                if let Err(error) = record.commit_derived(
                    storage,
                    *current,
                    vec![SessionChange::SubagentUsageChanged {
                        subagent_usage: delegated,
                    }],
                ) {
                    tracing::warn!(session_id = %current, "Subagent Usage did not roll up: {error}");
                }
            }
            let reading = record.snapshot.total_usage();
            if record.summary.total_usage == reading {
                continue;
            }
            record.summary.total_usage = reading;
            if ancestry.announces(*current) {
                self.publish_catalog_change(SessionCatalogChange::UsageChanged {
                    session_id: *current,
                    total_usage: reading,
                });
            }
        }
    }

    /// Derives the roll-up across one restored Session's whole subtree,
    /// deepest first so every Session is answered from children already
    /// derived. Nothing is committed or announced: a restored Session has no
    /// revision to spend and no client to tell yet.
    pub(super) fn restore_usage(&mut self, root: SessionId) {
        for session_id in self.subtree(root).into_iter().rev() {
            let delegated = self.subagent_usage(session_id);
            let Some(record) = self.sessions.get_mut(&session_id) else {
                continue;
            };
            record.snapshot.subagent_usage = delegated;
            record.summary.total_usage = record.snapshot.total_usage();
        }
    }

    /// Everything this Session's Subagents have consumed, to any depth, and
    /// `None` where they have reported nothing. The Session's own Turns are
    /// left out — the walk skips the Session it starts from — because they
    /// are already in the snapshot every reader holds.
    fn subagent_usage(&self, session_id: SessionId) -> Option<UsageTotal> {
        self.subtree(session_id)
            .into_iter()
            .skip(1)
            .filter_map(|child| UsageTotal::of_turns(&self.sessions.get(&child)?.snapshot.turns))
            .reduce(UsageTotal::saturating_add)
    }

    /// This Session and every Subagent Session below it, to any depth, each
    /// reached after the Session that spawned it — so a walk in reverse
    /// answers the deepest first.
    fn subtree(&self, session_id: SessionId) -> Vec<SessionId> {
        let mut walk = vec![session_id];
        let mut visit = 0;
        while visit < walk.len() {
            let current = walk[visit];
            walk.extend(
                self.sessions
                    .iter()
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
    /// Turn intervals are merged across the whole subtree, so a parent Turn
    /// that overlaps a surviving Subagent keeps anchoring Working after the
    /// parent Settles. Only the merged interval that is still open matters.
    pub(super) fn subtree_working_since(&self, session_id: SessionId) -> Option<SessionTimestamp> {
        self.subtree_working_since_with(session_id, None)
    }

    /// The subtree reading with the Session at the head projected through its
    /// pending commit. This lets the Session's own Working transition ride the
    /// same revision as the Turn transition that caused it; only ancestors of
    /// a changed child need a separate derived revision.
    fn subtree_working_since_with(
        &self,
        session_id: SessionId,
        projected: Option<&SessionSnapshot>,
    ) -> Option<SessionTimestamp> {
        let mut intervals = Vec::new();
        for current in self.subtree(session_id) {
            let snapshot = if current == session_id {
                projected.or_else(|| self.sessions.get(&current).map(|record| &record.snapshot))
            } else {
                self.sessions.get(&current).map(|record| &record.snapshot)
            };
            let Some(snapshot) = snapshot else {
                continue;
            };
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

        component.and_then(|(started_at, settled_at)| settled_at.is_none().then_some(started_at))
    }
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
    /// drove — the Subagent roll-up today. It moves the revision and persists
    /// like any commit, but leaves `updated_at` alone: a Session's own moment
    /// of last movement is about its own work, and nothing a listing orders
    /// or draws by moves here, so a client holding the change holds the whole
    /// of it.
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
        let status = derived_session_status(&next)?;
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
        self.snapshot = next;
        self.summary.session = self.snapshot.session.clone();
        self.summary
            .standing_inputs
            .subagent_questionnaires
            .clone_from(&self.snapshot.subagent_questionnaires);
        self.summary.standing_inputs.latest_turn =
            SessionStandingInputs::from_turns(&self.snapshot.turns).latest_turn;
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
        // `session.working_since` is deliberately left alone here: it carries
        // the whole subtree's reading, which only the state can derive, so
        // [`SessionStoreState::reconcile_working`] maintains it after every
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
            SessionChange::TurnStatusChanged {
                status, settled_at, ..
            } if status.is_terminal() => *settled_at = Some(committed_at),
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

fn derived_session_status(snapshot: &SessionSnapshot) -> anyhow::Result<SessionStatus> {
    Ok(if active_turn_id(snapshot)?.is_some() {
        SessionStatus::Active
    } else {
        SessionStatus::Idle
    })
}
