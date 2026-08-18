//! Shared projection rules for authoritative and client-side Session state.

use anyhow::{Result, anyhow};

use crate::protocol::{SessionChange, SessionSnapshot, SessionUpdate};

pub(crate) fn apply_update(snapshot: &mut SessionSnapshot, update: &SessionUpdate) -> Result<()> {
    if snapshot.session.id != update.session_id {
        return Err(anyhow!("Session update targeted a different Session"));
    }
    if !update.revision.immediately_follows(snapshot.revision) {
        return Err(anyhow!("Session update revision is not monotonic"));
    }

    let mut next = snapshot.clone();
    for change in &update.changes {
        match change {
            SessionChange::PromptAdded { prompt } => next.prompts.push(prompt.clone()),
            SessionChange::TurnAdded { turn } => next.turns.push(turn.clone()),
            SessionChange::MessageAdded { message } => next.messages.push(message.clone()),
            SessionChange::ActivityAdded { activity } => next.activities.push(activity.clone()),
            SessionChange::TurnStatusChanged { turn_id, status } => {
                let Some(turn) = next.turns.iter_mut().find(|turn| turn.id == *turn_id) else {
                    return Err(anyhow!("Session update referenced an unknown Turn"));
                };
                turn.status = *status;
            }
            SessionChange::SessionStatusChanged { status } => next.session.status = *status,
        }
    }
    next.revision = update.revision;
    *snapshot = next;
    Ok(())
}
