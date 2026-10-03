//! The `wait_subagents` calls each Session's Agent has open, from which a
//! reader learns that the Session's Working is spent waiting on its
//! Subagents rather than on work of its own (ADR 0035).
//!
//! A wait opens and closes nothing in the Transcript — every Provider's
//! projection absorbs the Broker's calls — and moves no Turn, so it is kept in
//! memory beside the Session its call is attributed to, and told to that
//! Session's subscribers as the Turn it waits in. Each wait is held by a
//! [`SubagentWait`] that forgets it when dropped, so however the call ends —
//! answered, refused, cancelled with its client, or dropped with a stopping
//! Server — the Session stops reading as waiting. Nothing here outlives the
//! process, since no call does.
//!
//! A wait is spent in the Turn working when it began. A Provider may begin a
//! Continuation of its own accord and call the Broker from it before Suru has
//! heard the Continuation begin — the call and the Provider's events reach
//! Suru by different roads — so a wait finding no Turn working is spent in
//! the next Turn to begin while it is open. A wait that began in a Turn keeps
//! it, so one lingering after that Turn settled never marks the next.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::protocol::{SessionChange, SessionId, TurnId};

use super::{SessionStore, projection::active_turn_id};

/// One `wait_subagents` call still waiting, and the Turn it is spent in:
/// the one working when it began, or — where none was — the next to begin,
/// `None` until one does.
#[derive(Debug)]
pub(super) struct OpenWait {
    id: u64,
    turn_id: Option<TurnId>,
}

/// A `wait_subagents` call waiting for the Agent of one Session, which reads
/// as waiting on its Subagents for as long as this is held.
#[must_use = "a wait reads as open only while its guard is held"]
pub(crate) struct SubagentWait {
    sessions: SessionStore,
    session_id: SessionId,
    id: u64,
}

impl Drop for SubagentWait {
    fn drop(&mut self) {
        self.sessions.end_subagent_wait(self.session_id, self.id);
    }
}

impl SessionStore {
    /// Records that the Agent of `session_id` has begun waiting on its
    /// Subagents in the Turn it is working — or, with none working yet, in
    /// the next to begin — and tells the Session's readers so; the returned
    /// guard ends the wait when dropped. A Session whose history is not in
    /// hand has no Agent calling, so records none.
    pub(crate) fn begin_subagent_wait(&self, session_id: SessionId) -> Option<SubagentWait> {
        static NEXT_WAIT: AtomicU64 = AtomicU64::new(0);
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        if state.is_deferred(session_id) {
            return None;
        }
        let record = state.sessions.get_mut(&session_id)?;
        let turn_id = active_turn_id(&record.snapshot).ok().flatten();
        let id = NEXT_WAIT.fetch_add(1, Ordering::Relaxed);
        record.subagent_waits.push(OpenWait { id, turn_id });
        record.announce_subagent_waits(&self.storage, session_id);
        Some(SubagentWait {
            sessions: self.clone(),
            session_id,
            id,
        })
    }

    /// Forgets the wait `id`, however it ended, and tells the Session's
    /// readers where that leaves it: waiting still in the Turn of the latest
    /// wait left open, or not at all. A Session deleted meanwhile has no one
    /// left to tell.
    fn end_subagent_wait(&self, session_id: SessionId, id: u64) {
        let mut state = self
            .state
            .lock()
            .expect("Session store lock is not poisoned");
        let Some(record) = state.sessions.get_mut(&session_id) else {
            return;
        };
        record.subagent_waits.retain(|wait| wait.id != id);
        record.announce_subagent_waits(&self.storage, session_id);
    }
}

impl super::SessionRecord {
    /// The Turn the Session reads as waiting on its Subagents in: that of the
    /// latest open wait spent in one. A call from a Turn that has since
    /// settled lingers until the Server learns its client has gone, so the
    /// latest wait — spent in the latest Turn — is the one a reader needs.
    fn subagent_wait_turn(&self) -> Option<TurnId> {
        self.subagent_waits
            .iter()
            .rev()
            .find_map(|wait| wait.turn_id)
    }

    /// Spends every open wait still without a Turn in the one the Session —
    /// as the revision being committed leaves it — has working, answering
    /// with where the Session then reads as waiting when any was. A wait is
    /// without a Turn only where none was working as it began, so any Turn
    /// working now began while it was open.
    pub(super) fn attach_subagent_waits(&mut self) -> Option<Option<TurnId>> {
        if self
            .subagent_waits
            .iter()
            .all(|wait| wait.turn_id.is_some())
        {
            return None;
        }
        let working = active_turn_id(&self.snapshot).ok().flatten()?;
        for wait in &mut self.subagent_waits {
            wait.turn_id.get_or_insert(working);
        }
        Some(self.subagent_wait_turn())
    }

