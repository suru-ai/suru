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
//! by the Title it has.
//!
//! The row is begun with the Subsession, in the same lock, so no reader finds
//! one without the other. What can still part them is outside any lock: a
//! Server stopping between the two writes, or a Title the Subsession takes
//! while its Sidekick's Session is not read. So the row is reconciled rather
//! than trusted — on every path that finds a Subsession again, a retried
//! beginning or a rejoined Worktree preparation, and whenever the Sidekick's
//! Session is read back from storage — and reconciling a row that already
//! stands as it should changes nothing.

use futures_util::future::BoxFuture;

use crate::protocol::{Activity, ActivityId, SessionChange, SessionId, TurnId};
use crate::storage::{StorageError, StorageSink};

use super::{SessionStore, SessionStoreState, projection::active_turn_id};

impl SessionStore {
    /// Puts right the row leading into `subsession` in the Transcript of the
    /// Sidekick's Session `sidekick`: stands it where it is missing, and has
    /// it name the Subsession's Title where it names another. A Sidekick's
    /// Session that is not held now is put right when it is next read.
    pub(crate) fn reconcile_subsession_row(
        &self,
        sidekick: SessionId,
        subsession: SessionId,
    ) -> anyhow::Result<()> {
        self.state
            .lock()
            .expect("Session store lock is not poisoned")
            .reconcile_subsession_row(&self.storage, sidekick, subsession)
            .map(|_| ())
    }

    /// Puts right the rows leading into every Subsession the Sidekick of
    /// `sidekick` began, just read back from storage: a Subsession whose row
    /// a stop lost is read too, for what it was first asked, and gains its
    /// row; one whose Title moved on while the Sidekick's Session was not read
    /// has its row follow. Asked of any Session read back, since only a
    /// Sidekick's has Subsessions naming it.
    pub(super) async fn reconcile_subsession_rows(&self, sidekick: SessionId) {
        let unread = {
            let mut state = self
                .state
                .lock()
                .expect("Session store lock is not poisoned");
            let mut unread = Vec::new();
            for subsession in state.subsessions_of(sidekick) {
                match state.reconcile_subsession_row(&self.storage, sidekick, subsession) {
                    Ok(Reconciled::Unread) => unread.push(subsession),
                    Ok(Reconciled::Done) => {}
                    Err(error) => tracing::warn!(
                        %sidekick,
                        %subsession,
                        "a Subsession's row was not put right: {error:#}"
                    ),
                }
            }
            unread
        };
        for subsession in unread {
            let reconciled = match self.hydrate_boxed(subsession).await {
                Ok(()) => self.reconcile_subsession_row(sidekick, subsession),
                Err(error) => Err(error.into()),
            };
            if let Err(error) = reconciled {
                tracing::warn!(
                    %sidekick,
                    %subsession,
                    "a Subsession's lost row was not stood again: {error:#}"
                );
            }
        }
    }

    /// Stands the row recording that the Sidekick of `sidekick` began the
    /// Session `session_id` on the Remote `remote`, naming it `title` and
    /// saying it was first asked `prompt`, where its Session is held and has
    /// no such row yet — so a beginning the Remote found again stands none
    /// twice. Its Title follows what the Remote derives while the Remote is
    /// kept in view.
    pub(crate) fn stand_remote_subsession_row(
        &self,
        sidekick: SessionId,
        remote: &str,
        session_id: SessionId,
        title: String,
        prompt: String,
    ) -> anyhow::Result<()> {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        if state.is_deferred(sidekick) {
            return Ok(());
        }
        let Some(record) = state.sessions.get(&sidekick) else {
            return Ok(());
        };
        if !record
            .snapshot
            .remote_subsession_rows(remote, session_id)
            .is_empty()
        {
            return Ok(());
        }
        let remote = remote.to_owned();
        state.stand_row(&self.storage, sidekick, move |turn_id| {
            Activity::Subsession {
                id: ActivityId::new(),
                turn_id,
                session_id,
                origin: Some(remote),
                title,
                prompt,
            }
        })
    }

