//! The managed client: Session attach, reconnection, deletion, and stream recovery.

use crate::server_support::PROGRESS_DEADLINE;
use crate::{
    failing_provider_support::spawn_with_failing_provider,
    support::{read_session_at_least_revision, receive_managed_client_initial_state},
};
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{Path as AxumPath, State},
    http::{
        HeaderMap, StatusCode,
        header::{AUTHORIZATION, CONTENT_TYPE},
    },
    response::{
        IntoResponse, Response,
        sse::{Event, Sse},
    },
    routing::get,
};
use futures_util::{StreamExt, stream};
use std::{
    convert::Infallible,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use suru::{
    build_identity,
    managed_client::{ManagedClient, ManagedClientConfig, ManagedEvent, SessionEvent},
    protocol::{
        Activity, ActivityId, CreateSessionRequest, InitialPrompt, LifecycleState, Message,
        MessageId, MessageRole, MessageStatus, ModelAvailability, PROTOCOL_VERSION, Prompt,
        PromptDelivery, PromptId, PromptOrder, PromptStatus, RuntimeDescriptor,
        SESSION_CATALOG_SNAPSHOT_EVENT, SESSION_SNAPSHOT_EVENT, SESSION_UPDATED_EVENT,
        ServerIdentity, Session, SessionCatalogRevision, SessionCatalogSnapshot, SessionChange,
        SessionCreated, SessionDeleted, SessionId, SessionRevision, SessionSnapshot, SessionStatus,
        SessionUpdate, TranscriptItem, Turn, TurnId, TurnStatus, Workspace,
    },
    server::ServerConfig,
};
use tokio::time::{Duration, timeout};

#[tokio::test]
async fn managed_clients_can_reconnect_to_a_session_that_outlives_its_first_client() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "managed-session-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let mut first_client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "managed-session-test")
            .expect("configure first managed client"),
    )
    .await
    .expect("connect first managed client");
    receive_managed_client_initial_state(&mut first_client).await;

    let created = first_client
        .create_session(CreateSessionRequest {
            session_id: None,
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Explain this workspace".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create Session through managed client");
    let settled = read_session_at_least_revision(
        &reqwest::Client::new(),
        server.descriptor(),
        created.session.id,
        SessionRevision(2),
    )
    .await;
    let mut first_subscription = first_client
        .attach_session(created.session.id)
        .await
        .expect("subscribe through first managed client");
    assert_eq!(
        first_subscription
            .next()
            .await
            .expect("first Session event arrives")
            .expect("first Session event is valid"),
        SessionEvent::snapshot(settled.clone())
    );

    drop(first_subscription);
    drop(first_client);

    let mut second_client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "managed-session-test")
            .expect("configure second managed client"),
    )
    .await
    .expect("connect second managed client");
    receive_managed_client_initial_state(&mut second_client).await;
    let mut second_subscription = second_client
        .attach_session(created.session.id)
        .await
        .expect("reconnect to existing Session");
    assert_eq!(
        second_subscription
            .next()
            .await
            .expect("reconnected Session event arrives")
            .expect("reconnected Session event is valid"),
        SessionEvent::snapshot(settled)
    );

    drop(second_subscription);
    drop(second_client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn managed_client_can_discover_read_and_attach_to_a_known_session() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "managed-session-attach-test")
            .expect("configure server"),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "managed-session-attach-test")
            .expect("configure managed client"),
    )
    .await
    .expect("connect managed client");
    receive_managed_client_initial_state(&mut client).await;
    let created = client
        .create_session(CreateSessionRequest {
            session_id: None,
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Attach to this Session".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create Session");
    let settled = read_session_at_least_revision(
        &reqwest::Client::new(),
        server.descriptor(),
        created.session.id,
        SessionRevision(2),
    )
    .await;

    assert_eq!(
        client
            .read_session(created.session.id)
            .await
            .expect("read Session through managed client"),
        settled
    );
    let summaries = client
        .list_sessions(Some(workspace.path()))
        .await
        .expect("discover Sessions through managed client");
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].id(), created.session.id);

    let mut subscription = client
        .attach_session(created.session.id)
        .await
        .expect("attach to known Session ID");
    assert_eq!(
        subscription
            .next()
            .await
            .expect("attached Session event arrives")
            .expect("attached Session event is valid"),
        SessionEvent::snapshot(settled)
    );

    drop(subscription);
    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn managed_clients_observe_durable_session_deletion() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let config = ServerConfig::new(state_dir.path(), "managed-session-delete-test")
        .expect("configure server")
        .with_data_dir(data_dir.path());
    let server = spawn_with_failing_provider(config.clone())
        .await
        .expect("spawn server");
    let mut deleting_client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "managed-session-delete-test")
            .expect("configure deleting client")
            .with_data_dir(data_dir.path()),
    )
    .await
    .expect("connect deleting client");
    let mut observing_client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "managed-session-delete-test")
            .expect("configure observing client")
            .with_data_dir(data_dir.path()),
    )
    .await
    .expect("connect observing client");
    receive_managed_client_initial_state(&mut deleting_client).await;
    receive_managed_client_initial_state(&mut observing_client).await;

    let created = deleting_client
        .create_session(CreateSessionRequest {
            session_id: None,
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Delete this Session and its Transcript".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create Session");
    let session_id = created.session.id;
    let settled = read_session_at_least_revision(
        &reqwest::Client::new(),
        server.descriptor(),
        session_id,
        SessionRevision(2),
    )
    .await;
    assert!(!settled.transcript.is_empty());
    let mut subscription = observing_client
        .attach_session(session_id)
        .await
        .expect("observe Session before deletion");
    assert!(matches!(
        subscription.next().await,
        Some(Ok(SessionEvent::Snapshot(_)))
    ));

    assert_eq!(
        timeout(PROGRESS_DEADLINE, observing_client.next())
            .await
            .expect("creation reaches connected client"),
        Some(ManagedEvent::SessionCreated(SessionCreated { session_id })),
        "a Session one client made is announced to every other client listing Sessions"
    );
    assert!(matches!(
        timeout(PROGRESS_DEADLINE, observing_client.next())
            .await
            .expect("the failed Turn outcome reaches the connected client"),
        Some(ManagedEvent::SessionStandingInputsChanged(changed))
            if changed.session_id == session_id
                && changed.inputs.latest_turn.is_some_and(|latest| {
                    latest.status == TurnStatus::Failed && latest.settled_at.is_some()
                })
    ));

    // The failed startup ends the Working the admission began, which every
    // listing client hears before anything else moves (ADR 0024).
    assert_eq!(
        timeout(PROGRESS_DEADLINE, observing_client.next())
            .await
            .expect("the end of Working reaches the connected client"),
        Some(ManagedEvent::SessionWorkingChanged(
            suru::protocol::SessionWorkingChanged {
                session_id,
                working_since: None,
            }
        ))
    );

    deleting_client
        .delete_session(session_id)
        .await
        .expect("delete Session through managed client");

    assert_eq!(
        timeout(PROGRESS_DEADLINE, observing_client.next())
            .await
            .expect("deletion reaches connected client"),
        Some(ManagedEvent::SessionDeleted(SessionDeleted { session_id }))
    );
    let stream_error = timeout(PROGRESS_DEADLINE, subscription.next())
        .await
        .expect("deleted Session stream terminates")
        .expect("deleted Session stream reports its terminal rejection")
        .expect_err("deleted Session cannot be reattached");
    assert!(!stream_error.is_recoverable());
    assert!(
        deleting_client
            .list_sessions(None)
            .await
            .expect("list Sessions after deletion")
            .is_empty()
    );
    assert!(
        deleting_client.read_session(session_id).await.is_err(),
        "deleted Session and its Transcript are no longer readable"
    );

    drop(subscription);
    drop(observing_client);
    drop(deleting_client);
    server.shutdown().await.expect("stop original server");

    let replacement = spawn_with_failing_provider(config)
        .await
        .expect("restart server");
    let mut replacement_client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "managed-session-delete-test")
            .expect("configure replacement client")
            .with_data_dir(data_dir.path()),
    )
    .await
    .expect("connect replacement client");
    receive_managed_client_initial_state(&mut replacement_client).await;
    assert!(
        replacement_client
            .list_sessions(None)
            .await
            .expect("list Sessions after restart")
            .is_empty()
    );
    assert!(
        replacement_client.read_session(session_id).await.is_err(),
        "deleted Session stays gone after restart"
    );

    drop(replacement_client);
    replacement
        .shutdown()
        .await
        .expect("stop replacement server");
}