    /// Carries [`Self::subagent_wait_turn`] to the Session's snapshot and
    /// subscribers, where it moved.
    fn announce_subagent_waits(&mut self, storage: &crate::storage::StorageSink, id: SessionId) {
        let waiting_on_subagents = self.subagent_wait_turn();
        if self.snapshot.waiting_on_subagents == waiting_on_subagents {
            return;
        }
        if let Err(error) = self.commit_derived(
            storage,
            id,
            vec![SessionChange::SessionWaitingOnSubagentsChanged {
                waiting_on_subagents,
            }],
        ) {
            tracing::warn!(session_id = %id, "A wait on Subagents was not told: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        protocol::{
            AdmitPromptRequest, AgentId, AgentIdentity, AgentSelection, CreateSessionRequest,
            InitialPrompt, ModelId, PromptDelivery, PromptId, ProviderId, SessionChange, SessionId,
            SessionSnapshot, TurnId,
        },
        sessions::{DeliveredTurnStatus, ProviderTurnOutcome, SessionStore, StoreOutcome},
        storage::{StorageRepository, StorageWriter},
    };

    /// A store holding one Session whose first Turn is working, and that
    /// Turn.
    async fn working_session(
        data_dir: &std::path::Path,
        execution_directory: &std::path::Path,
    ) -> (SessionStore, SessionSnapshot, TurnId) {
        let repository = StorageRepository::open(data_dir)
            .await
            .expect("open Session repository");
        let (_writer, storage) = StorageWriter::spawn(repository, &[]);
        let store = SessionStore::new(Default::default(), storage, Vec::new(), Default::default());
        let prompt_id = PromptId::new();
        let StoreOutcome::Created(snapshot) = store
            .create(CreateSessionRequest {
                session_id: None,
                preparation_id: None,
                agent_selection: None,
                execution_directory: crate::protocol::ExecutionDirectory {
                    path: execution_directory.to_owned(),
                },
                prompt: InitialPrompt {
                    id: prompt_id,
                    text: "Split the work between two Subagents".to_owned(),
                    skill_invocations: Vec::new(),
                    attachments: Vec::new(),
                },
            })
            .expect("create Session")
        else {
            panic!("a fresh Prompt creates a Session")
        };
        let delivered = store
            .deliver_prompt(
                snapshot.session.id,
                prompt_id,
                None,
                DeliveredTurnStatus::Active,
            )
            .expect("deliver the Prompt")
            .expect("the Prompt was still owed a Turn");
        let snapshot = store
            .snapshot(snapshot.session.id)
            .expect("the Session exists");
        (store, snapshot, delivered.turn_id)
    }

    fn agent() -> AgentIdentity {
        AgentIdentity {
            agent: AgentId::new("claude-agent"),
            selection: AgentSelection {
                provider: ProviderId::new("claude"),
                model: ModelId::new("claude-opus"),
                options: Vec::new(),
            },
        }
    }

    fn waiting_on_subagents(store: &SessionStore, session_id: SessionId) -> Option<TurnId> {
        store
            .snapshot(session_id)
            .expect("the Session exists")
            .waiting_on_subagents
    }

    #[tokio::test]
    async fn a_session_reads_as_waiting_on_subagents_while_any_wait_it_opened_is_held() {
        let data_dir = tempfile::tempdir().expect("create isolated data directory");
        let execution_directory = tempfile::tempdir().expect("create valid Workspace");
        let (store, snapshot, turn_id) =
            working_session(data_dir.path(), execution_directory.path()).await;
        let session_id = snapshot.session.id;
        let working_since = snapshot.working_since();
        let mut feed = store.subscribe(session_id).expect("subscribe").updates;

        let first = store
            .begin_subagent_wait(session_id)
            .expect("a wait in a working Turn is recorded");
        assert_eq!(waiting_on_subagents(&store, session_id), Some(turn_id));
        assert_eq!(
            feed.try_recv().expect("readers are told").changes,
            vec![SessionChange::SessionWaitingOnSubagentsChanged {
                waiting_on_subagents: Some(turn_id),
            }]
        );

        // Parallel Tool calls may open a second wait beside the first.
        let second = store
            .begin_subagent_wait(session_id)
            .expect("an overlapping wait is recorded");
        assert!(
            feed.try_recv().is_err(),
            "which moves nothing a reader sees"
        );
        drop(first);
        assert_eq!(
            waiting_on_subagents(&store, session_id),
            Some(turn_id),
            "the Session waits while any wait is open"
        );
        assert!(feed.try_recv().is_err());

        drop(second);
        assert_eq!(waiting_on_subagents(&store, session_id), None);
        assert_eq!(
            feed.try_recv().expect("readers are told").changes,
            vec![SessionChange::SessionWaitingOnSubagentsChanged {
                waiting_on_subagents: None,
            }]
        );
        assert_eq!(
            store.snapshot(session_id).and_then(|s| s.working_since()),
            working_since,
            "waiting moves nothing of Working"
        );
    }

    /// A Provider's Continuation and its call to the Broker reach Suru by
    /// different roads, so a wait may arrive before the Turn it is spent in.
    #[tokio::test]
    async fn a_wait_opened_with_no_turn_working_is_spent_in_the_next_to_begin() {
        let data_dir = tempfile::tempdir().expect("create isolated data directory");
        let execution_directory = tempfile::tempdir().expect("create valid Workspace");
        let (store, snapshot, turn_id) =
            working_session(data_dir.path(), execution_directory.path()).await;
        let session_id = snapshot.session.id;
        store
            .finish_provider_turn(
                session_id,
                turn_id,
                ProviderTurnOutcome::Completed {
                    trailing_output: Default::default(),
                },
            )
            .expect("settle the Turn");
        assert!(
            store.begin_subagent_wait(SessionId::new()).is_none(),
            "a Session this Server does not hold has no Agent to wait"
        );

        let early = store
            .begin_subagent_wait(session_id)
            .expect("a wait reaching the Broker before its Turn is recorded");
        assert_eq!(
            waiting_on_subagents(&store, session_id),
            None,
            "with no Turn to wait in yet"
        );
        let mut feed = store.subscribe(session_id).expect("subscribe").updates;
        let continuation = store
            .begin_continuation(session_id, agent())
            .expect("the Provider's Continuation begins");
        assert_eq!(
            waiting_on_subagents(&store, session_id),
            Some(continuation),
            "the wait is spent in the Turn that began while it was open"
        );
        let update = feed.try_recv().expect("readers are told");
        assert!(
            update.changes.iter().any(|change| matches!(
                change,
                SessionChange::TurnAdded { turn } if turn.id == continuation
            )) && update
                .changes
                .contains(&SessionChange::SessionWaitingOnSubagentsChanged {
                    waiting_on_subagents: Some(continuation),
                }),
            "in the revision the Turn begins: {update:?}"
        );
        assert!(
            store
                .snapshot(session_id)
                .expect("the Session exists")
                .only_waiting_on_subagents()
        );

        drop(early);
        assert_eq!(waiting_on_subagents(&store, session_id), None);
    }

    /// A wait from a Turn that has settled lingers until the Server learns
    /// its client has gone; one begun in the next Turn is the one read.
    #[tokio::test]
    async fn the_latest_wait_open_names_the_turn_the_session_waits_in() {
        let data_dir = tempfile::tempdir().expect("create isolated data directory");
        let execution_directory = tempfile::tempdir().expect("create valid Workspace");
        let (store, snapshot, first_turn) =
            working_session(data_dir.path(), execution_directory.path()).await;
        let session_id = snapshot.session.id;
        let lingering = store.begin_subagent_wait(session_id).expect("a wait");
        store
            .finish_provider_turn(
                session_id,
                first_turn,
                ProviderTurnOutcome::Completed {
                    trailing_output: Default::default(),
                },
            )
            .expect("settle the Turn");
        assert_eq!(
            waiting_on_subagents(&store, session_id),
            Some(first_turn),
            "the settled Turn's wait is still open"
        );
        assert!(
            !store
                .snapshot(session_id)
                .expect("the Session exists")
                .only_waiting_on_subagents(),
            "but a Turn that has settled waits on nothing"
        );

        let next_prompt = PromptId::new();
        let StoreOutcome::Created(_) = store
            .admit(
                session_id,
                AdmitPromptRequest {
                    delivery: PromptDelivery::Steer,
                    prompt: InitialPrompt {
                        id: next_prompt,
                        text: "And now the tests".to_owned(),
                        skill_invocations: Vec::new(),
                        attachments: Vec::new(),
                    },
                },
                Vec::new(),
                None,
            )
            .expect("admit the next Prompt")
        else {
            panic!("a fresh Prompt is admitted")
        };
        let next_turn = store
            .deliver_prompt(session_id, next_prompt, None, DeliveredTurnStatus::Active)
            .expect("deliver the next Prompt")
            .expect("the Prompt was owed a Turn")
            .turn_id;
        assert_eq!(
            waiting_on_subagents(&store, session_id),
            Some(first_turn),
            "a wait begun in a Turn keeps it when the next begins"
        );
        assert!(
            !store
                .snapshot(session_id)
                .expect("the Session exists")
                .only_waiting_on_subagents(),
            "so the next Turn reads as working"
        );
        let current = store.begin_subagent_wait(session_id).expect("a wait");
        assert_eq!(waiting_on_subagents(&store, session_id), Some(next_turn));
        drop(lingering);
        assert_eq!(waiting_on_subagents(&store, session_id), Some(next_turn));
        drop(current);
        assert_eq!(waiting_on_subagents(&store, session_id), None);
    }
}
