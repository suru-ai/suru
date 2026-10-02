//! The outline of a Session tree: what a Peer's Server reads of a tree on
//! this one to follow the work its Sidekick set going there
//! (CONTEXT.md: Sidekick Report).
//!
//! It is read under one lock, so every Session in it stands as it stood at
//! one moment, stamped with that moment on this Server's clock — the clock
//! every Prompt's taking, Intervention and Turn's settling is stamped by — so
//! a reader orders all of it, across every Session, as it happened. It holds
//! nothing anyone wrote: the first Message of each Turn is kept for who
//! opened it and nothing it said, and of the Activities only the rows leading
//! into Subagents and the Questionnaires and Approvals, emptied of what they
//! ask and what was answered.

use std::collections::HashSet;

use crate::protocol::{Activity, SessionId, SessionSnapshot, SessionTreeOutline};
use crate::storage::StorageError;

use super::SessionStore;

impl SessionStore {
    /// The outline of the tree `session_id` belongs to, read in one moment,
    /// the tree brought into memory first. `None` where no such Session is
    /// held.
    pub(crate) async fn tree_outline(
        &self,
        session_id: SessionId,
    ) -> Result<Option<SessionTreeOutline>, StorageError> {
        self.hydrate(session_id).await?;
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let Some(top_level) = state.top_level_of(session_id) else {
            return Ok(None);
        };
        let sessions = state
            .subtree(top_level)
            .into_iter()
            .filter_map(|session_id| {
                let record = state.sessions.get(&session_id)?;
                (!state.is_deferred(session_id)).then(|| outlined(&record.snapshot))
            })
            .collect();
        Ok(Some(SessionTreeOutline {
            read_at: state.next_timestamp(),
            sessions,
        }))
    }
}

/// `snapshot` cut to its outline.
fn outlined(snapshot: &SessionSnapshot) -> SessionSnapshot {
    let mut outlined = snapshot.clone();
    let mut opened = HashSet::new();
    outlined
        .messages
        .retain(|message| opened.insert(message.turn_id));
    for message in &mut outlined.messages {
        message.content.clear();
        message.skill_invocations.clear();
        message.attachments.clear();
    }
    for prompt in &mut outlined.prompts {
        prompt.text.clear();
        prompt.skill_invocations.clear();
        prompt.attachments.clear();
    }
    outlined.activities.retain_mut(|activity| match activity {
        Activity::Subagent { description, .. } => {
            description.clear();
            true
        }
        Activity::Questionnaire {
            questionnaire,
            answer,
            ..
        } => {
            questionnaire.questions.clear();
            if let Some(answer) = answer {
                answer.questions.clear();
            }
            true
        }
        Activity::Approval { .. } => true,
        _ => false,
    });
    outlined.transcript.clear();
    outlined
}
