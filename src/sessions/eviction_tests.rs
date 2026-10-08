//! A tree nothing uses is evicted to the deferred state and read back as it
//! was; one anything holds stays.

use std::{
    path::Path,
    time::{Duration, Instant},
};

use diesel::{Connection, SqliteConnection, connection::SimpleConnection};

use super::{
    DeliveredTurnStatus, ProviderActors, ProviderTurnOutcome, Released, SessionStore, StoreOutcome,
};
use crate::{
    protocol::{
        AdmitPromptRequest, AttachmentDescriptor, CreateSessionRequest, InitialPrompt, Message,
        MessageId, MessageRole, MessageStatus, PromptDelivery, PromptId, SessionChange, SessionId,
        SessionListItem, SessionSnapshot, TurnId,
    },
    provider::{ProviderSubagentId, ProviderWatchId},
    storage::{StorageRepository, StorageWriter, StoredSubagentIdentity},
};

/// No Provider actor runs for any Session here.
struct NoActors;

impl ProviderActors for NoActors {
    fn release(&self, _owners: &[SessionId]) -> Released {
        Box::pin(async {})
    }
}

/// A store over storage the way the Server opens one, so a tree it evicts
/// can be read back, with the writer it saves through.
async fn store(data_dir: &Path) -> (StorageRepository, StorageWriter, SessionStore) {
    store_over(StorageRepository::open(data_dir).await.unwrap()).await
}

/// A store over `repository`, with the writer it saves through.
async fn store_over(
    repository: StorageRepository,
) -> (StorageRepository, StorageWriter, SessionStore) {
    let restored = repository.load_sessions().await.unwrap();
    let (writer, sink) = StorageWriter::spawn(repository.clone());
    let store = SessionStore::new(restored, sink, Vec::new(), Default::default());
    (repository, writer, store)
}

