//! A user's own choice of a Workspace's Icon from the Icon Catalog, set over
//! the protocol and reused by every client the same `WorkspaceIconChanged`
//! change a derivation publishes. Prior art: `icons.rs` covers the Session
//! Icon's own choice this mirrors; `workspace_icons.rs` covers the Workspace
//! Icon's own derivation this stands beside.

use crate::server_support::PROGRESS_DEADLINE;
use crate::{
    failing_provider_support::spawn_with_failing_provider,
    provider_support::ControlledProvider,
    server_support::{next_workspace_description_changed, next_workspace_icon_changed},
    support::{hosted_model, hosted_selection},
};
use serde_json::json;
use suru::{
    protocol::{
        CreateSessionRequest, InitialPrompt, PromptId, ProviderId, SessionError, SessionErrorCode,
        SetWorkspaceIconRequest, WorkspaceId,
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
        // No Agent Selection, so no Errand runs and the only Icon the
        // Workspace carries is whichever this test chooses for it.
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

fn workspace_icon_provider() -> (
    std::sync::Arc<crate::provider_support::ControlledProviderRuntime>,
    ControlledProvider,
) {
    ControlledProvider::with_provider(
        ProviderId::new(PROVIDER),
        vec![hosted_model(PROVIDER, MODEL)],
    )
}

async fn connected_client(
    state_dir: &std::path::Path,
    channel: &str,
) -> suru::managed_client::ManagedClient {
    let mut client = suru::managed_client::ManagedClient::connect(
        suru::managed_client::ManagedClientConfig::new(state_dir, channel)
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    crate::support::receive_managed_client_initial_state(&mut client).await;
    client
}

/// The Icon Catalog name a listing's Workspace carries for `workspace_id`, or
/// `None` where the listing carries no Icon for it — read off any Session the
/// listing still holds rooted there.
async fn listed_workspace_icon(
    client: &suru::managed_client::ManagedClient,
    workspace_id: &WorkspaceId,
) -> Option<String> {
    client
        .list_sessions(None)
        .await
        .expect("list Sessions")
        .into_iter()
        .find_map(|item| {
            let workspace = item.workspace()?;
            (&workspace.id == workspace_id).then(|| workspace.icon.clone())?
        })
}

/// Answers the Title Errand a fresh Session's creation spawns, identifying it
/// by its schema's properties rather than by arrival order.
async fn answer_title_errand(provider: &mut ControlledProvider, title: &str) {
    let errand = timeout(PROGRESS_DEADLINE, provider.next_errand())
        .await
        .expect("the Title Errand reaches the Provider");
    assert!(
        errand.schema()["properties"]["title"].is_object(),
        "the Title Errand's schema asks for a title: {}",
        errand.schema()
    );
    errand.succeed(json!({ "title": title, "icon": "md-bug" }));
}

/// Answers the Workspace Icon Errand a fresh Session's creation spawns when
/// its Workspace has none, identified the same way: by its schema, which asks
/// for an Icon alone.
async fn next_workspace_errand(
    provider: &mut ControlledProvider,
) -> crate::provider_support::ErrandRequest {
    let errand = timeout(PROGRESS_DEADLINE, provider.next_errand())
        .await
        .expect("the Workspace Icon Errand reaches the Provider");
    assert!(
        !errand.schema()["properties"]
            .as_object()
            .unwrap()
            .contains_key("title"),
        "the Workspace Icon Errand's schema asks for an Icon alone: {}",
        errand.schema()
    );
    errand
}

async fn set_workspace_icon_response(
    descriptor: &suru::protocol::RuntimeDescriptor,
    workspace_id: &WorkspaceId,
    icon: &str,
) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/workspaces/icon", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&SetWorkspaceIconRequest {
            workspace_id: workspace_id.clone(),
            icon: icon.to_owned(),
        })
        .send()
        .await
        .expect("send Workspace Icon command")
}

