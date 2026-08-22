//! How a batch of changes becomes the Session's next snapshot: the commit that
//! projects, persists, and broadcasts one revision, plus the invariants the
//! store reads back off a snapshot.

use anyhow::anyhow;

use crate::protocol::{
    PromptOrder, SessionChange, SessionId, SessionRevision, SessionSnapshot, SessionStatus,
    SessionTimestamp, SessionUpdate, TurnId, TurnStatus,
};
use crate::session_projection::apply_update;
use crate::storage::StorageSink;

use super::{SessionRecord, SessionStore};

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
        if !state.sessions.contains_key(&session_id) {
            return Err(anyhow!("Session does not exist on this server instance"));
        }
        let updated_at = state.next_timestamp();
        let stored = state
            .sessions
            .get_mut(&session_id)
            .expect("Session existence was checked while holding the store lock");
        stored.commit(&self.storage, session_id, changes, updated_at)
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
