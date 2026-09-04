//! A Session's own Settle: the reversible marker that sets it aside as done
//! for now, mutated over the protocol and cleared by the next Prompt.

use crate::{
    failing_provider_support::spawn_with_failing_provider,
    server_support::{next_catalog_change, open_catalog_stream},
    support::{create_session, receive_managed_client_initial_state},
};
use diesel::{Connection, SqliteConnection, connection::SimpleConnection};
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, ManagedEvent},
    protocol::{
        AdmitPromptRequest, CreateSessionRequest, InitialPrompt, PromptDelivery, PromptId,
        RuntimeDescriptor, SessionCatalogChange, SessionError, SessionErrorCode, SessionId,
        SessionListItem, SessionSettlementChanged, SessionSummary, SessionTimestamp,
        SettleSessionRequest, Workspace,
    },
    server::ServerConfig,
};
use tokio::time::{Duration, timeout};

fn create_request(workspace: &std::path::Path, prompt: &str) -> CreateSessionRequest {
    CreateSessionRequest {
        // No Agent Selection, so no Title Errand runs and the catalog stream
        // carries nothing but what settling puts on it.
        agent_selection: None,
        workspace: Workspace {
            path: workspace.to_owned(),
        },
        prompt: InitialPrompt {
            id: PromptId::new(),
            text: prompt.to_owned(),
            skill_invocations: Vec::new(),
        },
    }
}

async fn settle(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
    settled: bool,
) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!(
            "{}/v1/sessions/{session_id}/settlement",
            descriptor.base_url
        ))
        .bearer_auth(&descriptor.token)
        .json(&SettleSessionRequest { settled })
        .send()
        .await
        .expect("send Session settlement command")
}

async fn settled_summary(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
    settled: bool,
) -> SessionSummary {
    settle(descriptor, session_id, settled)
        .await
        .error_for_status()
        .expect("Session settlement succeeds")
        .json::<SessionSummary>()
        .await
        .expect("decode the settled Session summary")
}

/// The one Session in a listing, whichever readability it came back with.
async fn listed(descriptor: &RuntimeDescriptor, session_id: SessionId) -> SessionListItem {
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
        .expect("decode the Session listing")
        .into_iter()
        .find(|item| item.id() == session_id)
        .expect("the Session remains listed")
}

#[tokio::test]
async fn settling_a_session_marks_its_summary_and_unsettling_clears_it() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "session-settle-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let created = create_session(
        &descriptor,
        &create_request(workspace.path(), "Ship the seam"),
    )
    .await
    .session
    .id;

    assert_eq!(
        listed(&descriptor, created).await.settled_at(),
        None,
        "a Session nobody set aside is active"
    );

    let active = listed(&descriptor, created).await;
    let settled = settled_summary(&descriptor, created, true).await;
    let marked_at = settled
        .settled_at
        .expect("settling stamps the moment it happened");
    assert_eq!(
        settled.updated_at,
        active.updated_at(),
        "setting work aside is a judgement about it rather than work on it, so \
         the shelf reads the marker's own stamp and last activity stands still"
    );
    assert_eq!(
        listed(&descriptor, created).await.settled_at(),
        Some(marked_at),
        "the listing carries the marker the mutation set"
    );

    let unsettled = settled_summary(&descriptor, created, false).await;
    assert_eq!(unsettled.settled_at, None);
    assert_eq!(listed(&descriptor, created).await.settled_at(), None);
    assert!(
        unsettled.updated_at > settled.updated_at,
        "a user reaching for work they set aside is the latest thing to happen \
         to it, so a client deriving settlement from idle reads it as active"
    );

    server.shutdown().await.expect("stop server");
}

#[tokio::test]
async fn settling_an_already_settled_session_keeps_the_moment_it_was_set_aside() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "session-resettle-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let created = create_session(
        &descriptor,
        &create_request(workspace.path(), "Ship the seam"),
    )
    .await
    .session
    .id;

    let first = settled_summary(&descriptor, created, true).await;
    let again = settled_summary(&descriptor, created, true).await;

    assert_eq!(
        again.settled_at, first.settled_at,
        "saying it twice does not move when the work was set aside"
    );

    server.shutdown().await.expect("stop server");
}

#[tokio::test]
async fn settling_a_session_this_server_does_not_hold_is_rejected() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "session-settle-missing-test")
            .expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();

    let response = settle(&descriptor, SessionId::new(), true).await;

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

#[tokio::test]
async fn a_settled_session_is_still_settled_after_a_server_replacement() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let config = ServerConfig::new(state_dir.path(), "session-settle-restart-test")
        .expect("configure original server")
        .with_data_dir(data_dir.path());
    let original = spawn_with_failing_provider(config.clone())
        .await
        .expect("spawn original server");
    let created = create_session(
        original.descriptor(),
        &create_request(workspace.path(), "Ship the seam"),
    )
    .await
    .session
    .id;
    let marked_at = settled_summary(original.descriptor(), created, true)
        .await
        .settled_at
        .expect("settling stamps the moment it happened");
    original.shutdown().await.expect("stop original server");

    let replacement = spawn_with_failing_provider(config)
        .await
        .expect("spawn replacement server");
    assert_eq!(
        listed(replacement.descriptor(), created).await.settled_at(),
        Some(marked_at),
        "the marker outlives the server that set it"
    );

    replacement
        .shutdown()
        .await
        .expect("stop replacement server");
}