#[tokio::test]
async fn setting_a_workspace_icon_updates_the_catalog_and_the_listing() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "workspace-icon-choice-set").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let mut client = connected_client(state_dir.path(), "workspace-icon-choice-set").await;

    let created = client
        .create_session(create_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create Session");
    let workspace_id = created.session.workspace.id.clone();
    assert_eq!(created.session.workspace.icon, None);

    client
        .set_workspace_icon(&workspace_id, "md-bug")
        .await
        .expect("set the Workspace's Icon");

    assert_eq!(
        next_workspace_icon_changed(&mut client).await,
        suru::protocol::WorkspaceIconChanged {
            workspace_id: workspace_id.clone(),
            icon: Some("md-bug".to_owned()),
        }
    );
    assert_eq!(
        listed_workspace_icon(&client, &workspace_id).await,
        Some("md-bug".to_owned())
    );
    let reopened = client
        .read_session(created.session.id)
        .await
        .expect("reopen the Session");
    assert_eq!(reopened.session.workspace.icon.as_deref(), Some("md-bug"));

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn choosing_an_unknown_icon_is_refused_and_changes_nothing() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "workspace-icon-choice-refusal")
            .expect("configure server"),
    )
    .await
    .expect("spawn server");
    let client = connected_client(state_dir.path(), "workspace-icon-choice-refusal").await;

    let created = client
        .create_session(create_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create Session");
    let workspace_id = created.session.workspace.id.clone();

    let response =
        set_workspace_icon_response(server.descriptor(), &workspace_id, "md-not-a-glyph").await;
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(
        response
            .json::<SessionError>()
            .await
            .expect("decode the rejection")
            .code,
        SessionErrorCode::InvalidIcon
    );

    let reopened = client
        .read_session(created.session.id)
        .await
        .expect("reopen the Session");
    assert_eq!(
        reopened.session.workspace.icon, None,
        "a refused Icon changes nothing"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn choosing_an_icon_for_an_unknown_workspace_is_refused() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "workspace-icon-choice-unknown-workspace")
            .expect("configure server"),
    )
    .await
    .expect("spawn server");

    let unknown = WorkspaceId("not-a-workspace-this-server-knows".to_owned());
    let response = set_workspace_icon_response(server.descriptor(), &unknown, "md-bug").await;
    assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        response
            .json::<SessionError>()
            .await
            .expect("decode the rejection")
            .code,
        SessionErrorCode::InvalidWorkspace
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_chosen_workspace_icon_stands_against_a_later_derivation() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = workspace_icon_provider();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "workspace-icon-choice-precedence")
            .expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let mut client = connected_client(state_dir.path(), "workspace-icon-choice-precedence").await;

    let created = client
        .create_session(errand_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create Session");
    let workspace_id = created.session.workspace.id.clone();

    // The Title Errand is spawned but has not yet answered, so the Workspace
    // Errand behind it has not even been built — the user's choice below
    // lands well before either one could.
    answer_title_errand(&mut provider, "Explain the Provider seam").await;
    let workspace_errand = next_workspace_errand(&mut provider).await;

    client
        .set_workspace_icon(&workspace_id, "md-bug")
        .await
        .expect("set the Workspace's Icon while its Errand is outstanding");
    assert_eq!(
        next_workspace_icon_changed(&mut client).await,
        suru::protocol::WorkspaceIconChanged {
            workspace_id: workspace_id.clone(),
            icon: Some("md-bug".to_owned()),
        },
        "the user's choice reaches the catalog before the Errand answers"
    );

    // The Errand now answers with a different Icon. It is filling an absence
    // that is no longer there, so it is discarded rather than published.
    workspace_errand.succeed(json!({ "icon": "dev-rust" }));

    assert_eq!(
        listed_workspace_icon(&client, &workspace_id).await,
        Some("md-bug".to_owned()),
        "the listing shows the chosen Icon rather than the later derivation"
    );
    let reopened = client
        .read_session(created.session.id)
        .await
        .expect("reopen the Session");
    assert_eq!(
        reopened.session.workspace.icon.as_deref(),
        Some("md-bug"),
        "the open Session shows the chosen Icon rather than the later derivation"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_chosen_workspace_icon_stands_while_the_errand_fills_the_absent_description() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = workspace_icon_provider();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "workspace-icon-choice-then-description")
            .expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let mut client =
        connected_client(state_dir.path(), "workspace-icon-choice-then-description").await;

    // Create the first Session without an Agent Selection, so no Errand of
    // any kind runs, and choose its Workspace's Icon by hand.
    let created = client
        .create_session(create_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create the first Session");
    let workspace_id = created.session.workspace.id.clone();
    client
        .set_workspace_icon(&workspace_id, "md-bug")
        .await
        .expect("set the Workspace's Icon");
    next_workspace_icon_changed(&mut client).await;

    // A new Session in the same Workspace, this time with an Agent
    // Selection: the Workspace still has no Description, so its Errand is
    // asked, and only the Description it answers with lands.
    client
        .create_session(errand_request(workspace.path(), "Ship the picker"))
        .await
        .expect("create the second Session");
    answer_title_errand(&mut provider, "Ship the Provider picker").await;
    next_workspace_errand(&mut provider).await.succeed(json!({
        "icon": "dev-rust",
        "description": "Where the Provider picker ships from.",
    }));
    next_workspace_description_changed(&mut client).await;

    assert_eq!(
        listed_workspace_icon(&client, &workspace_id).await,
        Some("md-bug".to_owned()),
        "a derived Icon never replaces a chosen one"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_chosen_workspace_icon_outlives_a_restart() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "workspace-icon-choice-restart")
            .expect("configure server"),
    )
    .await
    .expect("spawn server");
    let client = connected_client(state_dir.path(), "workspace-icon-choice-restart").await;

    let created = client
        .create_session(create_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create Session");
    let workspace_id = created.session.workspace.id.clone();
    client
        .set_workspace_icon(&workspace_id, "md-bug")
        .await
        .expect("set the Workspace's Icon");

    drop(client);
    server.shutdown().await.expect("shut down server");

    let restarted = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "workspace-icon-choice-restart")
            .expect("reconfigure server"),
    )
    .await
    .expect("respawn server");
    let client = connected_client(state_dir.path(), "workspace-icon-choice-restart").await;

    assert_eq!(
        listed_workspace_icon(&client, &workspace_id).await,
        Some("md-bug".to_owned()),
        "a chosen Workspace Icon survives a restart"
    );
    let reopened = client
        .read_session(created.session.id)
        .await
        .expect("reopen the Session");
    assert_eq!(reopened.session.workspace.icon.as_deref(), Some("md-bug"));

    restarted.shutdown().await.expect("shut down server");
}
