//! A user's own choice of a Session's Icon from the Icon Catalog, set over
//! the protocol and reused by every client the same `TitleChanged` change a
//! derived Icon publishes.

use crate::server_support::PROGRESS_DEADLINE;
use crate::{
    failing_provider_support::spawn_with_failing_provider,
    provider_support::ControlledProvider,
    server_support::{next_catalog_change_matching, next_derived_title, open_catalog_stream},
    support::{hosted_model, hosted_selection, receive_managed_client_initial_state},
};
use serde_json::json;
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, SessionEvent},
    protocol::{
        CreateSessionRequest, InitialPrompt, PromptId, ProviderId, SessionCatalogChange,
        SessionChange, SessionError, SessionErrorCode, SessionId, SessionTitleChanged,
        SetSessionIconRequest,
    },
    server::{self, ServerConfig},
};
use tokio::time::timeout;

const PROVIDER: &str = "controlled";
const MODEL: &str = "controlled-default";

fn create_request(workspace: &std::path::Path, prompt: &str) -> CreateSessionRequest {
    CreateSessionRequest {
        session_id: None,
        preparation_id: None,
        // No Agent Selection, so no Title Errand runs and the only Icon a
        // Session carries is whichever this test chooses for it.
        agent_selection: None,
        execution_directory: suru::protocol::ExecutionDirectory {
            path: workspace.to_owned(),
        },
        prompt: InitialPrompt {
            id: PromptId::new(),
            text: prompt.to_owned(),
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
        },
    }
}

fn errand_request(workspace: &std::path::Path, prompt: &str) -> CreateSessionRequest {
    CreateSessionRequest {
        session_id: None,
        preparation_id: None,
        agent_selection: Some(hosted_selection(PROVIDER, MODEL)),
        execution_directory: suru::protocol::ExecutionDirectory {
            path: workspace.to_owned(),
        },
        prompt: InitialPrompt {
            id: PromptId::new(),
            text: prompt.to_owned(),
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
        },
    }
}

fn titling_provider() -> (
    std::sync::Arc<crate::provider_support::ControlledProviderRuntime>,
    ControlledProvider,
) {
    ControlledProvider::with_provider(
        ProviderId::new(PROVIDER),
        vec![hosted_model(PROVIDER, MODEL)],
    )
}

async fn connected_client(state_dir: &std::path::Path, channel: &str) -> ManagedClient {
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir, channel).expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_managed_client_initial_state(&mut client).await;
    client
}

/// The Icon Catalog name one Session in a listing carries.
async fn listed_icon(client: &ManagedClient, session_id: SessionId) -> Option<String> {
    client
        .list_sessions(None)
        .await
        .expect("list Sessions")
        .into_iter()
        .find(|item| item.id() == session_id)
        .expect("the Session remains listed")
        .icon()
        .map(ToOwned::to_owned)
}

async fn set_icon_response(
    descriptor: &suru::protocol::RuntimeDescriptor,
    session_id: SessionId,
    icon: &str,
) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!(
            "{}/v1/sessions/{session_id}/icon",
            descriptor.base_url
        ))
        .bearer_auth(&descriptor.token)
        .json(&SetSessionIconRequest {
            icon: icon.to_owned(),
        })
        .send()
        .await
        .expect("send Session Icon command")
}