#[tokio::test]
async fn admitting_a_prompt_to_a_settled_session_makes_it_active_again() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "session-settle-prompt-test")
            .expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let created = create_session(
        &descriptor,
        &create_request(workspace.path(), "Ship the seam"),
    )
    .await
    .session
    .id;
    settled_summary(&descriptor, created, true).await;

    reqwest::Client::new()
        .post(format!(
            "{}/v1/sessions/{created}/prompts",
            descriptor.base_url
        ))
        .bearer_auth(&descriptor.token)
        .json(&AdmitPromptRequest {
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "One more thing".to_owned(),
                skill_invocations: Vec::new(),
            },
            delivery: PromptDelivery::Queue,
        })
        .send()
        .await
        .expect("admit a Prompt to the settled Session")
        .error_for_status()
        .expect("Prompt admission succeeds");

    assert_eq!(
        listed(&descriptor, created).await.settled_at(),
        None,
        "prompting a Session set aside brings it back"
    );

    server.shutdown().await.expect("stop server");
}

#[tokio::test]
async fn settlement_changes_are_announced_on_the_session_catalog_stream() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "session-settle-stream-test")
            .expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let mut catalog = open_catalog_stream(&descriptor).await;

    let created = create_session(
        &descriptor,
        &create_request(workspace.path(), "Ship the seam"),
    )
    .await
    .session
    .id;
    assert_eq!(
        next_catalog_change(&mut catalog).await,
        SessionCatalogChange::Created {
            session_id: created
        }
    );

    let marked_at = settled_summary(&descriptor, created, true)
        .await
        .settled_at
        .expect("settling stamps the moment it happened");
    timeout(Duration::from_secs(1), async {
        loop {
            if next_catalog_change(&mut catalog).await
                == (SessionCatalogChange::SettlementChanged {
                    session_id: created,
                    settled_at: Some(marked_at),
                })
            {
                return;
            }
        }
    })
    .await
    .expect("the Session settlement change follows any Turn settlement changes");

    // Saying it twice changes nothing, so it announces nothing: the very next
    // change on the stream is the one that follows it.
    settled_summary(&descriptor, created, true).await;
    settled_summary(&descriptor, created, false).await;
    assert_eq!(
        next_catalog_change(&mut catalog).await,
        SessionCatalogChange::SettlementChanged {
            session_id: created,
            settled_at: None,
        }
    );

    server.shutdown().await.expect("stop server");
}

#[tokio::test]
async fn a_managed_client_hears_settlement_changes_for_sessions_it_never_opened() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "session-settle-managed-test")
            .expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let created = create_session(
        &descriptor,
        &create_request(workspace.path(), "Ship the seam"),
    )
    .await
    .session
    .id;

    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "session-settle-managed-test")
            .expect("configure managed client"),
    )
    .await
    .expect("connect managed client");
    receive_managed_client_initial_state(&mut client).await;

    let marked_at = client
        .settle_session(created, true)
        .await
        .expect("settle the Session over the managed client")
        .settled_at
        .expect("settling stamps the moment it happened");

    assert_eq!(
        timeout(Duration::from_secs(5), client.next())
            .await
            .expect("the settlement reaches the client"),
        Some(ManagedEvent::SessionSettlementChanged(
            SessionSettlementChanged {
                session_id: created,
                settled_at: Some(marked_at),
            }
        ))
    );

    server.shutdown().await.expect("stop server");
}

/// A moment far enough ahead of any wall clock a test machine could be running
/// that only a restored marker can put it in the store's hands.
const AFTER_ANY_CLOCK: i64 = 4_102_444_800_000;

/// A Settle is the one moment the store mints without a commit behind it, so it
/// can stand later than every `updated_at` in the database. A restored store
/// resumes past it too, or the marker it hands out next would be one a Session
/// already wears.
#[tokio::test]
async fn a_restored_marker_carries_the_store_clock_past_it() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let config = ServerConfig::new(state_dir.path(), "session-settle-clock-test")
        .expect("configure original server")
        .with_data_dir(data_dir.path());
    let original = spawn_with_failing_provider(config.clone())
        .await
        .expect("spawn original server");
    let created = create_session(
        original.descriptor(),
        &create_request(workspace.path(), "Ship the seam"),
    )
    .await
    .session
    .id;
    settled_summary(original.descriptor(), created, true).await;
    original.shutdown().await.expect("stop original server");

    // Stand the stored marker beyond any moment the replacement's own clock
    // could reach, so what it stamps next can only have come from reading it.
    let database_path = config.data_dir().join("suru.db");
    let mut database = SqliteConnection::establish(
        database_path
            .to_str()
            .expect("fixture database path is valid UTF-8"),
    )
    .expect("open the persisted Session fixture");
    database
        .batch_execute(&format!(
            "UPDATE sessions SET settled_at = {AFTER_ANY_CLOCK};"
        ))
        .expect("stand the stored marker beyond the clock");
    drop(database);

    let replacement = spawn_with_failing_provider(config)
        .await
        .expect("spawn replacement server");
    assert_eq!(
        listed(replacement.descriptor(), created).await.settled_at(),
        Some(SessionTimestamp(AFTER_ANY_CLOCK as u64)),
        "the replacement restored the marker it was given"
    );
    settled_summary(replacement.descriptor(), created, false).await;
    let next = settled_summary(replacement.descriptor(), created, true)
        .await
        .settled_at
        .expect("settling stamps the moment it happened");

    assert!(
        next > SessionTimestamp(AFTER_ANY_CLOCK as u64),
        "a Settle after a restore is stamped past the marker that was restored, \
         {next:?} vs {AFTER_ANY_CLOCK}"
    );

    replacement
        .shutdown()
        .await
        .expect("stop replacement server");
}
