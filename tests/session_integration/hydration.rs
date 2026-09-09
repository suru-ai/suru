//! Lazy durable history through the server's public Session boundaries.
use crate::server_support::PROGRESS_DEADLINE;
use crate::{failing_provider_support::spawn_with_failing_provider, support};
use eventsource_stream::Eventsource;
use futures_util::{StreamExt, future::join_all};
use suru::{
    protocol::{
        Activity, ActivityId, CreateSessionRequest, InitialPrompt, RuntimeDescriptor,
        SessionCatalogChange, SessionChange, SessionListItem, SessionRevision, SessionSnapshot,
        SessionUpdate,
    },
    server::{RunningServer, ServerConfig},
};
use tokio::time::timeout;

struct History {
    _root: tempfile::TempDir,
    config: ServerConfig,
    snapshots: Vec<SessionSnapshot>,
}

impl History {
    async fn stored(count: usize) -> Self {
        let root = tempfile::tempdir().unwrap();
        let config = ServerConfig::new(root.path(), "lazy-history-test").unwrap();
        let server = spawn_with_failing_provider(config.clone()).await.unwrap();
        let mut snapshots = Vec::new();
        for index in 0..count {
            let created = support::create_session(
                server.descriptor(),
                &CreateSessionRequest {
                    preparation_id: None,
                    agent_selection: None,
                    execution_directory: suru::protocol::ExecutionDirectory {
                        path: root.path().to_owned(),
                    },
                    prompt: InitialPrompt {
                        id: suru::protocol::PromptId::new(),
                        text: format!("Keep durable history {index}"),
                        skill_invocations: Vec::new(),
                    },
                },
            )
            .await;
            snapshots.push(
                support::read_session_at_least_revision(
                    &reqwest::Client::new(),
                    server.descriptor(),
                    created.session.id,
                    SessionRevision(2),
                )
                .await,
            );
        }
        server.shutdown().await.unwrap();
        Self {
            _root: root,
            config,
            snapshots,
        }
    }

    fn database(&self) -> diesel::SqliteConnection {
        use diesel::Connection;
        diesel::SqliteConnection::establish(
            self.config.data_dir().join("suru.db").to_str().unwrap(),
        )
        .unwrap()
    }

    async fn start(&self) -> RunningServer {
        spawn_with_failing_provider(self.config.clone())
            .await
            .unwrap()
    }
}

fn request(
    descriptor: &RuntimeDescriptor,
    method: reqwest::Method,
    path: &str,
) -> reqwest::RequestBuilder {
    reqwest::Client::new()
        .request(method, format!("{}{path}", descriptor.base_url))
        .bearer_auth(&descriptor.token)
}

