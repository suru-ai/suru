//! The authenticated session SSE stream and its shutdown behaviour.

use crate::server_support::PROGRESS_DEADLINE;
use crate::{
    failing_provider_support::spawn_with_failing_provider,
    support::{read_session_at_least_revision, receive_managed_client_initial_state},
};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, SessionEvent},
    protocol::{
        CreateSessionRequest, InitialPrompt, Message, MessageId, MessageRole, MessageStatus,
        Prompt, PromptDelivery, PromptId, PromptOrder, PromptStatus, SESSION_SNAPSHOT_EVENT,
        SessionChange, SessionRevision, SessionSnapshot, Turn, TurnId, TurnStatus,
    },
    server::{AgentOutput, ServerConfig},
};
use tokio::time::timeout;

#[tokio::test]
async fn authenticated_session_stream_starts_with_a_complete_revisioned_snapshot() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "session-events-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let client = reqwest::Client::new();
    let created = client
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&CreateSessionRequest {
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
        .send()
        .await
        .expect("create Session")
        .error_for_status()
        .expect("Session creation succeeds")
        .json::<SessionSnapshot>()
        .await
        .expect("decode created Session");
    let events_url = format!(
        "{}/v1/sessions/{}/events",
        descriptor.base_url, created.session.id
    );

    let unauthenticated = client
        .get(&events_url)
        .send()
        .await
        .expect("request Session stream without authentication");
    assert_eq!(unauthenticated.status(), reqwest::StatusCode::UNAUTHORIZED);

    let response = client
        .get(&events_url)
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("open authenticated Session stream")
        .error_for_status()
        .expect("Session stream authenticates");
    let mut events = response.bytes_stream().eventsource();
    let first = timeout(PROGRESS_DEADLINE, events.next())
        .await
        .expect("Session snapshot arrives")
        .expect("Session stream remains open")
        .expect("decode Session SSE event");

    assert_eq!(first.event, SESSION_SNAPSHOT_EVENT);
    let first_snapshot = serde_json::from_str::<SessionSnapshot>(&first.data)
        .expect("decode Session snapshot event");
    assert_eq!(first_snapshot.session.id, created.session.id);
    assert_eq!(first.id, first_snapshot.revision.0.to_string());

    drop(events);
    let reconnected = client
        .get(&events_url)
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("reconnect to Session stream")
        .error_for_status()
        .expect("Session remains available after stream disconnect");
    let mut reconnected_events = reconnected.bytes_stream().eventsource();
    let fresh_snapshot = timeout(PROGRESS_DEADLINE, reconnected_events.next())
        .await
        .expect("fresh Session snapshot arrives")
        .expect("reconnected Session stream remains open")
        .expect("decode reconnected Session SSE event");
    assert_eq!(fresh_snapshot.event, SESSION_SNAPSHOT_EVENT);
    let fresh_snapshot_body = serde_json::from_str::<SessionSnapshot>(&fresh_snapshot.data)
        .expect("decode fresh Session snapshot event");
    assert_eq!(fresh_snapshot_body.session.id, created.session.id);
    assert_eq!(
        fresh_snapshot.id,
        fresh_snapshot_body.revision.0.to_string()
    );
    assert!(fresh_snapshot_body.revision >= first_snapshot.revision);

    drop(reconnected_events);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn real_session_stream_appends_and_completes_one_stable_agent_message() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "agent-output-stream-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "agent-output-stream-test")
            .expect("configure managed client"),
    )
    .await
    .expect("connect managed client");
    receive_managed_client_initial_state(&mut client).await;

    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Explain the stream".to_owned(),
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
    let prompt_id = PromptId::new();
    let turn_id = TurnId::new();
    let message_id = MessageId::new();
    let mut subscription = client
        .subscribe_session(session_id)
        .await
        .expect("subscribe to Session");
    assert_eq!(
        timeout(PROGRESS_DEADLINE, subscription.next())
            .await
            .expect("Session snapshot arrives")
            .expect("Session stream remains open")
            .expect("Session snapshot is valid"),
        SessionEvent::snapshot(settled.clone())
    );

    let active_update = server
        .session_event_sink()
        .publish(
            session_id,
            vec![
                SessionChange::PromptAdded {
                    prompt: Prompt {
                        id: prompt_id,
                        text: "Continue with an active Agent".to_owned(),
                        delivery: PromptDelivery::Steer,
                        admission_order: PromptOrder(2),
                        status: PromptStatus::Delivered,
                        withdrawal: None,
                        skill_invocations: Vec::new(),
                        attachments: Vec::new(),
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
                        content: "Continue with an active Agent".to_owned(),
                        skill_invocations: Vec::new(),
                        attachments: Vec::new(),
                        truncated: false,
                    },
                },
            ],
        )
        .await
        .expect("start an active Turn for Agent output");
    assert_eq!(
        active_update.revision,
        SessionRevision(settled.revision.0 + 1)
    );
    assert_eq!(
        timeout(PROGRESS_DEADLINE, subscription.next())
            .await
            .expect("active Turn update arrives")
            .expect("Session stream remains open")
            .expect("active Turn update is valid"),
        SessionEvent::Updated(active_update)
    );

    let output = server.agent_output();
    let expected = [
        AgentOutput::MessageStarted {
            message_id,
            turn_id,
        },
        AgentOutput::MessageDelta {
            message_id,
            content: "Hello".to_owned(),
        },
        AgentOutput::MessageDelta {
            message_id,
            content: " world".to_owned(),
        },
        AgentOutput::MessageCompleted { message_id },
    ];
    let mut published = Vec::new();
    for (index, event) in expected.into_iter().enumerate() {
        let update = output
            .emit(session_id, event)
            .await
            .expect("publish provider-neutral Agent output");
        assert_eq!(
            update.revision,
            SessionRevision(settled.revision.0 + index as u64 + 2)
        );
        published.push(update);
    }
    for expected_update in published {
        assert_eq!(
            timeout(PROGRESS_DEADLINE, subscription.next())
                .await
                .expect("Session update arrives")
                .expect("Session stream remains open")
                .expect("Session update is valid"),
            SessionEvent::Updated(expected_update)
        );
    }

    let mut reconnected = client
        .subscribe_session(session_id)
        .await
        .expect("reconnect to completed Session");
    let SessionEvent::Snapshot(completed) = timeout(PROGRESS_DEADLINE, reconnected.next())
        .await
        .expect("fresh completed snapshot arrives")
        .expect("reconnected Session stream remains open")
        .expect("completed Session snapshot is valid")
    else {
        panic!("reconnected Session must begin with a snapshot");
    };
    let agent_messages = completed
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::Agent)
        .collect::<Vec<_>>();
    assert_eq!(
        agent_messages.len(),
        1,
        "chunks must not create Message rows"
    );
    assert_eq!(agent_messages[0].id, message_id);
    assert_eq!(agent_messages[0].content, "Hello world");
    assert_eq!(agent_messages[0].status, MessageStatus::Completed);
    assert_eq!(completed.revision, SessionRevision(settled.revision.0 + 5));

    drop(reconnected);
    drop(subscription);
    drop(client);
    drop(output);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn active_session_stream_does_not_delay_graceful_server_shutdown() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "session-shutdown-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let client = reqwest::Client::new();
    let created = client
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&CreateSessionRequest {
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
        .send()
        .await
        .expect("create Session")
        .error_for_status()
        .expect("Session creation succeeds")
        .json::<SessionSnapshot>()
        .await
        .expect("decode created Session");
    let response = client
        .get(format!(
            "{}/v1/sessions/{}/events",
            descriptor.base_url, created.session.id
        ))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("open Session stream")
        .error_for_status()
        .expect("Session stream authenticates");
    let mut events = response.bytes_stream().eventsource();
    timeout(PROGRESS_DEADLINE, events.next())
        .await
        .expect("Session snapshot arrives")
        .expect("Session stream remains open")
        .expect("decode Session snapshot event");

    timeout(PROGRESS_DEADLINE, server.shutdown())
        .await
        .expect("active Session stream does not delay graceful shutdown")
        .expect("shut down server");
    assert!(
        timeout(PROGRESS_DEADLINE, events.next())
            .await
            .expect("Session stream closes on shutdown")
            .is_none()
    );
}
