//! How a batch of changes becomes the Session's next snapshot: the commit that
//! projects, persists, and broadcasts one revision, plus the invariants the
//! store reads back off a snapshot.

use anyhow::anyhow;

use crate::protocol::{
    PromptOrder, SessionCatalogChange, SessionChange, SessionId, SessionRevision, SessionSnapshot,
    SessionStatus, SessionTimestamp, SessionUpdate, TurnId, TurnStatus,
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
        changes: Vec<SessionChange>,
    ) -> anyhow::Result<SessionUpdate> {
        let updated_at = self.next_timestamp();
        let record = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| anyhow!("Session does not exist on this server instance"))?;
        let update = record.commit(storage, session_id, changes, updated_at)?;
        self.reconcile_working(session_id);
        Ok(update)
    }

    /// Re-derives [`crate::protocol::SessionSummary::working_since`] for the
    /// Session and every ancestor up to its listed root, and announces the
    /// root's reading when it flipped. Each summary carries its whole
    /// subtree's reading — the latest Turn, or any working Subagent below —
    /// so a listing keeps saying Working while Subagents outlive the Turn
    /// that spawned them (ADR 0015). Only the root announces, because a
    /// Subagent's child Session rides no catalog stream.
    pub(super) fn reconcile_working(&mut self, session_id: SessionId) {
        let mut current = session_id;
        loop {
            let reading = self.subtree_working_since(current);
            let Some(record) = self.sessions.get_mut(&current) else {
                return;
            };
            let flipped = record.summary.working_since != reading;
            record.summary.working_since = reading;
            let parent = record.snapshot.session.parent;
            match parent {
                Some(parent) if self.sessions.contains_key(&parent) => current = parent,
                None => {
                    if flipped {
                        self.publish_catalog_change(SessionCatalogChange::WorkingChanged {
                            session_id: current,
                            working_since: reading,
                        });
                    }
                    return;
                }
                // A child severed from its parent joins no listing, so there
                // is no row its reading could move.
                Some(_) => return,
            }
        }
    }

    /// When live work below this Session began: the earliest moment any
    /// still-working Turn in its subtree started — its own latest Turn, or a
    /// Subagent's at any depth — and `None` when everything has settled.
    pub(super) fn subtree_working_since(&self, session_id: SessionId) -> Option<SessionTimestamp> {
        let mut walk = vec![session_id];
        let mut earliest: Option<SessionTimestamp> = None;
        let mut visit = 0;
        while visit < walk.len() {
            let current = walk[visit];
            if let Some(record) = self.sessions.get(&current)
                && let Some(since) = record.snapshot.working_since()
            {
                earliest = Some(earliest.map_or(since, |held| held.min(since)));
            }
            walk.extend(
                self.sessions
                    .iter()
                    .filter(|(_, record)| record.snapshot.session.parent == Some(current))
                    .map(|(child_id, _)| *child_id),
            );
            visit += 1;
        }
        earliest
    }
}

impl SessionRecord {
    pub(super) fn commit(
        &mut self,
        storage: &StorageSink,
        session_id: SessionId,
        mut changes: Vec<SessionChange>,
        updated_at: SessionTimestamp,
    ) -> anyhow::Result<SessionUpdate> {
        stamp_turn_timing(&mut changes, updated_at);
        let update = self.publish(session_id, changes)?;
        self.summary.updated_at = updated_at;
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
        // `summary.working_since` is deliberately left alone here: it carries
        // the whole subtree's reading, which only the state can derive, so
        // [`SessionStoreState::reconcile_working`] maintains it after every
        // commit.
        Ok(update)
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