    /// [`SessionStore::hydrate`], boxed: reading a Sidekick's Session back can
    /// read its Subsessions back in turn.
    fn hydrate_boxed(&self, session_id: SessionId) -> BoxFuture<'_, Result<(), StorageError>> {
        Box::pin(self.hydrate(session_id))
    }
}

/// How far reconciling one Subsession's row got.
enum Reconciled {
    /// The row stands as it should, or there is nothing to put right now.
    Done,
    /// The row is missing, and the Subsession must be read back for what it
    /// was first asked before it can stand.
    Unread,
}

impl SessionStoreState {
    /// The Sessions, held or not yet read, that remember the Sidekick's
    /// Session `sidekick` as the one that began them.
    pub(super) fn subsessions_of(&self, sidekick: SessionId) -> Vec<SessionId> {
        self.sessions
            .iter()
            .filter(|(_, record)| record.summary.session.sidekick() == Some(sidekick))
            .map(|(session_id, _)| *session_id)
            .collect()
    }

    /// Stands the row recording that the Sidekick of `sidekick` began
    /// `subsession`, naming the Subsession by the Title it has now and saying
    /// what it was first asked: in the Turn the Sidekick works in, or in a
    /// Continuation begun to hold it where none works, as a brokered
    /// Subagent's row stands (see [`SessionStoreState::stand_row`]). Both
    /// Sessions must be held.
    pub(super) fn stand_subsession_row(
        &mut self,
        storage: &StorageSink,
        sidekick: SessionId,
        subsession: SessionId,
    ) -> anyhow::Result<()> {
        let row = self.subsession_row(sidekick, subsession)?;
        self.stand_row(storage, sidekick, row)
    }

    /// Stands again the row a stop lost between `subsession` being begun and
    /// its row being written: in the Turn the Sidekick works in, or else its
    /// latest, where it was begun or after it, and as a change the Server
    /// derived rather than work, so putting it right moves neither how the
    /// Sidekick's Session stands nor when it last moved — no Turn is begun to
    /// hold it, and reading the Session back changes nothing else a reader
    /// sees. A Sidekick's Session with no Turn at all stands it as a beginning
    /// would.
    fn restore_subsession_row(
        &mut self,
        storage: &StorageSink,
        sidekick: SessionId,
        subsession: SessionId,
    ) -> anyhow::Result<()> {
        let row = self.subsession_row(sidekick, subsession)?;
        let holder = &self
            .sessions
            .get(&sidekick)
            .ok_or_else(|| anyhow::anyhow!("the Sidekick's Session does not exist"))?
            .snapshot;
        let Some(turn_id) =
            active_turn_id(holder)?.or_else(|| holder.turns.last().map(|turn| turn.id))
        else {
            return self.stand_row(storage, sidekick, row);
        };
        self.sessions
            .get_mut(&sidekick)
            .expect("the Sidekick's Session is held")
            .commit_derived(
                storage,
                sidekick,
                vec![SessionChange::ActivityAdded {
                    activity: row(turn_id),
                }],
            )?;
        Ok(())
    }

    /// The row leading into `subsession` from the Transcript of `sidekick`,
    /// for the Turn it is to stand in: naming the Subsession by the Title it
    /// has now and saying what it was first asked. Both Sessions must be
    /// held.
    fn subsession_row(
        &self,
        sidekick: SessionId,
        subsession: SessionId,
    ) -> anyhow::Result<impl FnOnce(TurnId) -> Activity + use<>> {
        if self.is_deferred(sidekick) || self.is_deferred(subsession) {
            anyhow::bail!("a Sidekick's row stands only between Sessions that are held");
        }
        let snapshot = &self
            .sessions
            .get(&subsession)
            .ok_or_else(|| anyhow::anyhow!("the Subsession does not exist"))?
            .snapshot;
        let title = snapshot.title.clone();
        let prompt = snapshot
            .prompts
            .first()
            .map(|prompt| prompt.text.clone())
            .unwrap_or_default();
        Ok(move |turn_id| Activity::Subsession {
            id: ActivityId::new(),
            turn_id,
            session_id: subsession,
            origin: None,
            title,
            prompt,
        })
    }

