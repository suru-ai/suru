use std::{convert::Infallible, sync::Arc};

use axum::{
    Json, Router,
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response, sse::Event, sse::Sse},
    routing::get,
};
use chidori::{
    build_identity,
    managed_client::{ManagedClient, ManagedClientConfig, SessionEvent},
    protocol::{
        Activity, ActivityId, ActivityKind, AdmitPromptRequest, CounterSnapshot,
        CreateSessionRequest, InitialPrompt, LifecycleState, Message, MessageId, MessageRole,
        PROTOCOL_VERSION, Prompt, PromptId, PromptStatus, RuntimeDescriptor,
        SESSION_SNAPSHOT_EVENT, SESSION_UPDATED_EVENT, SNAPSHOT_EVENT, ServerIdentity, Session,
        SessionChange, SessionError, SessionErrorCode, SessionId, SessionRevision, SessionSnapshot,
        SessionStatus, SessionUpdate, Turn, TurnId, TurnStatus, Workspace,
    },
    server::{self, ServerConfig},
};
use eventsource_stream::Eventsource;
use futures_util::{StreamExt, stream};
use tokio::time::{Duration, timeout};

#[tokio::test]
async fn authenticated_first_prompt_atomically_creates_a_failed_session_turn() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace_parent = tempfile::tempdir().expect("create workspace parent");
    let workspace = workspace_parent.path().join("workspace");
    std::fs::create_dir(&workspace).expect("create workspace");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "session-create-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let prompt_id = PromptId::new();

    let response = reqwest::Client::new()
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&CreateSessionRequest {
            workspace: Workspace {
                path: workspace_parent.path().join(".").join("workspace"),
            },
            prompt: InitialPrompt {
                id: prompt_id,
                text: "Explain this workspace".to_owned(),
            },
        })
        .send()
        .await
        .expect("create Session");

    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let snapshot = response
        .json::<SessionSnapshot>()
        .await
        .expect("decode created Session snapshot");
    assert_eq!(
        snapshot.session.workspace.path,
        std::fs::canonicalize(&workspace).expect("canonicalize expected Workspace")
    );
    assert_eq!(snapshot.session.agent, None);
    assert_eq!(snapshot.session.status, SessionStatus::Idle);
    assert_eq!(snapshot.prompts.len(), 1);
    assert_eq!(snapshot.prompts[0].id, prompt_id);
    assert_eq!(snapshot.prompts[0].status, PromptStatus::Delivered);
    assert_eq!(snapshot.turns.len(), 1);
    assert_eq!(snapshot.turns[0].prompt_id, prompt_id);
    assert_eq!(snapshot.turns[0].status, TurnStatus::Failed);
    assert_eq!(snapshot.messages.len(), 1);
    assert_eq!(snapshot.messages[0].role, MessageRole::User);
    assert_eq!(snapshot.messages[0].content, "Explain this workspace");
    assert_eq!(snapshot.activities.len(), 1);
    assert_eq!(snapshot.activities[0].kind, ActivityKind::Error);
    assert!(snapshot.activities[0].text.contains("No Agent"));
    assert!(
        snapshot
            .messages
            .iter()
            .all(|message| message.role != MessageRole::Agent),
        "an unavailable Agent must not be represented by a synthetic Agent Message"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn client_generated_prompt_ids_make_session_creation_retries_idempotent() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid workspace");
    let other_workspace = tempfile::tempdir().expect("create second workspace");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "session-create-idempotency-test")
            .expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let client = reqwest::Client::new();
    let prompt_id = PromptId::new();
    let request = CreateSessionRequest {
        workspace: Workspace {
            path: workspace.path().to_owned(),
        },
        prompt: InitialPrompt {
            id: prompt_id,
            text: "Explain this workspace".to_owned(),
        },
    };

    let first = client
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&request)
        .send()
        .await
        .expect("create Session");
    assert_eq!(first.status(), reqwest::StatusCode::CREATED);
    let first = first
        .json::<SessionSnapshot>()
        .await
        .expect("decode created Session");

    let exact_retry = client
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&request)
        .send()
        .await
        .expect("retry Session creation");
    assert_eq!(exact_retry.status(), reqwest::StatusCode::OK);
    assert_eq!(
        exact_retry
            .json::<SessionSnapshot>()
            .await
            .expect("decode retried Session"),
        first
    );

    for conflicting in [
        CreateSessionRequest {
            workspace: request.workspace.clone(),
            prompt: InitialPrompt {
                id: prompt_id,
                text: "Different content".to_owned(),
            },
        },
        CreateSessionRequest {
            workspace: Workspace {
                path: other_workspace.path().to_owned(),
            },
            prompt: request.prompt.clone(),
        },
    ] {
        let response = client
            .post(format!("{}/v1/sessions", descriptor.base_url))
            .bearer_auth(&descriptor.token)
            .json(&conflicting)
            .send()
            .await
            .expect("reuse Prompt identity with conflicting creation metadata");
        assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
        assert_eq!(
            response
                .json::<SessionError>()
                .await
                .expect("decode Prompt conflict")
                .code,
            SessionErrorCode::PromptConflict
        );
    }

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn admitted_steers_stream_once_and_exact_retries_do_not_duplicate_them() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid workspace");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "prompt-admission-idempotency-test")
            .expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let client = reqwest::Client::new();
    let created = client
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&CreateSessionRequest {
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Initial Prompt".to_owned(),
            },
        })
        .send()
        .await
        .expect("create Session")
        .error_for_status()
        .expect("Session creation succeeds")
        .json::<SessionSnapshot>()
        .await
        .expect("decode Session");
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
    timeout(Duration::from_secs(1), events.next())
        .await
        .expect("Session snapshot arrives")
        .expect("Session stream remains open")
        .expect("decode Session snapshot event");

    let prompt_id = PromptId::new();
    let command = AdmitPromptRequest {
        prompt: InitialPrompt {
            id: prompt_id,
            text: "Use the smaller interface".to_owned(),
        },
    };
    let admitted = client
        .post(format!(
            "{}/v1/sessions/{}/prompts",
            descriptor.base_url, created.session.id
        ))
        .bearer_auth(&descriptor.token)
        .json(&command)
        .send()
        .await
        .expect("admit steer");
    assert_eq!(admitted.status(), reqwest::StatusCode::CREATED);
    let admitted = admitted
        .json::<Prompt>()
        .await
        .expect("decode admitted Prompt");
    assert_eq!(admitted.id, prompt_id);
    assert_eq!(admitted.status, PromptStatus::Delivered);

    let update_event = timeout(Duration::from_secs(1), events.next())
        .await
        .expect("Session update arrives")
        .expect("Session stream remains open")
        .expect("decode Session update event");
    assert_eq!(update_event.event, SESSION_UPDATED_EVENT);
    let update = serde_json::from_str::<SessionUpdate>(&update_event.data)
        .expect("decode streamed Session update");
    assert_eq!(update.revision, SessionRevision(2));
    assert!(update.changes.iter().any(
        |change| matches!(change, SessionChange::PromptAdded { prompt } if prompt.id == prompt_id)
    ));
    let turn_id = update
        .changes
        .iter()
        .find_map(|change| match change {
            SessionChange::TurnAdded { turn } if turn.prompt_id == prompt_id => Some(turn.id),
            _ => None,
        })
        .expect("delivered steer creates a Turn");
    assert!(update.changes.iter().any(|change| {
        matches!(change, SessionChange::MessageAdded { message }
            if message.turn_id == turn_id && message.content == command.prompt.text)
    }));

    let exact_retry = client
        .post(format!(
            "{}/v1/sessions/{}/prompts",
            descriptor.base_url, created.session.id
        ))
        .bearer_auth(&descriptor.token)
        .json(&command)
        .send()
        .await
        .expect("retry steer admission");
    assert_eq!(exact_retry.status(), reqwest::StatusCode::OK);
    assert_eq!(
        exact_retry
            .json::<Prompt>()
            .await
            .expect("decode retried Prompt"),
        admitted
    );
    assert!(
        timeout(Duration::from_millis(100), events.next())
            .await
            .is_err(),
        "an exact retry must not emit a duplicate Session update"
    );

    let conflicting = client
        .post(format!(
            "{}/v1/sessions/{}/prompts",
            descriptor.base_url, created.session.id
        ))
        .bearer_auth(&descriptor.token)
        .json(&AdmitPromptRequest {
            prompt: InitialPrompt {
                id: prompt_id,
                text: "Conflicting content".to_owned(),
            },
        })
        .send()
        .await
        .expect("reuse Prompt identity with conflicting content");
    assert_eq!(conflicting.status(), reqwest::StatusCode::CONFLICT);
    assert_eq!(
        conflicting
            .json::<SessionError>()
            .await
            .expect("decode Prompt conflict")
            .code,
        SessionErrorCode::PromptConflict
    );

    drop(events);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn invalid_workspace_and_blank_prompt_are_rejected_before_session_creation() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid workspace");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "session-validation-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let client = reqwest::Client::new();

    let unauthenticated = client
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .json(&CreateSessionRequest {
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Explain this workspace".to_owned(),
            },
        })
        .send()
        .await
        .expect("create Session without authentication");
    assert_eq!(unauthenticated.status(), reqwest::StatusCode::UNAUTHORIZED);

    let blank = client
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&CreateSessionRequest {
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: " \n\t ".to_owned(),
            },
        })
        .send()
        .await
        .expect("submit blank Prompt");
    assert_eq!(blank.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        blank
            .json::<SessionError>()
            .await
            .expect("decode blank Prompt error")
            .code,
        SessionErrorCode::EmptyPrompt
    );

    let missing_workspace = client
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&CreateSessionRequest {
            workspace: Workspace {
                path: workspace.path().join("missing"),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Explain this workspace".to_owned(),
            },
        })
        .send()
        .await
        .expect("submit invalid Workspace");
    assert_eq!(
        missing_workspace.status(),
        reqwest::StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        missing_workspace
            .json::<SessionError>()
            .await
            .expect("decode invalid Workspace error")
            .code,
        SessionErrorCode::InvalidWorkspace
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn authenticated_session_stream_starts_with_a_complete_revisioned_snapshot() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid workspace");
    let server = server::spawn(
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
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Explain this workspace".to_owned(),
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
    let first = timeout(Duration::from_secs(1), events.next())
        .await
        .expect("Session snapshot arrives")
        .expect("Session stream remains open")
        .expect("decode Session SSE event");

    assert_eq!(first.event, SESSION_SNAPSHOT_EVENT);
    assert_eq!(first.id, created.revision.0.to_string());
    assert_eq!(
        serde_json::from_str::<SessionSnapshot>(&first.data)
            .expect("decode Session snapshot event"),
        created
    );

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
    let fresh_snapshot = timeout(Duration::from_secs(1), reconnected_events.next())
        .await
        .expect("fresh Session snapshot arrives")
        .expect("reconnected Session stream remains open")
        .expect("decode reconnected Session SSE event");
    assert_eq!(fresh_snapshot.event, SESSION_SNAPSHOT_EVENT);
    assert_eq!(fresh_snapshot.id, created.revision.0.to_string());

    drop(reconnected_events);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn active_session_stream_does_not_delay_graceful_server_shutdown() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid workspace");
    let server = server::spawn(
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
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Explain this workspace".to_owned(),
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
    timeout(Duration::from_secs(1), events.next())
        .await
        .expect("Session snapshot arrives")
        .expect("Session stream remains open")
        .expect("decode Session snapshot event");

    timeout(Duration::from_secs(1), server.shutdown())
        .await
        .expect("active Session stream does not delay graceful shutdown")
        .expect("shut down server");
    assert!(
        timeout(Duration::from_secs(1), events.next())
            .await
            .expect("Session stream closes on shutdown")
            .is_none()
    );
}

#[tokio::test]
async fn managed_clients_can_reconnect_to_a_session_that_outlives_its_first_client() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid workspace");
    let server = server::spawn(
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
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Explain this workspace".to_owned(),
            },
        })
        .await
        .expect("create Session through managed client");
    let mut first_subscription = first_client
        .subscribe_session(created.session.id)
        .await
        .expect("subscribe through first managed client");
    assert_eq!(
        first_subscription
            .next()
            .await
            .expect("first Session event arrives")
            .expect("first Session event is valid"),
        SessionEvent::Snapshot(created.clone())
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
        .subscribe_session(created.session.id)
        .await
        .expect("reconnect to existing Session");
    assert_eq!(
        second_subscription
            .next()
            .await
            .expect("reconnected Session event arrives")
            .expect("reconnected Session event is valid"),
        SessionEvent::Snapshot(created)
    );

    drop(second_subscription);
    drop(second_client);
    server.shutdown().await.expect("shut down server");
}