#[tokio::test]
async fn managed_client_switching_away_does_not_interrupt_an_active_turn() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "managed-session-switch-test")
            .expect("configure server"),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "managed-session-switch-test")
            .expect("configure managed client"),
    )
    .await
    .expect("connect managed client");
    receive_managed_client_initial_state(&mut client).await;

    let first = client
        .create_session(CreateSessionRequest {
            session_id: None,
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "First Session".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create first Session");
    let first = read_session_at_least_revision(
        &reqwest::Client::new(),
        server.descriptor(),
        first.session.id,
        SessionRevision(2),
    )
    .await;
    let second = client
        .create_session(CreateSessionRequest {
            session_id: None,
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Second Session".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create second Session");
    let second = read_session_at_least_revision(
        &reqwest::Client::new(),
        server.descriptor(),
        second.session.id,
        SessionRevision(2),
    )
    .await;

    let mut first_subscription = client
        .attach_session(first.session.id)
        .await
        .expect("attach first Session");
    assert!(matches!(
        first_subscription.next().await,
        Some(Ok(SessionEvent::Snapshot(_)))
    ));
    let prompt_id = PromptId::new();
    let turn_id = TurnId::new();
    server
        .session_event_sink()
        .publish(
            first.session.id,
            vec![
                SessionChange::PromptAdded {
                    prompt: Prompt {
                        id: prompt_id,
                        text: "Keep working while detached".to_owned(),
                        delivery: PromptDelivery::Steer,
                        admission_order: PromptOrder(2),
                        status: PromptStatus::Delivered,
                        withdrawal: None,
                        skill_invocations: Vec::new(),
                        attachments: Vec::new(),
                        author: None,
                    },
                },
                SessionChange::TurnAdded {
                    turn: Turn {
                        id: turn_id,
                        prompt_id: Some(prompt_id),
                        compaction_requested: false,
                        agent: None,
                        status: TurnStatus::Active,
                        started_at: None,
                        settled_at: None,
                        last_output_at: None,
                        usage: None,
                        cost: None,
                        cost_basis: None,
                        cost_details: None,
                    },
                },
                SessionChange::MessageAdded {
                    message: Message {
                        id: MessageId::new(),
                        turn_id,
                        role: MessageRole::User,
                        status: MessageStatus::Completed,
                        content: "Keep working while detached".to_owned(),
                        skill_invocations: Vec::new(),
                        attachments: Vec::new(),
                        truncated: false,
                        author: None,
                    },
                },
                SessionChange::SessionStatusChanged {
                    status: SessionStatus::Active,
                },
            ],
        )
        .await
        .expect("start active Turn");
    assert!(matches!(
        first_subscription.next().await,
        Some(Ok(SessionEvent::Updated(_)))
    ));

    drop(first_subscription);
    let mut second_subscription = client
        .attach_session(second.session.id)
        .await
        .expect("switch subscription to second Session");
    assert!(matches!(
        second_subscription.next().await,
        Some(Ok(SessionEvent::Snapshot(_)))
    ));
    let still_active = client
        .read_session(first.session.id)
        .await
        .expect("read detached first Session");
    assert_eq!(still_active.session.status, SessionStatus::Active);
    assert_eq!(
        still_active
            .turns
            .iter()
            .find(|turn| turn.id == turn_id)
            .expect("active Turn remains in detached Session")
            .status,
        TurnStatus::Active
    );

    drop(second_subscription);
    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn two_clients_converge_on_one_session_without_observing_another_session() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "shared-session-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let session_events = server.session_event_sink();
    let mut first_client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "shared-session-test")
            .expect("configure first client"),
    )
    .await
    .expect("connect first client");
    let mut second_client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "shared-session-test")
            .expect("configure second client"),
    )
    .await
    .expect("connect second client");
    receive_managed_client_initial_state(&mut first_client).await;
    receive_managed_client_initial_state(&mut second_client).await;

    let shared = first_client
        .create_session(CreateSessionRequest {
            session_id: None,
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Shared Session".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create shared Session");
    let isolated = first_client
        .create_session(CreateSessionRequest {
            session_id: None,
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Isolated Session".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create isolated Session");
    let shared_settled = read_session_at_least_revision(
        &reqwest::Client::new(),
        server.descriptor(),
        shared.session.id,
        SessionRevision(2),
    )
    .await;
    let isolated_settled = read_session_at_least_revision(
        &reqwest::Client::new(),
        server.descriptor(),
        isolated.session.id,
        SessionRevision(2),
    )
    .await;
    let mut first_subscription = first_client
        .attach_session(shared.session.id)
        .await
        .expect("attach first client to shared Session");
    let mut second_subscription = second_client
        .attach_session(shared.session.id)
        .await
        .expect("attach second client to shared Session");

    let first_projection = first_subscription
        .next()
        .await
        .expect("first client receives shared Session")
        .expect("first shared Session snapshot is valid");
    let second_projection = second_subscription
        .next()
        .await
        .expect("second client receives shared Session")
        .expect("second shared Session snapshot is valid");
    assert_eq!(
        first_projection,
        SessionEvent::snapshot(shared_settled.clone())
    );
    assert_eq!(second_projection, first_projection);

    let mut isolated_subscription = second_client
        .attach_session(isolated.session.id)
        .await
        .expect("attach second client to isolated Session");
    assert_eq!(
        isolated_subscription
            .next()
            .await
            .expect("isolated Session snapshot arrives")
            .expect("isolated Session snapshot is valid"),
        SessionEvent::snapshot(isolated_settled)
    );

    let before_update = first_client
        .list_sessions(None)
        .await
        .expect("list Sessions before update");
    assert_eq!(before_update[0].id(), isolated.session.id);
    let prompt_id = PromptId::new();
    let turn_id = TurnId::new();
    let update = session_events
        .publish(
            shared.session.id,
            vec![
                SessionChange::PromptAdded {
                    prompt: Prompt {
                        id: prompt_id,
                        text: "Observe this change".to_owned(),
                        delivery: PromptDelivery::Steer,
                        admission_order: PromptOrder(2),
                        status: PromptStatus::Delivered,
                        withdrawal: None,
                        skill_invocations: Vec::new(),
                        attachments: Vec::new(),
                        author: None,
                    },
                },
                SessionChange::TurnAdded {
                    turn: Turn {
                        id: turn_id,
                        prompt_id: Some(prompt_id),
                        compaction_requested: false,
                        agent: None,
                        status: TurnStatus::Active,
                        started_at: None,
                        settled_at: None,
                        last_output_at: None,
                        usage: None,
                        cost: None,
                        cost_basis: None,
                        cost_details: None,
                    },
                },
                SessionChange::MessageAdded {
                    message: Message {
                        id: MessageId::new(),
                        turn_id,
                        role: MessageRole::User,
                        status: MessageStatus::Completed,
                        content: "Observe this change".to_owned(),
                        skill_invocations: Vec::new(),
                        attachments: Vec::new(),
                        truncated: false,
                        author: None,
                    },
                },
                SessionChange::ActivityAdded {
                    activity: Activity::Status {
                        id: ActivityId::new(),
                        turn_id,
                        text: "Working".to_owned(),
                    },
                },
                SessionChange::SessionStatusChanged {
                    status: SessionStatus::Active,
                },
            ],
        )
        .await
        .expect("publish provider-neutral Session changes");
    assert_eq!(
        update.revision,
        SessionRevision(shared_settled.revision.0 + 1)
    );

    let first_update = first_subscription
        .next()
        .await
        .expect("first client receives Session update")
        .expect("first client Session update is valid");
    let second_update = second_subscription
        .next()
        .await
        .expect("second client receives Session update")
        .expect("second client Session update is valid");
    assert_eq!(first_update, SessionEvent::Updated(update.clone()));
    assert_eq!(second_update, first_update);
    assert!(
        timeout(Duration::from_millis(100), isolated_subscription.next())
            .await
            .is_err(),
        "an update for the shared Session must not appear on another Session stream"
    );

    let current = first_client
        .read_session(shared.session.id)
        .await
        .expect("read updated shared Session");
    assert_eq!(current.revision, update.revision);
    assert_eq!(current.prompts.len(), 2);
    assert_eq!(current.turns.len(), 2);
    assert_eq!(current.messages.len(), 2);
    assert_eq!(current.activities.len(), 2);
    assert_eq!(current.session.status, SessionStatus::Active);
    let after_update = first_client
        .list_sessions(None)
        .await
        .expect("list Sessions after update");
    let updated_summary = after_update[0]
        .readable()
        .expect("updated Session is readable");
    assert_eq!(updated_summary.session.id, shared.session.id);
    assert_eq!(updated_summary.session.status, SessionStatus::Active);
    assert!(updated_summary.updated_at > updated_summary.created_at);

    drop(isolated_subscription);
    drop(second_subscription);
    drop(first_subscription);
    drop(second_client);
    drop(first_client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn managed_session_stream_rejects_a_non_monotonic_revision() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let session_id = SessionId::new();
    let snapshot = failed_session_snapshot(session_id, workspace.path());
    let fixture = MalformedSessionStreamFixture::spawn(
        state_dir.path(),
        "malformed-session-stream-test",
        snapshot.clone(),
    )
    .await;
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "malformed-session-stream-test")
            .expect("configure managed client"),
    )
    .await
    .expect("connect managed client to fixture server");
    receive_managed_client_initial_state(&mut client).await;
    let mut subscription = client
        .attach_session(session_id)
        .await
        .expect("subscribe to fixture Session");

    assert_eq!(
        subscription
            .next()
            .await
            .expect("Session snapshot arrives")
            .expect("Session snapshot is valid"),
        SessionEvent::snapshot(snapshot)
    );
    let error = subscription
        .next()
        .await
        .expect("invalid Session update arrives")
        .expect_err("duplicate revision must be rejected");
    assert!(error.to_string().contains("revision is not monotonic"));
    assert!(!error.is_recoverable());

    drop(subscription);
    drop(client);
    drop(fixture);
}

