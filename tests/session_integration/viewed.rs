//! Server-owned Viewed state, exercised through the public session and catalog protocol.

use crate::{
    failing_provider_support::spawn_with_failing_provider,
    server_support::{next_catalog_change, open_catalog_stream},
    support::{create_session, receive_managed_client_initial_state},
};
use futures_util::StreamExt;
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, ManagedEvent},
    protocol::{
        CreateSessionRequest, InitialPrompt, PromptId, SessionCatalogChange, SessionError,
        SessionErrorCode, SessionId, SessionListItem, SessionStandingInputs, SessionSummary,
        ViewSessionOperationId, ViewSessionRequest, Workspace,
    },
    server::ServerConfig,
};
use tokio::time::{Duration, timeout};

fn create_request(workspace: &std::path::Path) -> CreateSessionRequest {
    CreateSessionRequest {
        agent_selection: None,
        workspace: Workspace {
            path: workspace.to_owned(),
        },
        prompt: InitialPrompt {
            id: PromptId::new(),
            text: "Remember that I opened this".to_owned(),
            skill_invocations: Vec::new(),
        },
    }
}

#[tokio::test]
async fn viewing_a_session_the_server_does_not_hold_is_rejected() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "unknown-session-viewed-test")
            .expect("configure server"),
    )
    .await
    .expect("spawn server");

    let response = view(server.descriptor(), SessionId::new()).await;
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    assert_eq!(
        response
            .json::<SessionError>()
            .await
            .expect("decode the rejection")
            .code,
        SessionErrorCode::SessionNotFound
    );

    server.shutdown().await.expect("stop server");
}

async fn view(
    descriptor: &suru::protocol::RuntimeDescriptor,
    session_id: SessionId,
) -> reqwest::Response {
    view_with_operation(descriptor, session_id, ViewSessionOperationId::new()).await
}

async fn view_with_operation(
    descriptor: &suru::protocol::RuntimeDescriptor,
    session_id: SessionId,
    operation_id: ViewSessionOperationId,
) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!(
            "{}/v1/sessions/{session_id}/viewed",
            descriptor.base_url
        ))
        .bearer_auth(&descriptor.token)
        .json(&ViewSessionRequest { operation_id })
        .send()
        .await
        .expect("send Session viewed command")
}

#[tokio::test]
async fn replaying_a_viewed_operation_does_not_move_viewed_or_announce_it_again() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "idempotent-session-viewed-test")
            .expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let mut catalog = open_catalog_stream(&descriptor).await;
    let session_id = create_session(&descriptor, &create_request(workspace.path()))
        .await
        .session
        .id;
    assert_eq!(
        next_catalog_change(&mut catalog).await,
        SessionCatalogChange::Created { session_id }
    );

    let first_operation = ViewSessionOperationId::new();
    let first = view_with_operation(&descriptor, session_id, first_operation)
        .await
        .error_for_status()
        .expect("first Viewed operation succeeds")
        .json::<SessionSummary>()
        .await
        .expect("decode first Viewed result");
    let first_viewed = first
        .standing_inputs
        .viewed_at
        .expect("first Viewed moment");
    while !matches!(
        next_catalog_change(&mut catalog).await,
        SessionCatalogChange::StandingInputsChanged { inputs, .. }
            if inputs.viewed_at == Some(first_viewed)
    ) {}

    let second = view(&descriptor, session_id)
        .await
        .error_for_status()
        .expect("second Viewed operation succeeds")
        .json::<SessionSummary>()
        .await
        .expect("decode second Viewed result");
    let second_viewed = second
        .standing_inputs
        .viewed_at
        .expect("second Viewed moment");
    assert!(second_viewed > first_viewed);
    while !matches!(
        next_catalog_change(&mut catalog).await,
        SessionCatalogChange::StandingInputsChanged { inputs, .. }
            if inputs.viewed_at == Some(second_viewed)
    ) {}

    let replayed = view_with_operation(&descriptor, session_id, first_operation)
        .await
        .error_for_status()
        .expect("replayed Viewed operation succeeds")
        .json::<SessionSummary>()
        .await
        .expect("decode replayed Viewed result");
    assert_eq!(
        replayed.standing_inputs.viewed_at,
        Some(second_viewed),
        "a stale retry cannot clear a later outcome by minting a new moment"
    );
    assert!(
        timeout(Duration::from_millis(50), catalog.next())
            .await
            .is_err(),
        "an idempotent replay publishes no catalog change"
    );

    server.shutdown().await.expect("stop server");
}