async fn listing(descriptor: &RuntimeDescriptor) -> Vec<SessionListItem> {
    request(descriptor, reqwest::Method::GET, "/v1/sessions")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn status(snapshot: &SessionSnapshot, text: &str) -> SessionChange {
    SessionChange::ActivityAdded {
        activity: Activity::Status {
            id: ActivityId::new(),
            turn_id: snapshot.turns[0].id,
            text: text.to_owned(),
        },
    }
}

#[tokio::test]
async fn concurrent_first_readers_and_subscribers_keep_one_history_and_every_mutation() {
    let history = History::stored(1).await;
    let before = &history.snapshots[0];
    let id = before.session.id;
    let server = history.start().await;
    let descriptor = server.descriptor();
    let sink = server.session_event_sink();
    let subscriptions = join_all((0..4).map(|_| async {
        let response = request(
            descriptor,
            reqwest::Method::GET,
            &format!("/v1/sessions/{id}/events"),
        )
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
        let mut events = response.bytes_stream().eventsource();
        let first = timeout(PROGRESS_DEADLINE, events.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(first.event, suru::protocol::SESSION_SNAPSHOT_EVENT);
        (
            serde_json::from_str::<SessionSnapshot>(&first.data).unwrap(),
            events,
        )
    }));
    let reads = join_all((0..4).map(|_| support::read_session(descriptor, id)));
    let (feeds, reads, update) = tokio::join!(
        subscriptions,
        reads,
        sink.publish(id, vec![status(before, "First mutation")])
    );
    let update = update.unwrap();
    assert_eq!(update.revision, SessionRevision(before.revision.0 + 1));
    let after = support::read_session(descriptor, id).await;
    for snapshot in reads
        .iter()
        .chain(feeds.iter().map(|(snapshot, _)| snapshot))
    {
        assert!(
            snapshot == before || snapshot == &after,
            "a first access sees one complete revision"
        );
    }
    assert_eq!(after.prompts, before.prompts);
    assert_eq!(after.messages, before.messages);
    assert_eq!(
        &after.activities[..before.activities.len()],
        before.activities
    );
    let final_update = sink
        .publish(id, vec![status(before, "After every reader attached")])
        .await
        .unwrap();
    for (snapshot, mut events) in feeds {
        let mut revision = snapshot.revision;
        while revision < final_update.revision {
            let event = timeout(PROGRESS_DEADLINE, events.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(event.event, suru::protocol::SESSION_UPDATED_EVENT);
            let update: SessionUpdate = serde_json::from_str(&event.data).unwrap();
            assert_eq!(update.revision, SessionRevision(revision.0 + 1));
            revision = update.revision;
        }
    }
    let durable = support::read_session(descriptor, id).await;
    server.shutdown().await.unwrap();
    let restarted = history.start().await;
    assert_eq!(
        support::read_session(restarted.descriptor(), id).await,
        durable,
        "writer hydration preserves old content and every new update through restart"
    );
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn deletion_racing_first_reads_cannot_resurrect_history_or_damage_another_session() {
    let history = History::stored(2).await;
    let before = &history.snapshots[0];
    let id = before.session.id;
    let server = history.start().await;
    let descriptor = server.descriptor();
    let path = format!("/v1/sessions/{id}");
    let reads = join_all((0..8).map(|_| request(descriptor, reqwest::Method::GET, &path).send()));
    let deletion = request(descriptor, reqwest::Method::DELETE, &path).send();
    let (reads, deleted) = tokio::join!(reads, deletion);
    assert_eq!(deleted.unwrap().status(), reqwest::StatusCode::NO_CONTENT);
    for read in reads {
        let response = read.unwrap();
        match response.status() {
            reqwest::StatusCode::OK => {
                assert_eq!(response.json::<SessionSnapshot>().await.unwrap(), *before)
            }
            reqwest::StatusCode::NOT_FOUND => {}
            other => panic!("read racing deletion returned {other}"),
        }
    }
    assert!(
        server
            .session_event_sink()
            .publish(id, vec![status(before, "Late output")])
            .await
            .is_err()
    );
    assert_eq!(
        request(descriptor, reqwest::Method::GET, &path)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::NOT_FOUND
    );
    let other = &history.snapshots[1];
    assert_eq!(
        listing(descriptor)
            .await
            .iter()
            .map(SessionListItem::id)
            .collect::<Vec<_>>(),
        vec![other.session.id]
    );
    server.shutdown().await.unwrap();
    let restarted = history.start().await;
    assert_eq!(
        request(restarted.descriptor(), reqwest::Method::GET, &path)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::NOT_FOUND
    );
    assert_eq!(
        support::read_session(restarted.descriptor(), other.session.id).await,
        *other
    );
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn prompt_identity_in_an_unopened_session_cannot_be_reused_by_creation_or_admission() {
    use suru::protocol::{AdmitPromptRequest, PromptDelivery, SessionError, SessionErrorCode};
    let history = History::stored(2).await;
    let owner = &history.snapshots[0];
    let target = &history.snapshots[1];
    let collision = InitialPrompt {
        id: owner.prompts[0].id,
        text: "Different work using an existing Prompt identity".to_owned(),
        skill_invocations: Vec::new(),
    };
    let server = history.start().await;
    let denied = request(server.descriptor(), reqwest::Method::POST, "/v1/sessions")
        .json(&CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: owner.session.execution_directory.clone(),
            prompt: collision.clone(),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), reqwest::StatusCode::CONFLICT);
    assert_eq!(
        denied.json::<SessionError>().await.unwrap().code,
        SessionErrorCode::PromptConflict
    );
    server.shutdown().await.unwrap();

    let server = history.start().await;
    let denied = request(
        server.descriptor(),
        reqwest::Method::POST,
        &format!("/v1/sessions/{}/prompts", target.session.id),
    )
    .json(&AdmitPromptRequest {
        prompt: collision,
        delivery: PromptDelivery::Queue,
    })
    .send()
    .await
    .unwrap();
    assert_eq!(denied.status(), reqwest::StatusCode::CONFLICT);
    assert_eq!(
        denied.json::<SessionError>().await.unwrap().code,
        SessionErrorCode::PromptConflict
    );
    assert_eq!(listing(server.descriptor()).await.len(), 2);
    server.shutdown().await.unwrap();
    let restarted = history.start().await;
    for snapshot in &history.snapshots {
        assert_eq!(
            support::read_session(restarted.descriptor(), snapshot.session.id).await,
            *snapshot
        );
    }
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn malformed_metadata_is_isolated_at_startup_without_listing_children_as_roots() {
    use diesel::connection::SimpleConnection;
    let history = History::stored(3).await;
    let malformed_parent = history.snapshots[0].session.id;
    let malformed_turn = history.snapshots[1].session.id;
    history
        .database()
        .batch_execute(&format!(
            "UPDATE sessions SET parent_session_id = 'not-a-uuid' WHERE id = '{malformed_parent}';
         UPDATE turns SET payload = '{{' WHERE session_id = '{malformed_turn}';"
        ))
        .unwrap();
    let server = history.start().await;
    let listed = listing(server.descriptor()).await;
    assert_eq!(
        listed.len(),
        2,
        "a malformed child link never turns the child into a listed root"
    );
    assert!(listed.iter().any(
        |item| matches!(item, SessionListItem::Unreadable(summary) if summary.id == malformed_turn)
    ));
    let healthy = &history.snapshots[2];
    assert_eq!(
        support::read_session(server.descriptor(), healthy.session.id).await,
        *healthy
    );
    assert_eq!(
        request(
            server.descriptor(),
            reqwest::Method::DELETE,
            &format!("/v1/sessions/{malformed_parent}")
        )
        .send()
        .await
        .unwrap()
        .status(),
        reqwest::StatusCode::CONFLICT
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn unreadable_child_hydration_refreshes_root_usage_and_keeps_parent_owned_deletion() {
    use crate::provider_support::ControlledProvider;
    use diesel::{Connection, RunQueryDsl};
    use suru::{
        protocol::Usage,
        provider::{
            ProviderEvent, ProviderEventAttribution, ProviderSubagentId, ProviderSubagentStatus,
        },
    };
    let root = tempfile::tempdir().unwrap();
    let channel = "unreadable-child-hydration";
    let config = ServerConfig::new(root.path(), channel).unwrap();
    let fixture = support::working_turn(root.path(), channel).await;
    let child = ProviderSubagentId::new("child");
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentStarted {
            subagent_id: child.clone(),
            name: "Explore".to_owned(),
            description: "Read the workspace".to_owned(),
        })
        .await;
    fixture
        .provider_session
        .emit_attributed_and_wait_until_observed(
            ProviderEventAttribution::Subagent(child.clone()),
            ProviderEvent::Usage {
                usage: Usage {
                    output_tokens: Some(7),
                    ..Default::default()
                },
                cost: None,
            },
        )
        .await;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentCompleted {
            subagent_id: child,
            status: ProviderSubagentStatus::Completed,
        })
        .await;
    fixture.provider_session.emit(ProviderEvent::TurnCompleted);
    let before = support::read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        fixture.session_id,
        "parent and child settle with recorded Usage",
        |snapshot| {
            snapshot.turns[0].status.is_terminal()
                && snapshot.working_since().is_none()
                && snapshot
                    .total_usage()
                    .is_some_and(|usage| usage.output_tokens == Some(7))
        },
    )
    .await;
    let child_id = before
        .activities
        .iter()
        .find_map(|activity| match activity {
            Activity::Subagent { session_id, .. } => Some(*session_id),
            _ => None,
        })
        .unwrap();
    drop(fixture.provider_session);
    fixture.server.shutdown().await.unwrap();
    let mut database =
        diesel::SqliteConnection::establish(config.data_dir().join("suru.db").to_str().unwrap())
            .unwrap();
    // Invalid Provider-owned data is discovered only when this tree opens.
    diesel::sql_query("INSERT INTO provider_resume_states(session_id, provider, payload) VALUES (?, 'controlled', '{')")
        .bind::<diesel::sql_types::Text, _>(child_id.to_string()).execute(&mut database).unwrap();
    drop(database);
    let (runtime, _provider) = ControlledProvider::new();
    let server = suru::server::spawn_with_provider(config.clone(), runtime)
        .await
        .unwrap();
    let initial = listing(server.descriptor()).await;
    assert!(matches!(&initial[..], [SessionListItem::Readable(summary)]
        if summary.session.id == before.session.id && summary.total_usage.unwrap().output_tokens == Some(7)));
    let (_, mut events) =
        crate::support::open_catalog_stream_with_snapshot(server.descriptor()).await;
    let failed = request(
        server.descriptor(),
        reqwest::Method::GET,
        &format!("/v1/sessions/{child_id}"),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(failed.status(), reqwest::StatusCode::NOT_FOUND);
    let changed = timeout(PROGRESS_DEADLINE, events.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        changed.change,
        SessionCatalogChange::Invalidated {
            session_id: before.session.id
        }
    );
    drop(events);
    let listed = listing(server.descriptor()).await;
    assert!(matches!(&listed[..], [SessionListItem::Readable(summary)]
        if summary.session.id == before.session.id && summary.total_usage.is_none()));
    let parent = support::read_session(server.descriptor(), before.session.id).await;
    assert_eq!(parent.messages, before.messages);
    assert!(parent.total_usage().is_none());
    assert_eq!(
        request(
            server.descriptor(),
            reqwest::Method::DELETE,
            &format!("/v1/sessions/{child_id}")
        )
        .send()
        .await
        .unwrap()
        .status(),
        reqwest::StatusCode::CONFLICT
    );
    assert_eq!(
        request(
            server.descriptor(),
            reqwest::Method::DELETE,
            &format!("/v1/sessions/{}", before.session.id)
        )
        .send()
        .await
        .unwrap()
        .status(),
        reqwest::StatusCode::NO_CONTENT
    );
    server.shutdown().await.unwrap();
    let restarted = spawn_with_failing_provider(config).await.unwrap();
    assert!(listing(restarted.descriptor()).await.is_empty());
    for id in [before.session.id, child_id] {
        assert_eq!(
            request(
                restarted.descriptor(),
                reqwest::Method::GET,
                &format!("/v1/sessions/{id}")
            )
            .send()
            .await
            .unwrap()
            .status(),
            reqwest::StatusCode::NOT_FOUND
        );
    }
    restarted.shutdown().await.unwrap();
}