#[tokio::test]
async fn managed_session_stream_classifies_body_failures_as_recoverable() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let session_id = SessionId::new();
    let snapshot = failed_session_snapshot(session_id, workspace.path());
    let fixture = MalformedSessionStreamFixture::spawn_transport_failure(
        state_dir.path(),
        "broken-session-transport-test",
        snapshot.clone(),
    )
    .await;
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "broken-session-transport-test")
            .expect("configure managed client"),
    )
    .await
    .expect("connect managed client to fixture server");
    receive_managed_client_initial_state(&mut client).await;
    let mut subscription = client
        .subscribe_session(session_id)
        .await
        .expect("subscribe to fixture Session");

    assert_eq!(
        subscription
            .next()
            .await
            .expect("Session snapshot arrives")
            .expect("Session snapshot is valid"),
        SessionEvent::snapshot(snapshot)
    );
    let error = subscription
        .next()
        .await
        .expect("transport failure arrives")
        .expect_err("broken response body must fail");
    assert!(error.is_recoverable());

    drop(subscription);
    drop(client);
    drop(fixture);
}

#[tokio::test]
async fn managed_subscription_rehydrates_before_live_deltas_after_same_server_disconnect() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let session_id = SessionId::new();
    let initial = failed_session_snapshot(session_id, workspace.path());
    let mut current = initial.clone();
    current.revision = SessionRevision(2);
    current.activities.push(Activity::Status {
        id: ActivityId::new(),
        turn_id: current.turns[0].id,
        text: "Recovered current state".to_owned(),
    });
    let update = SessionUpdate {
        session_id,
        revision: SessionRevision(3),
        changes: vec![SessionChange::SessionStatusChanged {
            status: SessionStatus::Active,
        }],
    };
    let fixture = ReconnectingSessionStreamFixture::spawn(
        state_dir.path(),
        "reconnecting-session-stream-test",
        initial.clone(),
        current.clone(),
        update.clone(),
    )
    .await;
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "reconnecting-session-stream-test")
            .expect("configure managed client"),
    )
    .await
    .expect("connect managed client to fixture server");
    receive_managed_client_initial_state(&mut client).await;
    let mut subscription = client
        .attach_session(session_id)
        .await
        .expect("attach to fixture Session");

    assert_eq!(
        subscription
            .next()
            .await
            .expect("initial Session snapshot arrives")
            .expect("initial Session snapshot is valid"),
        SessionEvent::snapshot(initial)
    );
    assert_eq!(
        timeout(PROGRESS_DEADLINE, subscription.next())
            .await
            .expect("subscription reconnects")
            .expect("fresh Session snapshot arrives")
            .expect("fresh Session snapshot is valid"),
        SessionEvent::snapshot(current)
    );
    assert_eq!(
        subscription
            .next()
            .await
            .expect("live Session delta arrives")
            .expect("live Session delta is valid"),
        SessionEvent::Updated(update)
    );

    drop(subscription);
    drop(client);
    drop(fixture);
}