async fn receive_managed_client_initial_state(client: &mut ManagedClient) {
    assert!(matches!(
        client.next().await,
        Some(chidori::managed_client::ManagedEvent::Connecting)
    ));
    assert!(matches!(
        client.next().await,
        Some(chidori::managed_client::ManagedEvent::Connected(_))
    ));
    assert!(matches!(
        client.next().await,
        Some(chidori::managed_client::ManagedEvent::Snapshot(_))
    ));
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
        .subscribe_session(session_id)
        .await
        .expect("subscribe to fixture Session");

    assert_eq!(
        subscription
            .next()
            .await
            .expect("Session snapshot arrives")
            .expect("Session snapshot is valid"),
        SessionEvent::Snapshot(snapshot)
    );
    let error = subscription
        .next()
        .await
        .expect("invalid Session update arrives")
        .expect_err("duplicate revision must be rejected");
    assert!(error.contains("revision is not monotonic"));

    drop(subscription);
    drop(client);
    drop(fixture);
}

fn failed_session_snapshot(session_id: SessionId, workspace: &std::path::Path) -> SessionSnapshot {
    let prompt_id = PromptId::new();
    let turn_id = TurnId::new();
    SessionSnapshot {
        session: Session {
            id: session_id,
            workspace: Workspace {
                path: workspace.to_owned(),
            },
            agent: None,
            status: SessionStatus::Idle,
        },
        revision: SessionRevision::INITIAL,
        prompts: vec![Prompt {
            id: prompt_id,
            text: "Explain this workspace".to_owned(),
            status: PromptStatus::Delivered,
        }],
        turns: vec![Turn {
            id: turn_id,
            prompt_id,
            status: TurnStatus::Failed,
        }],
        messages: vec![Message {
            id: MessageId::new(),
            turn_id,
            role: MessageRole::User,
            content: "Explain this workspace".to_owned(),
        }],
        activities: vec![Activity {
            id: ActivityId::new(),
            turn_id,
            kind: ActivityKind::Error,
            text: "No Agent is selected".to_owned(),
        }],
    }
}

#[derive(Clone)]
struct MalformedSessionStreamState {
    descriptor: RuntimeDescriptor,
    snapshot: SessionSnapshot,
}

struct MalformedSessionStreamFixture {
    task: tokio::task::JoinHandle<()>,
}

impl MalformedSessionStreamFixture {
    async fn spawn(state_dir: &std::path::Path, channel: &str, snapshot: SessionSnapshot) -> Self {
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
        });
        let app = Router::new()
            .route("/health", get(malformed_fixture_health))
            .route("/v1/events", get(malformed_fixture_server_events))
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
    let snapshot = CounterSnapshot {
        instance_id: state.descriptor.instance_id,
        value: 0,
        revision: 0,
    };
    let first = stream::once(async move {
        Ok::<_, Infallible>(
            Event::default()
                .event(SNAPSHOT_EVENT)
                .id("0")
                .json_data(snapshot)
                .expect("serialize fixture server snapshot"),
        )
    });
    Sse::new(first.chain(stream::pending())).into_response()
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