#[tokio::test]
async fn setting_an_icon_updates_the_catalog_and_the_open_session() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "icon-set-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let mut client = connected_client(state_dir.path(), "icon-set-test").await;

    let created = client
        .create_session(create_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create Session");
    let session_id = created.session.id;
    assert_eq!(created.icon, None);

    let mut subscription = client
        .subscribe_session(session_id)
        .await
        .expect("subscribe Session");
    // Drain the initial snapshot before acting, so the update the test waits
    // for below is unambiguously the Icon choice's own.
    let initial = timeout(PROGRESS_DEADLINE, subscription.next())
        .await
        .expect("initial snapshot arrives")
        .expect("stream open")
        .expect("valid snapshot");
    assert!(matches!(initial, SessionEvent::Snapshot(_)));

    let summary = client
        .set_session_icon(session_id, "md-bug")
        .await
        .expect("set the Session's Icon");
    assert_eq!(summary.icon.as_deref(), Some("md-bug"));
    assert_eq!(
        summary.title, "Explain the seam",
        "choosing an Icon carries the Title through unchanged"
    );

    assert_eq!(
        next_derived_title(&mut client).await,
        SessionTitleChanged {
            session_id,
            title: "Explain the seam".to_owned(),
            icon: Some("md-bug".to_owned()),
        },
        "the choosing client sees the same TitleChanged catalog change a derivation publishes"
    );
    assert_eq!(
        listed_icon(&client, session_id).await,
        Some("md-bug".to_owned())
    );

    timeout(PROGRESS_DEADLINE, async {
        loop {
            let event = subscription
                .next()
                .await
                .expect("stream remains open")
                .expect("valid update");
            if let SessionEvent::Updated(update) = event
                && update.changes.iter().any(|change| {
                    matches!(
                        change,
                        SessionChange::TitleChanged { title, icon }
                            if title == "Explain the seam" && icon.as_deref() == Some("md-bug")
                    )
                })
            {
                break;
            }
        }
    })
    .await
    .expect("the chosen Icon reaches the open Session stream");

    let read = client.read_session(session_id).await.expect("read Session");
    assert_eq!(read.icon.as_deref(), Some("md-bug"));

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn choosing_an_unknown_icon_is_refused_and_changes_nothing() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "icon-refusal-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let client = connected_client(state_dir.path(), "icon-refusal-test").await;

    let created = client
        .create_session(create_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create Session");
    let session_id = created.session.id;
    // Read once more before acting, rather than trusting creation's own
    // revision to still be current: nothing about this Session's setup
    // guarantees no other commit lands between creation and the refusal
    // this test provokes.
    let before = client
        .read_session(session_id)
        .await
        .expect("read Session before the refusal");

    let response = set_icon_response(server.descriptor(), session_id, "md-not-a-glyph").await;
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(
        response
            .json::<SessionError>()
            .await
            .expect("decode the rejection")
            .code,
        SessionErrorCode::InvalidIcon
    );

    let read = client.read_session(session_id).await.expect("read Session");
    assert_eq!(read.icon, None, "a refused Icon changes nothing");
    assert_eq!(read.title, "Explain the seam");
    assert_eq!(read.revision, before.revision, "no revision was committed");

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_chosen_icon_stands_against_a_later_derivation() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = titling_provider();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "icon-precedence-test").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let mut client = connected_client(state_dir.path(), "icon-precedence-test").await;

    let created = client
        .create_session(errand_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create Session");
    let session_id = created.session.id;

    // The Title Errand is spawned but has not yet answered: the Provider is
    // holding it, so the user's choice below lands first.
    let errand = timeout(PROGRESS_DEADLINE, provider.next_errand())
        .await
        .expect("an Errand reaches the Provider");

    client
        .set_session_icon(session_id, "md-bug")
        .await
        .expect("set the Session's Icon while the Errand is outstanding");
    assert_eq!(
        next_derived_title(&mut client).await,
        SessionTitleChanged {
            session_id,
            title: "Explain the seam".to_owned(),
            icon: Some("md-bug".to_owned()),
        },
        "the user's choice reaches the catalog before the Errand answers"
    );

    // The Errand now answers with a different Icon. The Title it derives
    // still lands — the Title's own guard only ever cares whether the Title
    // it was derived from still stands — but the Icon it offers is filling an
    // absence that is no longer there.
    errand.succeed(json!({
        "title": "Explain the Provider seam",
        "icon": "dev-rust",
    }));
    assert_eq!(
        next_derived_title(&mut client).await,
        SessionTitleChanged {
            session_id,
            title: "Explain the Provider seam".to_owned(),
            icon: Some("md-bug".to_owned()),
        },
        "the derived Title lands, but the chosen Icon stands against the derived one"
    );

    assert_eq!(
        listed_icon(&client, session_id).await,
        Some("md-bug".to_owned()),
        "the listing shows the chosen Icon rather than the later derivation"
    );
    let read = client.read_session(session_id).await.expect("read Session");
    assert_eq!(read.title, "Explain the Provider seam");
    assert_eq!(
        read.icon.as_deref(),
        Some("md-bug"),
        "the open Session shows the chosen Icon rather than the later derivation"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_chosen_icon_outlives_a_restart() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "icon-restart-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let client = connected_client(state_dir.path(), "icon-restart-test").await;

    let created = client
        .create_session(create_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create Session");
    let session_id = created.session.id;
    client
        .set_session_icon(session_id, "md-bug")
        .await
        .expect("set the Session's Icon");

    drop(client);
    server.shutdown().await.expect("shut down server");

    let restarted = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "icon-restart-test").expect("reconfigure server"),
    )
    .await
    .expect("respawn server");
    let client = connected_client(state_dir.path(), "icon-restart-test").await;

    assert_eq!(
        listed_icon(&client, session_id).await,
        Some("md-bug".to_owned()),
        "a chosen Icon survives a restart"
    );
    let reopened = client
        .read_session(session_id)
        .await
        .expect("reopen the Session");
    assert_eq!(reopened.icon.as_deref(), Some("md-bug"));

    restarted.shutdown().await.expect("shut down server");
}

/// Every catalog client, not only the one that chose an Icon, sees the same
/// `TitleChanged` change reach the shared Session catalog stream.
#[tokio::test]
async fn setting_an_icon_reaches_the_catalog_stream() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "icon-catalog-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let client = connected_client(state_dir.path(), "icon-catalog-test").await;
    let mut catalog = open_catalog_stream(server.descriptor()).await;

    let created = client
        .create_session(create_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create Session");
    let session_id = created.session.id;

    client
        .set_session_icon(session_id, "md-bug")
        .await
        .expect("set the Session's Icon");

    let change = next_catalog_change_matching(&mut catalog, |change| {
        matches!(change, SessionCatalogChange::TitleChanged { session_id: id, .. } if *id == session_id)
    })
    .await;
    assert_eq!(
        change,
        SessionCatalogChange::TitleChanged {
            session_id,
            title: "Explain the seam".to_owned(),
            icon: Some("md-bug".to_owned()),
        }
    );

    drop(catalog);
    server.shutdown().await.expect("shut down server");
}