fn failed_session_snapshot(session_id: SessionId, workspace: &std::path::Path) -> SessionSnapshot {
    let prompt_id = PromptId::new();
    let turn_id = TurnId::new();
    let message_id = MessageId::new();
    let activity_id = ActivityId::new();
    SessionSnapshot {
        title: String::new(),
        icon: None,
        session: Session {
            checkout: None,
            context_fill: None,
            id: session_id,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.to_owned(),
            },
            workspace: Workspace::directory(workspace.to_owned()),
            agent_selection: None,
            agent_selection_availability: ModelAvailability::Available,
            approval_posture: None,
            status: SessionStatus::Idle,
            working_since: None,
            monitoring_since: None,
            parent: None,
            begun_by: None,
        },
        revision: SessionRevision::INITIAL,
        prompts: vec![Prompt {
            id: prompt_id,
            text: "Explain this workspace".to_owned(),
            delivery: PromptDelivery::Steer,
            admission_order: PromptOrder::INITIAL,
            status: PromptStatus::Delivered,
            withdrawal: None,
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
            author: None,
        }],
        turns: vec![Turn {
            id: turn_id,
            prompt_id: Some(prompt_id),
            compaction_requested: false,
            agent: None,
            status: TurnStatus::Failed,
            started_at: None,
            settled_at: None,
            last_output_at: None,
            usage: None,
            cost: None,
            cost_basis: None,
            cost_details: None,
        }],
        messages: vec![Message {
            id: message_id,
            turn_id,
            role: MessageRole::User,
            status: MessageStatus::Completed,
            content: "Explain this workspace".to_owned(),
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
            truncated: false,
            author: None,
        }],
        activities: vec![Activity::Error {
            id: activity_id,
            turn_id,
            text: "No Agent is selected".to_owned(),
        }],
        subagent_interventions: Vec::new(),
        pending_approvals: Vec::new(),
        submitting_approvals: Vec::new(),
        pending_approvals_revision: suru::protocol::SessionRevision(0),
        watches: Vec::new(),
        waiting_on_subagents: None,
        subagent_usage: None,
        total_cost: None,
        own_cost: None,
        transcript: vec![
            TranscriptItem::Message { message_id },
            TranscriptItem::Activity { activity_id },
        ],
        attachments: Vec::new(),
    }
}

