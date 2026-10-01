//! Subsessions: the Sessions a Sidekick begins on the user's behalf, each of
//! which remembers the Sidekick's Session that began it (CONTEXT.md:
//! Subsession).
//!
//! A Subsession is a top-level Session in every way that matters to its
//! work, and nothing here makes it otherwise: it is no child of the Sidekick's
//! Session, so its Turns keep no Sidekick Working, an interrupt or a deletion
//! of the Sidekick's Session never reaches it, and its Usage and Cost are
//! rolled up beneath nothing. What the Sidekick's Transcript carries of it is
//! one row, standing where it was begun and leading into it, which names it
//! by the Title it has — so the store carries every Title derived for a
//! Subsession onto that row.

use crate::protocol::{Activity, ActivityId, SessionChange, SessionId, SessionSnapshot};
use crate::storage::StorageSink;

use super::{SessionStore, SessionStoreState};

impl SessionStore {
    /// Stands the row recording that the Sidekick of `sidekick` began
    /// `subsession` in that Sidekick's Transcript, naming the Subsession by
    /// its Title and saying what it was first asked: in the Turn the Sidekick
    /// works in, or in a Continuation begun to hold it where none works, as a
    /// brokered Subagent's row stands (see [`SessionStoreState::stand_row`]).
    ///
    /// The Subsession stands whether or not its row does, since it is a
    /// Session of its own that every listing reaches; a Sidekick's Session
    /// that is not held — deleted, or its history still unread — gains none.
    pub(crate) fn stand_subsession_row(
        &self,
        sidekick: SessionId,
        subsession: &SessionSnapshot,
    ) -> anyhow::Result<()> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        if state.is_deferred(sidekick) {
            anyhow::bail!("the Sidekick's Session is not held");
        }
        let prompt = subsession
            .prompts
            .first()
            .map(|prompt| prompt.text.clone())
            .unwrap_or_default();
        state.stand_row(&self.storage, sidekick, |turn_id| Activity::Subsession {
            id: ActivityId::new(),
            turn_id,
            session_id: subsession.session.id,
            title: subsession.title.clone(),
            prompt,
        })
    }
}

impl SessionStoreState {
    /// Carries the Title `subsession` has now onto every row in its
    /// Sidekick's Transcript that leads into it, where `subsession` is a
    /// Subsession and its Sidekick's Session is held. A Sidekick's Session
    /// whose history is still unread keeps the Title its rows last carried.
    pub(super) fn follow_subsession_title(&mut self, storage: &StorageSink, subsession: SessionId) {
        let Some(record) = self.sessions.get(&subsession) else {
            return;
        };
        let Some(sidekick) = record.snapshot.session.sidekick() else {
            return;
        };
        let title = record.summary.title.clone();
        if self.is_deferred(sidekick) {
            return;
        }
        let Some(holder) = self.sessions.get_mut(&sidekick) else {
            return;
        };
        let changes = holder
            .snapshot
            .activities
            .iter()
            .filter_map(|activity| match activity {
                Activity::Subsession {
                    id,
                    session_id,
                    title: named,
                    ..
                } if *session_id == subsession && *named != title => {
                    Some(SessionChange::SubsessionTitleChanged {
                        activity_id: *id,
                        title: title.clone(),
                    })
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        if changes.is_empty() {
            return;
        }
        if let Err(error) = holder.commit_derived(storage, sidekick, changes) {
            tracing::warn!(
                %subsession,
                %sidekick,
                "a Subsession's row did not follow its Title: {error:#}"
            );
        }
    }
}