/// A Session begun by its first Prompt, at work on the Turn it began.
fn working(store: &SessionStore, workspace: &Path) -> (SessionId, TurnId) {
    let StoreOutcome::Created(snapshot) = store
        .create(CreateSessionRequest {
            session_id: None,
            preparation_id: None,
            agent_selection: None,
            execution_directory: crate::protocol::ExecutionDirectory {
                path: workspace.to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Map the storage writer".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .unwrap()
    else {
        panic!("a fresh Prompt creates a Session");
    };
    let session_id = snapshot.session.id;
    let turn_id = store
        .deliver_prompt(
            session_id,
            snapshot.prompts[0].id,
            None,
            DeliveredTurnStatus::Active,
        )
        .unwrap()
        .expect("the Prompt was still owed a Turn")
        .turn_id;
    (session_id, turn_id)
}

fn reply(turn_id: TurnId, content: &str) -> SessionChange {
    SessionChange::MessageAdded {
        message: Message {
            id: MessageId::new(),
            turn_id,
            role: MessageRole::Agent,
            status: MessageStatus::Completed,
            content: content.to_owned(),
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
            truncated: false,
            author: None,
        },
    }
}

fn settle(store: &SessionStore, session_id: SessionId, turn_id: TurnId) {
    store
        .finish_provider_turn(
            session_id,
            turn_id,
            ProviderTurnOutcome::Completed {
                trailing_output: Default::default(),
            },
        )
        .unwrap();
}

/// A settled tree: a top-level Session whose Turn spawned a native Subagent,
/// each having written a Message, every Turn settled.
fn settled_tree(store: &SessionStore, workspace: &Path) -> (SessionId, SessionId) {
    let (root_id, root_turn) = working(store, workspace);
    let child = store
        .create_subagent(
            root_id,
            StoredSubagentIdentity {
                provider: crate::protocol::ProviderId::new("codex"),
                subagent_id: ProviderSubagentId::new("explorer"),
            },
            "explorer",
            "map the seams",
            None,
        )
        .unwrap();
    store
        .publish(child.session_id, vec![reply(child.turn_id, "Two seams.")])
        .unwrap();
    settle(store, child.session_id, child.turn_id);
    store
        .publish(root_id, vec![reply(root_turn, "The writer holds a copy.")])
        .unwrap();
    settle(store, root_id, root_turn);
    (root_id, child.session_id)
}

fn snapshot(store: &SessionStore, session_id: SessionId) -> SessionSnapshot {
    store
        .snapshot(session_id)
        .expect("the Session's history is held")
}

fn database(data_dir: &Path) -> SqliteConnection {
    let mut connection =
        SqliteConnection::establish(data_dir.join("suru.db").to_str().unwrap()).unwrap();
    connection
        .batch_execute("PRAGMA busy_timeout = 5000;")
        .unwrap();
    connection
}

#[tokio::test]
async fn a_settled_tree_nothing_holds_is_evicted_once_idle_and_reads_back_as_it_was() {
    let data_dir = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (_repository, writer, store) = store(data_dir.path()).await;
    let (root_id, child_id) = settled_tree(&store, workspace.path());
    // A change no Turn boundary saved, which eviction lands first.
    store
        .publish(
            root_id,
            vec![SessionChange::TitleChanged {
                title: "Evicting idle trees".to_owned(),
                icon: None,
            }],
        )
        .unwrap();

    assert!(
        store
            .evict_idle_trees(Duration::from_secs(60 * 60), &NoActors)
            .await
            .is_empty(),
        "a tree just used is not idle yet"
    );
    let (root, child) = (snapshot(&store, root_id), snapshot(&store, child_id));

    assert_eq!(
        store.evict_idle_trees(Duration::ZERO, &NoActors).await,
        vec![root_id]
    );
    assert!(store.snapshot(root_id).is_none(), "its history is evicted");
    assert!(
        store.snapshot(child_id).is_none(),
        "a tree is evicted whole"
    );
    assert!(
        store.list(None).iter().any(|item| matches!(
            item,
            SessionListItem::Readable(summary)
                if summary.session.id == root_id && summary.title == "Evicting idle trees"
        )),
        "it is listed as it was"
    );

    // Reading any Session of it reads the whole tree back.
    store.hydrate(child_id).await.unwrap();
    assert_eq!(snapshot(&store, root_id), root);
    assert_eq!(snapshot(&store, child_id), child);

    // And it is evicted again once nothing uses it.
    assert_eq!(
        store.evict_idle_trees(Duration::ZERO, &NoActors).await,
        vec![root_id]
    );
    store.hydrate(root_id).await.unwrap();
    assert_eq!(snapshot(&store, root_id), root);
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_tree_anything_holds_stays_until_nothing_does() {
    let data_dir = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (_repository, writer, store) = store(data_dir.path()).await;

    // Work running in it.
    let (root_id, turn_id) = working(&store, workspace.path());
    assert!(
        store
            .evict_idle_trees(Duration::ZERO, &NoActors)
            .await
            .is_empty()
    );

    // A Prompt owed a Turn by this process, which reading the history back
    // would withdraw.
    let queued = PromptId::new();
    store
        .admit(
            root_id,
            AdmitPromptRequest {
                delivery: PromptDelivery::Queue,
                prompt: InitialPrompt {
                    id: queued,
                    text: "And the tests".to_owned(),
                    skill_invocations: Vec::new(),
                    attachments: Vec::new(),
                },
            },
            Vec::new(),
            None,
        )
        .unwrap();
    settle(&store, root_id, turn_id);
    assert!(
        store
            .evict_idle_trees(Duration::ZERO, &NoActors)
            .await
            .is_empty()
    );
    let next_turn = store
        .deliver_prompt(root_id, queued, None, DeliveredTurnStatus::Active)
        .unwrap()
        .expect("the Prompt was still owed a Turn")
        .turn_id;
    settle(&store, root_id, next_turn);

    // A reader subscribed to it.
    let feed = store.subscribe(root_id).unwrap();
    assert!(
        store
            .evict_idle_trees(Duration::ZERO, &NoActors)
            .await
            .is_empty()
    );
    drop(feed);

    // A reader following its tree.
    let tree = store.subscribe_subagent_tree(root_id).unwrap();
    assert!(
        store
            .evict_idle_trees(Duration::ZERO, &NoActors)
            .await
            .is_empty()
    );
    drop(tree);

    // A Watch its Agent left running, which keeps it Monitoring.
    let watch = ProviderWatchId::new("build");
    store
        .start_watch(root_id, watch.clone(), "the build".to_owned())
        .unwrap();
    assert!(
        store
            .evict_idle_trees(Duration::ZERO, &NoActors)
            .await
            .is_empty()
    );
    store.settle_watch(root_id, &watch, false).unwrap();

    // What it owes storage, while storage refuses it.
    database(data_dir.path())
        .batch_execute(
            "CREATE TRIGGER refuse_saves BEFORE INSERT ON sessions \
             BEGIN SELECT RAISE(ROLLBACK, 'database or disk is full'); END;",
        )
        .unwrap();
    store
        .publish(
            root_id,
            vec![SessionChange::TitleChanged {
                title: "Unsaved".to_owned(),
                icon: None,
            }],
        )
        .unwrap();
    assert!(
        store
            .evict_idle_trees(Duration::ZERO, &NoActors)
            .await
            .is_empty()
    );
    assert!(store.snapshot(root_id).is_some(), "its history stays held");
    database(data_dir.path())
        .batch_execute("DROP TRIGGER refuse_saves;")
        .unwrap();

    // Once nothing holds it, it goes.
    let held = snapshot(&store, root_id);
    assert_eq!(
        store.evict_idle_trees(Duration::ZERO, &NoActors).await,
        vec![root_id]
    );
    store.hydrate(root_id).await.unwrap();
    assert_eq!(snapshot(&store, root_id), held);
    assert_eq!(snapshot(&store, root_id).title, "Unsaved");
    writer.shutdown().await.unwrap();
}

/// However the test is scheduled, a tree evicted had gone unused for the
/// whole idle period since whatever last used it.
#[tokio::test]
async fn a_tree_is_evicted_only_once_idle_for_the_whole_period() {
    let data_dir = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (_repository, writer, store) = store(data_dir.path()).await;
    let idle = Duration::from_millis(50);

    let before = Instant::now();
    let (root_id, _) = settled_tree(&store, workspace.path());
    let evicted = store.evict_idle_trees(idle, &NoActors).await;
    assert!(evicted.is_empty() || before.elapsed() >= idle);
    if !evicted.is_empty() {
        store.hydrate(root_id).await.unwrap();
    }

    // An access entering the hydration boundary uses it again.
    tokio::time::sleep(idle).await;
    let before = Instant::now();
    store.hydrate(root_id).await.unwrap();
    let evicted = store.evict_idle_trees(idle, &NoActors).await;
    assert!(evicted.is_empty() || before.elapsed() >= idle);
    if !evicted.is_empty() {
        store.hydrate(root_id).await.unwrap();
    }

    tokio::time::sleep(idle).await;
    assert_eq!(store.evict_idle_trees(idle, &NoActors).await, vec![root_id]);
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_store_sweeps_idle_trees_at_the_cadence_it_is_given() {
    let data_dir = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (_repository, writer, store) = store(data_dir.path()).await;
    let (root_id, _) = settled_tree(&store, workspace.path());
    let (stop, shutdown) = tokio::sync::watch::channel(false);

    store.evict_idle_sessions(
        Duration::from_millis(1),
        Duration::from_millis(5),
        std::sync::Arc::new(NoActors),
        shutdown,
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        while store.snapshot(root_id).is_some() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the sweep evicts the idle tree");
    stop.send_replace(true);
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_session_owing_a_save_is_deleted_with_nothing_of_it_left_stored() {
    let data_dir = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (repository, writer, store) = store(data_dir.path()).await;
    let (root_id, child_id) = settled_tree(&store, workspace.path());
    store
        .publish(
            root_id,
            vec![SessionChange::TitleChanged {
                title: "Doomed".to_owned(),
                icon: None,
            }],
        )
        .unwrap();

    store.delete(root_id).unwrap();
    writer.shutdown().await.unwrap();

    assert!(repository.session(root_id).await.unwrap().is_none());
    assert!(repository.session(child_id).await.unwrap().is_none());
    assert!(
        repository
            .load_sessions()
            .await
            .unwrap()
            .readable
            .is_empty(),
        "no save of it lands after its deletion"
    );
}

/// The Provider actors a release was asked to stop, and what each release
/// does before it finishes.
struct RecordingActors {
    released: std::sync::Mutex<Vec<SessionId>>,
    /// A commit the stopping actor still lands before it has stopped.
    last_word: Option<(SessionStore, SessionId)>,
}

impl ProviderActors for RecordingActors {
    fn release(&self, owners: &[SessionId]) -> Released {
        self.released.lock().unwrap().extend_from_slice(owners);
        let last_word = self.last_word.clone();
        Box::pin(async move {
            if let Some((store, session_id)) = last_word {
                store
                    .publish(
                        session_id,
                        vec![SessionChange::TitleChanged {
                            title: "A last word".to_owned(),
                            icon: None,
                        }],
                    )
                    .unwrap();
            }
        })
    }
}

#[tokio::test]
async fn a_tree_is_evicted_only_once_its_provider_actors_have_stopped_saying_anything() {
    let data_dir = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (_repository, writer, store) = store(data_dir.path()).await;
    let (root_id, child_id) = settled_tree(&store, workspace.path());

    // What a stopping actor says lands on the history it holds, and the tree
    // it used stays held: nothing it says is dropped on a deferred record.
    let speaking = RecordingActors {
        released: Default::default(),
        last_word: Some((store.clone(), root_id)),
    };
    assert!(
        store
            .evict_idle_trees(Duration::ZERO, &speaking)
            .await
            .is_empty()
    );
    assert_eq!(snapshot(&store, root_id).title, "A last word");
    assert_eq!(
        *speaking.released.lock().unwrap(),
        vec![root_id],
        "only a Session owning its actor has one to stop; a native Subagent rides its root's"
    );

    // Once its actors stop without a word, it goes.
    let quiet = RecordingActors {
        released: Default::default(),
        last_word: None,
    };
    assert_eq!(
        store.evict_idle_trees(Duration::ZERO, &quiet).await,
        vec![root_id]
    );
    assert_eq!(*quiet.released.lock().unwrap(), vec![root_id]);
    assert!(store.snapshot(child_id).is_none());
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_tree_that_cannot_be_encoded_is_kept_and_stopping_says_so() {
    let data_dir = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (_repository, writer, store) = store(data_dir.path()).await;
    let (root_id, _) = settled_tree(&store, workspace.path());
    // A moment no stored column can hold, which the Session now owes
    // storage.
    {
        let mut state = store.state.lock().unwrap();
        let record = state.sessions.get_mut(&root_id).unwrap();
        record.summary.created_at = crate::protocol::SessionTimestamp(u64::MAX);
        record.unsaved.note_summary(None);
    }

    assert!(
        store
            .evict_idle_trees(Duration::ZERO, &NoActors)
            .await
            .is_empty(),
        "its history is the only copy of what storage lacks"
    );
    assert!(store.snapshot(root_id).is_some());
    assert!(
        writer.shutdown().await.is_err(),
        "stopping with a Session never saved fails"
    );
}

#[tokio::test]
async fn retrying_a_creation_after_its_tree_was_evicted_finds_the_session_it_made() {
    let data_dir = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (_repository, writer, store) = store(data_dir.path()).await;
    let request = CreateSessionRequest {
        session_id: None,
        preparation_id: None,
        agent_selection: Some(crate::protocol::AgentSelection {
            provider: crate::protocol::ProviderId::new("codex"),
            model: crate::protocol::ModelId::new("gpt-5.5"),
            options: Vec::new(),
        }),
        execution_directory: crate::protocol::ExecutionDirectory {
            path: workspace.path().to_owned(),
        },
        prompt: InitialPrompt {
            id: PromptId::new(),
            text: "Map the storage writer".to_owned(),
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
        },
    };
    let StoreOutcome::Created(created) = store.create(request.clone()).unwrap() else {
        panic!("a fresh Prompt creates a Session");
    };
    let session_id = created.session.id;
    let turn_id = store
        .deliver_prompt(
            session_id,
            request.prompt.id,
            None,
            DeliveredTurnStatus::Active,
        )
        .unwrap()
        .expect("the Prompt was still owed a Turn")
        .turn_id;
    settle(&store, session_id, turn_id);
    assert_eq!(
        store.evict_idle_trees(Duration::ZERO, &NoActors).await,
        vec![session_id]
    );

    // The response was lost, and the client asks again, as admission does.
    store.hydrate_prompt_owner(request.prompt.id).await.unwrap();
    assert!(
        matches!(
            store.existing_creation(&request),
            Ok(Some(existing)) if existing.session.id == session_id
        ),
        "the retry is answered with the Session it made"
    );
    assert!(
        matches!(
            store.create(request),
            Ok(StoreOutcome::Existing(existing)) if existing.session.id == session_id
        ),
        "and creating again finds it"
    );
    writer.shutdown().await.unwrap();
}

/// An Attachment `bytes`, bound in a Prompt of its own added to the settled
/// Session `session_id` with its description.
fn bind(store: &SessionStore, session_id: SessionId, attachment: &AttachmentDescriptor) {
    let snapshot = snapshot(store, session_id);
    let label = "[Image 1]";
    store
        .publish(
            session_id,
            vec![
                SessionChange::AttachmentsDescribed {
                    attachments: vec![attachment.clone()],
                },
                SessionChange::PromptAdded {
                    prompt: crate::protocol::Prompt {
                        id: PromptId::new(),
                        text: label.to_owned(),
                        skill_invocations: Vec::new(),
                        attachments: vec![crate::protocol::AttachmentBinding {
                            attachment_id: attachment.id.clone(),
                            label: label.to_owned(),
                            span: crate::protocol::TextSpan {
                                start: 0,
                                end: label.len() as u32,
                            },
                        }],
                        delivery: PromptDelivery::Queue,
                        admission_order: crate::protocol::PromptOrder(
                            snapshot.prompts.len() as u64 + 1,
                        ),
                        status: crate::protocol::PromptStatus::Cancelled,
                        withdrawal: None,
                        author: None,
                        taken: None,
                    },
                },
            ],
        )
        .unwrap();
}

/// Lands what `session_id` owes storage now, as saving its Resume State does.
fn save_now(store: &SessionStore, session_id: SessionId) {
    store
        .save_resume_state(
            session_id,
            crate::protocol::ProviderId::new("codex"),
            crate::provider::ProviderResumeState::new(serde_json::json!({ "thread": "t" })),
        )
        .unwrap();
}

/// Session B binds an Attachment only Session A's stored rows join, and B's
/// save finds storage out of step with B, so the save that would have joined
/// B is let go until B is written whole. Deleting A must not reclaim the
/// Attachment before then.
#[tokio::test]
async fn a_deletion_waits_for_a_session_storage_is_out_of_step_with_to_be_written_whole() {
    let data_dir = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    // The Attachment's grace runs on a clock only the test moves, so no
    // idle sweep reclaims it before the bindings that matter are made.
    let grace = Duration::from_secs(60 * 60);
    let (clock, hand) = crate::clock::ServerClock::manual();
    let repository = StorageRepository::open(data_dir.path())
        .await
        .unwrap()
        .with_attachment_grace(grace)
        .with_clock(clock);
    let attachments = crate::attachments::AttachmentStore::new(repository.clone());
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image::RgbaImage::new(4, 4))
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    let Ok(uploaded) = attachments.upload(png.into_inner()).await else {
        panic!("the fixture uploads");
    };
    let attachment = uploaded.descriptor;
    let restored = repository.load_sessions().await.unwrap();
    let (writer, sink) = StorageWriter::spawn(repository.clone());
    let store = SessionStore::new(restored, sink, Vec::new(), Default::default());

    let (a_id, a_turn) = working(&store, workspace.path());
    settle(&store, a_id, a_turn);
    bind(&store, a_id, &attachment);
    save_now(&store, a_id);

    let (b_id, b_turn) = working(&store, workspace.path());
    settle(&store, b_id, b_turn);
    database(data_dir.path())
        .batch_execute(&format!("DELETE FROM turns WHERE id = '{b_turn}';"))
        .unwrap();
    bind(&store, b_id, &attachment);
    store
        .publish(
            b_id,
            vec![SessionChange::TurnOutputObserved {
                turn_id: b_turn,
                observed_at: crate::protocol::SessionTimestamp(1),
            }],
        )
        .unwrap();
    save_now(&store, b_id);

    // Past the grace period the Attachment was uploaded at, with A's binding
    // stored and B's yet to be.
    hand.advance(grace + Duration::from_secs(1));
    store.delete(a_id).unwrap();

    assert!(
        attachments
            .fetch(attachment.id.clone())
            .await
            .unwrap()
            .is_some(),
        "the Attachment B binds outlives A's deletion"
    );
    writer.shutdown().await.unwrap();
    let stored = repository.session(b_id).await.unwrap().unwrap();
    assert_eq!(stored.snapshot.prompts.len(), 2, "B is stored whole");
    assert!(attachments.fetch(attachment.id).await.unwrap().is_some());
}

/// A Session storage is out of step with whose deletion fails stays held,
/// still owed whole: it is kept from eviction, and written whole once storage
/// takes it, rather than its dropped save being forgotten.
#[tokio::test]
async fn a_session_out_of_step_whose_deletion_fails_is_still_written_whole() {
    let data_dir = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    // The writer takes no idle tick before the assertions below are made, so
    // none finds the store quiet enough to take the whole rewrite first: only
    // the deletion's failure is left to account for it.
    let (repository, writer, store) = store_over(
        StorageRepository::open(data_dir.path())
            .await
            .unwrap()
            .with_idle_flush_delay(Duration::from_secs(60 * 60)),
    )
    .await;
    let (session_id, turn_id) = working(&store, workspace.path());
    settle(&store, session_id, turn_id);
    database(data_dir.path())
        .batch_execute(&format!("DELETE FROM turns WHERE id = '{turn_id}';"))
        .unwrap();
    store
        .publish(
            session_id,
            vec![SessionChange::TurnOutputObserved {
                turn_id,
                observed_at: crate::protocol::SessionTimestamp(1),
            }],
        )
        .unwrap();
    // The save building on the lost row is let go, the Session owed whole.
    save_now(&store, session_id);

    database(data_dir.path())
        .batch_execute(
            "CREATE TRIGGER refuse_deletions BEFORE DELETE ON sessions \
             BEGIN SELECT RAISE(ABORT, 'refused'); END;",
        )
        .unwrap();
    assert!(
        store.delete(session_id).is_err(),
        "storage refuses the deletion"
    );
    assert!(
        store
            .evict_idle_trees(Duration::ZERO, &NoActors)
            .await
            .is_empty(),
        "its history is the only complete copy, so it is not evicted"
    );
    database(data_dir.path())
        .batch_execute("DROP TRIGGER refuse_deletions;")
        .unwrap();

    let held = snapshot(&store, session_id);
    writer.shutdown().await.unwrap();
    let stored = repository.session(session_id).await.unwrap().unwrap();
    assert_eq!(stored.snapshot.turns, held.turns, "it is written whole");
    assert_eq!(stored.snapshot.messages, held.messages);
}