#[derive(Clone)]
struct MalformedSessionStreamState {
    descriptor: RuntimeDescriptor,
    snapshot: SessionSnapshot,
    transport_failure: bool,
}

struct MalformedSessionStreamFixture {
    task: tokio::task::JoinHandle<()>,
}

impl MalformedSessionStreamFixture {
    async fn spawn(state_dir: &std::path::Path, channel: &str, snapshot: SessionSnapshot) -> Self {
        Self::spawn_with_transport_failure(state_dir, channel, snapshot, false).await
    }

    async fn spawn_transport_failure(
        state_dir: &std::path::Path,
        channel: &str,
        snapshot: SessionSnapshot,
    ) -> Self {
        Self::spawn_with_transport_failure(state_dir, channel, snapshot, true).await
    }

    async fn spawn_with_transport_failure(
        state_dir: &std::path::Path,
        channel: &str,
        snapshot: SessionSnapshot,
        transport_failure: bool,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind malformed Session stream fixture");
        let descriptor = RuntimeDescriptor::new(
            format!(
                "http://{}",
                listener.local_addr().expect("read fixture address")
            ),
            "malformed-session-stream-token".to_owned(),
            ServerIdentity {
                instance_id: uuid::Uuid::new_v4(),
                pid: std::process::id(),
                protocol_version: PROTOCOL_VERSION,
                build_identity: build_identity::for_current_executable()
                    .expect("identify fixture test executable"),
            },
        );
        let runtime_dir = state_dir.join(channel);
        std::fs::create_dir_all(&runtime_dir).expect("create fixture runtime directory");
        serde_json::to_writer(
            std::fs::File::create(runtime_dir.join("runtime.json"))
                .expect("create fixture runtime descriptor"),
            &descriptor,
        )
        .expect("write fixture runtime descriptor");
        let state = Arc::new(MalformedSessionStreamState {
            descriptor,
            snapshot,
            transport_failure,
        });
        let app = Router::new()
            .route("/health", get(malformed_fixture_health))
            .route("/v1/events", get(malformed_fixture_server_events))
            .route("/v1/session-events", get(malformed_fixture_catalog_events))
            .route(
                "/v1/sessions/{session_id}/events",
                get(malformed_fixture_session_events),
            )
            .with_state(state);
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve malformed Session stream fixture");
        });
        Self { task }
    }
}