    /// Puts right the row leading into `subsession` in the Transcript of
    /// `sidekick`, where both are held: has every row leading into it name its
    /// Title as it is now, and stands one where none does. A Sidekick's
    /// Session not held now has nothing to put right until it is read back.
    fn reconcile_subsession_row(
        &mut self,
        storage: &StorageSink,
        sidekick: SessionId,
        subsession: SessionId,
    ) -> anyhow::Result<Reconciled> {
        if self.is_deferred(sidekick) || !self.sessions.contains_key(&sidekick) {
            return Ok(Reconciled::Done);
        }
        let Some(title) = self
            .sessions
            .get(&subsession)
            .map(|record| record.summary.title.clone())
        else {
            return Ok(Reconciled::Done);
        };
        let rows = self.sessions[&sidekick]
            .snapshot
            .activities
            .iter()
            .filter_map(|activity| match activity {
                Activity::Subsession {
                    id,
                    session_id,
                    origin: None,
                    title,
                    ..
                } if *session_id == subsession => Some((*id, title.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        if rows.is_empty() {
            if self.is_deferred(subsession) {
                return Ok(Reconciled::Unread);
            }
            self.restore_subsession_row(storage, sidekick, subsession)?;
            return Ok(Reconciled::Done);
        }
        let changes = rows
            .into_iter()
            .filter(|(_, named)| *named != title)
            .map(|(activity_id, _)| SessionChange::SubsessionTitleChanged {
                activity_id,
                title: title.clone(),
            })
            .collect::<Vec<_>>();
        if !changes.is_empty() {
            self.sessions
                .get_mut(&sidekick)
                .expect("the Sidekick's Session is held")
                .commit_derived(storage, sidekick, changes)?;
        }
        Ok(Reconciled::Done)
    }

    /// Carries `title`, which the Remote `remote` says its Session
    /// `session_id` has now, onto every row leading into it in the
    /// Transcript of each Sidekick that began it there, where that
    /// Sidekick's Session is held.
    pub(super) fn follow_remote_subsession_title(
        &mut self,
        storage: &StorageSink,
        sidekicks: &[SessionId],
        remote: &str,
        session_id: SessionId,
        title: &str,
    ) {
        for sidekick in sidekicks {
            if self.is_deferred(*sidekick) {
                continue;
            }
            let Some(record) = self.sessions.get_mut(sidekick) else {
                continue;
            };
            let changes = record
                .snapshot
                .remote_subsession_rows(remote, session_id)
                .into_iter()
                .filter(|(_, named)| *named != title)
                .map(|(activity_id, _)| SessionChange::SubsessionTitleChanged {
                    activity_id,
                    title: title.to_owned(),
                })
                .collect::<Vec<_>>();
            if changes.is_empty() {
                continue;
            }
            if let Err(error) = record.commit_derived(storage, *sidekick, changes) {
                tracing::warn!(
                    %sidekick,
                    %session_id,
                    "a Remote Subsession's row did not follow its Title: {error:#}"
                );
            }
        }
    }

    /// Carries the Title `subsession` has now onto every row in its
    /// Sidekick's Transcript that leads into it, where `subsession` is a
    /// Subsession and its Sidekick's Session is held; one not held follows
    /// when it is next read back.
    pub(super) fn follow_subsession_title(&mut self, storage: &StorageSink, subsession: SessionId) {
        let Some(sidekick) = self
            .sessions
            .get(&subsession)
            .and_then(|record| record.snapshot.session.sidekick())
        else {
            return;
        };
        if let Err(error) = self.reconcile_subsession_row(storage, sidekick, subsession) {
            tracing::warn!(
                %subsession,
                %sidekick,
                "a Subsession's row did not follow its Title: {error:#}"
            );
        }
    }
}

impl crate::protocol::SessionSnapshot {
    /// The rows of this Transcript leading into the Session `session_id` of
    /// the Remote `remote`, by their identity and the Title each names.
    fn remote_subsession_rows(
        &self,
        remote: &str,
        session_id: SessionId,
    ) -> Vec<(ActivityId, String)> {
        self.activities
            .iter()
            .filter_map(|activity| match activity {
                Activity::Subsession {
                    id,
                    session_id: led_into,
                    origin: Some(origin),
                    title,
                    ..
                } if *led_into == session_id && origin == remote => Some((*id, title.clone())),
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::{
        protocol::{
            Author, CreateSessionRequest, ExecutionDirectory, InitialPrompt, PromptId,
            ResolvedWorkspace,
        },
        sessions::{CreateSessionError, StoreOutcome},
        storage::{RestoredSessions, StorageRepository, StorageWriter},
    };

    /// A first Prompt asking `text` of a Session in `workspace`.
    fn beginning(workspace: &Path, text: &str) -> CreateSessionRequest {
        CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: ExecutionDirectory {
                path: workspace.to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: text.to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        }
    }

    fn sidekick(session_id: SessionId) -> Author {
        Author::Sidekick {
            session_id,
            title: "Plan the work".to_owned(),
        }
    }

    async fn empty_store(directory: &Path) -> (StorageWriter, SessionStore) {
        let repository = StorageRepository::open(directory).await.unwrap();
        let (writer, sink) = StorageWriter::spawn(repository, &[]);
        let store = SessionStore::new(
            RestoredSessions::default(),
            sink,
            Vec::new(),
            Default::default(),
        );
        (writer, store)
    }

    fn rows(store: &SessionStore, session_id: SessionId) -> Vec<(SessionId, String, String)> {
        store
            .subscribe(session_id)
            .expect("the Session is held")
            .snapshot
            .activities
            .into_iter()
            .filter_map(|activity| match activity {
                Activity::Subsession {
                    session_id,
                    title,
                    prompt,
                    ..
                } => Some((session_id, title, prompt)),
                _ => None,
            })
            .collect()
    }

    /// No reader ever finds a Subsession its Sidekick's Transcript does not
    /// lead into: the row stands in the step that begins it.
    #[tokio::test]
    async fn a_subsession_and_its_row_are_begun_in_one_step() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::paths::canonical(directory.path()).unwrap();
        let (writer, store) = empty_store(&workspace).await;
        let StoreOutcome::Created(sidekick_snapshot) = store
            .create(beginning(&workspace, "Plan the work"))
            .unwrap()
        else {
            panic!("the Sidekick's Session is begun afresh");
        };
        let sidekick_id = sidekick_snapshot.session.id;

        let StoreOutcome::Created(subsession) = store
            .create_in(
                beginning(&workspace, "Fix the flaky login test"),
                ResolvedWorkspace::directory(workspace.clone()),
                Vec::new(),
                None,
                Some(sidekick(sidekick_id)),
            )
            .unwrap()
        else {
            panic!("the Subsession is begun afresh");
        };
        assert_eq!(
            rows(&store, sidekick_id),
            [(
                subsession.session.id,
                "Fix the flaky login test".to_owned(),
                "Fix the flaky login test".to_owned()
            )],
            "the Sidekick's Transcript leads into the Subsession the moment it is begun"
        );

        writer.shutdown().await.unwrap();
    }

    /// A Sidekick whose own Session is gone has no Transcript to lead into
    /// what it begins, so nothing is begun.
    #[tokio::test]
    async fn nothing_is_begun_for_a_sidekick_whose_session_is_gone() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = crate::paths::canonical(directory.path()).unwrap();
        let (writer, store) = empty_store(&workspace).await;

        let refused = store.create_in(
            beginning(&workspace, "Fix the flaky login test"),
            ResolvedWorkspace::directory(workspace.clone()),
            Vec::new(),
            None,
            Some(sidekick(SessionId::new())),
        );
        assert!(
            matches!(refused, Err(CreateSessionError::AuthorGone)),
            "{refused:?}"
        );
        assert!(store.list(None).is_empty(), "no Session was begun");

        writer.shutdown().await.unwrap();
    }
}