async fn listed_summary(
    descriptor: &suru::protocol::RuntimeDescriptor,
    session_id: SessionId,
) -> SessionSummary {
    reqwest::Client::new()
        .get(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("list Sessions")
        .error_for_status()
        .expect("Session listing succeeds")
        .json::<Vec<SessionListItem>>()
        .await
        .expect("decode Session listing")
        .into_iter()
        .find_map(|item| {
            (item.id() == session_id)
                .then(|| item.readable().cloned())
                .flatten()
        })
        .expect("the readable Session remains listed")
}

async fn next_viewed(client: &mut ManagedClient, viewed_at: suru::protocol::SessionTimestamp) {
    timeout(Duration::from_secs(5), async {
        loop {
            let event = client
                .next()
                .await
                .expect("managed Client remains connected");
            if matches!(
                event,
                ManagedEvent::SessionStandingInputsChanged(changed)
                    if changed.inputs.viewed_at == Some(viewed_at)
            ) {
                return;
            }
        }
    })
    .await
    .expect("Viewed reaches the managed Client");
}

#[tokio::test]
async fn viewing_a_session_stamps_its_summary_and_announces_the_reading() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "session-viewed-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let mut catalog = open_catalog_stream(&descriptor).await;
    let session_id = create_session(&descriptor, &create_request(workspace.path()))
        .await
        .session
        .id;
    assert_eq!(
        next_catalog_change(&mut catalog).await,
        SessionCatalogChange::Created { session_id }
    );

    let viewed = view(&descriptor, session_id)
        .await
        .error_for_status()
        .expect("Session viewed command succeeds")
        .json::<SessionSummary>()
        .await
        .expect("decode viewed Session summary");
    let viewed_at = viewed
        .standing_inputs
        .viewed_at
        .expect("viewing stamps the Server's clock");
    assert_eq!(
        listed_summary(&descriptor, session_id)
            .await
            .standing_inputs
            .viewed_at,
        Some(viewed_at),
        "the readable listing carries the same Viewed moment"
    );
    let announced = loop {
        let change = next_catalog_change(&mut catalog).await;
        if matches!(
            &change,
            SessionCatalogChange::StandingInputsChanged { inputs, .. }
                if inputs.viewed_at == Some(viewed_at)
        ) {
            break change;
        }
    };
    assert_eq!(
        announced,
        SessionCatalogChange::StandingInputsChanged {
            session_id,
            inputs: SessionStandingInputs {
                pending_questionnaires: Vec::new(),
                submitting_questionnaires: Vec::new(),
                pending_questionnaires_revision: suru::protocol::SessionRevision(0),
                subagent_questionnaires: Vec::new(),
                latest_turn: viewed.standing_inputs.latest_turn,
                viewed_at: Some(viewed_at),
            },
        },
        "every client listing the Session receives its new Viewed moment"
    );

    server.shutdown().await.expect("stop server");
}

#[tokio::test]
async fn viewed_survives_a_server_replacement() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let config = ServerConfig::new(state_dir.path(), "session-viewed-restart-test")
        .expect("configure original server")
        .with_data_dir(data_dir.path());
    let original = spawn_with_failing_provider(config.clone())
        .await
        .expect("spawn original server");
    let session_id = create_session(original.descriptor(), &create_request(workspace.path()))
        .await
        .session
        .id;
    let viewed_at = view(original.descriptor(), session_id)
        .await
        .error_for_status()
        .expect("Session viewed command succeeds")
        .json::<SessionSummary>()
        .await
        .expect("decode viewed Session summary")
        .standing_inputs
        .viewed_at
        .expect("viewing stamps the Server's clock");
    original.shutdown().await.expect("stop original server");

    let replacement = spawn_with_failing_provider(config)
        .await
        .expect("spawn replacement server");
    assert_eq!(
        listed_summary(replacement.descriptor(), session_id)
            .await
            .standing_inputs
            .viewed_at,
        Some(viewed_at),
        "the Viewed moment outlives the Server that stamped it"
    );

    replacement
        .shutdown()
        .await
        .expect("stop replacement server");
}

#[tokio::test]
async fn one_clients_viewed_request_reaches_every_client_listing_the_server() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let instance = "shared-session-viewed-test";
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), instance).expect("configure server"),
    )
    .await
    .expect("spawn server");
    let mut first = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), instance).expect("configure first Client"),
    )
    .await
    .expect("connect first Client");
    let mut second = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), instance).expect("configure second Client"),
    )
    .await
    .expect("connect second Client");
    receive_managed_client_initial_state(&mut first).await;
    receive_managed_client_initial_state(&mut second).await;
    let session_id = first
        .create_session(create_request(workspace.path()))
        .await
        .expect("create Session")
        .session
        .id;

    let viewed_at = first
        .view_session(
            session_id,
            ViewSessionRequest {
                operation_id: ViewSessionOperationId::new(),
            },
        )
        .await
        .expect("report Session Viewed")
        .standing_inputs
        .viewed_at
        .expect("the Server stamps Viewed");
    next_viewed(&mut first, viewed_at).await;
    next_viewed(&mut second, viewed_at).await;

    drop(first);
    drop(second);
    server.shutdown().await.expect("stop server");
}