impl Drop for MalformedSessionStreamFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn malformed_fixture_health(
    State(state): State<Arc<MalformedSessionStreamState>>,
    headers: HeaderMap,
) -> Response {
    if !fixture_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(state.descriptor.health(LifecycleState::Ready)).into_response()
}

async fn malformed_fixture_server_events(
    State(state): State<Arc<MalformedSessionStreamState>>,
    headers: HeaderMap,
) -> Response {
    if !fixture_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Sse::new(fixture_lifecycle_stream().chain(stream::pending())).into_response()
}

async fn malformed_fixture_catalog_events(
    State(state): State<Arc<MalformedSessionStreamState>>,
    headers: HeaderMap,
) -> Response {
    fixture_catalog_events_response(&headers, &state.descriptor.token, state.snapshot.session.id)
}

async fn malformed_fixture_session_events(
    State(state): State<Arc<MalformedSessionStreamState>>,
    AxumPath(session_id): AxumPath<SessionId>,
    headers: HeaderMap,
) -> Response {
    if !fixture_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if session_id != state.snapshot.session.id {
        return StatusCode::NOT_FOUND.into_response();
    }
    let snapshot = state.snapshot.clone();
    if state.transport_failure {
        let snapshot_event = format!(
            "event: {SESSION_SNAPSHOT_EVENT}\nid: {}\ndata: {}\n\n",
            snapshot.revision.0,
            serde_json::to_string(&snapshot).expect("serialize fixture Session snapshot"),
        );
        let snapshot_chunk =
            stream::once(async move { Ok::<_, std::io::Error>(Bytes::from(snapshot_event)) });
        let failed_chunk = stream::once(async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Err::<Bytes, _>(std::io::Error::other("fixture Session transport failure"))
        });
        return Response::builder()
            .header(CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(snapshot_chunk.chain(failed_chunk)))
            .expect("build broken Session stream response");
    }
    let update = SessionUpdate {
        session_id,
        revision: SessionRevision::INITIAL,
        changes: Vec::new(),
    };
    let events = vec![
        Event::default()
            .event(SESSION_SNAPSHOT_EVENT)
            .id(snapshot.revision.0.to_string())
            .json_data(snapshot)
            .expect("serialize fixture Session snapshot"),
        Event::default()
            .event(SESSION_UPDATED_EVENT)
            .id(update.revision.0.to_string())
            .json_data(update)
            .expect("serialize fixture Session update"),
    ]
    .into_iter()
    .map(Ok::<_, Infallible>);
    Sse::new(stream::iter(events)).into_response()
}

