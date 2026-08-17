//! Client-side projection of one provider-neutral Session stream.

use anyhow::{Result, anyhow};

use crate::protocol::{SessionChange, SessionSnapshot, SessionUpdate};

#[derive(Clone, Debug)]
pub(crate) struct SessionProjection {
    snapshot: SessionSnapshot,
}

impl SessionProjection {
    pub(crate) fn new(snapshot: SessionSnapshot) -> Self {
        Self { snapshot }
    }

    pub(crate) fn snapshot(&self) -> &SessionSnapshot {
        &self.snapshot
    }

    pub(crate) fn apply(&mut self, update: SessionUpdate) -> Result<()> {
        if self.snapshot.session.id != update.session_id {
            return Err(anyhow!("Session update targeted a different Session"));
        }
        if !update.revision.immediately_follows(self.snapshot.revision) {
            return Err(anyhow!("Session update revision is not monotonic"));
        }

        let mut next = self.snapshot.clone();
        for change in update.changes {
            match change {
                SessionChange::PromptAdded { prompt } => next.prompts.push(prompt),
                SessionChange::TurnAdded { turn } => next.turns.push(turn),
                SessionChange::MessageAdded { message } => next.messages.push(message),
                SessionChange::ActivityAdded { activity } => next.activities.push(activity),
                SessionChange::TurnStatusChanged { turn_id, status } => {
                    let Some(turn) = next.turns.iter_mut().find(|turn| turn.id == turn_id) else {
                        return Err(anyhow!("Session update referenced an unknown Turn"));
                    };
                    turn.status = status;
                }
                SessionChange::SessionStatusChanged { status } => next.session.status = status,
            }
        }
        next.revision = update.revision;
        self.snapshot = next;
        Ok(())
    }
}