fn fixture_authenticated(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == format!("Bearer {token}"))
}

#[derive(Clone)]
struct ReconnectingSessionStreamState {
    descriptor: RuntimeDescriptor,
    connections: Arc<AtomicUsize>,
    initial: SessionSnapshot,
    current: SessionSnapshot,
    update: SessionUpdate,
}

struct ReconnectingSessionStreamFixture {
    task: tokio::task::JoinHandle<()>,
}

impl ReconnectingSessionStreamFixture {
    async fn spawn(
        state_dir: &std::path::Path,
        channel: &str,
        initial: SessionSnapshot,
        current: SessionSnapshot,
        update: SessionUpdate,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind reconnecting Session stream fixture");
        let descriptor = RuntimeDescriptor::new(
            format!(
                "http://{}",
                listener.local_addr().expect("read fixture address")
            ),
            "reconnecting-session-stream-token".to_owned(),
            ServerIdentity {
                instance_id: uuid::Uuid::new_v4(),
                pid: std::process::id(),
                protocol_version: PROTOCOL_VERSION,
                build_identity: build_identity::for_current_executable()
                    .expect("identify fixture test executable"),
            },
        );
        let runtime_dir = state_dir.join(channel);
        std::fs::create_dir_all(&runtime_dir).expect("create fixture runtime directory");
        serde_json::to_writer(
            std::fs::File::create(runtime_dir.join("runtime.json"))
                .expect("create fixture runtime descriptor"),
            &descriptor,
        )
        .expect("write fixture runtime descriptor");
        let state = Arc::new(ReconnectingSessionStreamState {
            descriptor,
            connections: Arc::new(AtomicUsize::new(0)),
            initial,
            current,
            update,
        });
        let app = Router::new()
            .route("/health", get(reconnecting_fixture_health))
            .route("/v1/events", get(reconnecting_fixture_server_events))
            .route(
                "/v1/session-events",
                get(reconnecting_fixture_catalog_events),
            )
            .route(
                "/v1/sessions/{session_id}/events",
                get(reconnecting_fixture_session_events),
            )
            .with_state(state);
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve reconnecting Session stream fixture");
        });
        Self { task }
    }
}

impl Drop for ReconnectingSessionStreamFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn reconnecting_fixture_health(
    State(state): State<Arc<ReconnectingSessionStreamState>>,
    headers: HeaderMap,
) -> Response {
    if !fixture_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(state.descriptor.health(LifecycleState::Ready)).into_response()
}

async fn reconnecting_fixture_server_events(
    State(state): State<Arc<ReconnectingSessionStreamState>>,
    headers: HeaderMap,
) -> Response {
    if !fixture_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Sse::new(fixture_lifecycle_stream().chain(stream::pending())).into_response()
}

async fn reconnecting_fixture_catalog_events(
    State(state): State<Arc<ReconnectingSessionStreamState>>,
    headers: HeaderMap,
) -> Response {
    fixture_catalog_events_response(&headers, &state.descriptor.token, state.initial.session.id)
}

/// The lifecycle-stream opening every conforming server sends: the connected
/// comment, the effective-settings snapshot, then the Model Catalog.
fn fixture_lifecycle_stream()
-> impl futures_util::Stream<Item = Result<Event, Infallible>> + Send + 'static {
    stream::once(async move { Ok::<_, Infallible>(Event::default().comment("connected")) })
        .chain(stream::once(async move {
            Ok::<_, Infallible>(
                Event::default()
                    .event(suru::protocol::SETTINGS_SNAPSHOT_EVENT)
                    .json_data(suru::protocol::SettingsSnapshot::default())
                    .expect("serialize fixture settings snapshot"),
            )
        }))
        .chain(stream::once(async move {
            Ok::<_, Infallible>(
                Event::default()
                    .event(suru::protocol::MODEL_CATALOG_EVENT)
                    .json_data(suru::protocol::ModelCatalog {
                        providers: Vec::new(),
                    })
                    .expect("serialize fixture Model Catalog"),
            )
        }))
}

fn fixture_catalog_events_response(
    headers: &HeaderMap,
    token: &str,
    session_id: SessionId,
) -> Response {
    if !fixture_authenticated(headers, token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let snapshot = SessionCatalogSnapshot {
        workspace_paths: Default::default(),
        revision: SessionCatalogRevision::INITIAL,
        session_ids: vec![session_id],
        checkout_states: Vec::new(),
    };
    Sse::new(
        stream::once(async move {
            Ok::<_, Infallible>(
                Event::default()
                    .event(SESSION_CATALOG_SNAPSHOT_EVENT)
                    .id(snapshot.revision.0.to_string())
                    .json_data(snapshot)
                    .expect("serialize fixture Session catalog snapshot"),
            )
        })
        .chain(stream::pending()),
    )
    .into_response()
}

async fn reconnecting_fixture_session_events(
    State(state): State<Arc<ReconnectingSessionStreamState>>,
    AxumPath(session_id): AxumPath<SessionId>,
    headers: HeaderMap,
) -> Response {
    if !fixture_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if session_id != state.initial.session.id {
        return StatusCode::NOT_FOUND.into_response();
    }

    if state.connections.fetch_add(1, Ordering::SeqCst) == 0 {
        let snapshot = state.initial.clone();
        return Sse::new(stream::once(async move {
            Ok::<_, Infallible>(
                Event::default()
                    .event(SESSION_SNAPSHOT_EVENT)
                    .id(snapshot.revision.0.to_string())
                    .json_data(snapshot)
                    .expect("serialize initial fixture Session snapshot"),
            )
        }))
        .into_response();
    }

    let snapshot = state.current.clone();
    let update = state.update.clone();
    let events = stream::iter([
        Event::default()
            .event(SESSION_SNAPSHOT_EVENT)
            .id(snapshot.revision.0.to_string())
            .json_data(snapshot)
            .expect("serialize current fixture Session snapshot"),
        Event::default()
            .event(SESSION_UPDATED_EVENT)
            .id(update.revision.0.to_string())
            .json_data(update)
            .expect("serialize fixture Session update"),
    ])
    .map(Ok::<_, Infallible>)
    .chain(stream::pending());
    Sse::new(events).into_response()
}
