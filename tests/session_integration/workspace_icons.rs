//! Deriving a Workspace's Icon from its presented name and README, through
//! the second Errand a Session's creation spawns beside its Title Errand.

use crate::server_support::PROGRESS_DEADLINE;
use crate::{
    provider_support::ControlledProvider,
    repositories::git,
    server_support::{
        config_root_pinning, next_derived_title, next_workspace_description_changed,
        next_workspace_icon_changed,
    },
    support::{hosted_model, hosted_selection},
};
use serde_json::json;
use suru::{
    protocol::{
        AgentId, AgentIdentity, CreateSessionRequest, DerivationErrand, InitialPrompt, PromptId,
        ProviderId,
    },
    provider::ProviderEvent,
    server::{self, ServerConfig},
};
use tokio::time::timeout;

const PROVIDER: &str = "controlled";
const MODEL: &str = "controlled-default";

fn create_request(workspace: &std::path::Path, prompt: &str) -> CreateSessionRequest {
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

/// Runs a Session's first Turn through to completion, which is every chance a
/// derivation would have had to reach the Provider. What follows can then say
/// no Errand was asked for without waiting out a deadline.
async fn work_the_first_turn(provider: &mut ControlledProvider) {
    let mut session = timeout(PROGRESS_DEADLINE, provider.next_start())
        .await
        .expect("the first Turn starts a Provider Session")
        .succeed(AgentIdentity {
            agent: AgentId::new("controlled-agent"),
            selection: hosted_selection(PROVIDER, MODEL),
        });
    timeout(PROGRESS_DEADLINE, session.next_turn())
        .await
        .expect("the first Turn reaches the Provider")
        .succeed();
    session.emit(ProviderEvent::TurnCompleted);
}

/// Answers the Title Errand a fresh Session's creation spawns, identifying it
/// by its schema's properties rather than by arrival order — the same
/// technique a harness distinguishing the two Errands must use, since both
/// run on the Provider's own queue.
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

#[tokio::test]
async fn derivation_on_the_first_session_lands_the_catalog_change_and_the_listing() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = workspace_icon_provider();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "workspace-icon-first-session")
            .expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let mut client = connected_client(state_dir.path(), "workspace-icon-first-session").await;

    let created = client
        .create_session(create_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create Session");
    let workspace_id = created.session.workspace.id.clone();

    answer_title_errand(&mut provider, "Explain the Provider seam").await;
    // Drained past the Title Errand above, which lands first on this task.
    next_derived_title(&mut client).await;
    let workspace_errand = next_workspace_errand(&mut provider).await;
    workspace_errand.succeed(json!({ "icon": "dev-rust" }));

    assert_eq!(
        next_workspace_icon_changed(&mut client).await,
        suru::protocol::WorkspaceIconChanged {
            workspace_id: workspace_id.clone(),
            icon: Some("dev-rust".to_owned()),
        }
    );
    let listed = client
        .list_sessions(None)
        .await
        .expect("list Sessions")
        .into_iter()
        .find(|item| item.id() == created.session.id)
        .expect("the Session remains listed");
    assert_eq!(
        listed
            .workspace()
            .and_then(|workspace| workspace.icon.as_deref()),
        Some("dev-rust"),
        "the listed Session's Workspace carries the derived Icon"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn no_errand_is_asked_once_the_workspace_carries_an_icon_and_a_description() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = workspace_icon_provider();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "workspace-icon-second-session")
            .expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let mut client = connected_client(state_dir.path(), "workspace-icon-second-session").await;

    client
        .create_session(create_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create the first Session");
    answer_title_errand(&mut provider, "Explain the Provider seam").await;
    next_derived_title(&mut client).await;
    next_workspace_errand(&mut provider).await.succeed(json!({
        "icon": "dev-rust",
        "description": "Where the Provider seam is explained.",
    }));
    // Waiting for the catalog to report both commits — the Description's
    // first, as the Errand's reply is committed — is what proves the second
    // Session below is created only after the Workspace's Icon and
    // Description have actually landed, rather than racing the Errand's own
    // reply.
    next_workspace_description_changed(&mut client).await;
    next_workspace_icon_changed(&mut client).await;

    client
        .create_session(create_request(workspace.path(), "Ship the picker"))
        .await
        .expect("create the second Session");
    answer_title_errand(&mut provider, "Ship the Provider picker").await;
    next_derived_title(&mut client).await;

    assert!(
        provider.try_next_errand().is_none(),
        "a Workspace that already carries an Icon and a Description asks no Session created in it for another"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_failed_workspace_errand_is_retried_by_the_next_session_created_there() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = workspace_icon_provider();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "workspace-icon-retry").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let mut client = connected_client(state_dir.path(), "workspace-icon-retry").await;

    client
        .create_session(create_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create the first Session");
    answer_title_errand(&mut provider, "Explain the Provider seam").await;
    next_derived_title(&mut client).await;
    next_workspace_errand(&mut provider)
        .await
        .fail("the Provider is signed out");

    client
        .create_session(create_request(workspace.path(), "Ship the picker"))
        .await
        .expect("create the second Session");
    answer_title_errand(&mut provider, "Ship the Provider picker").await;
    next_derived_title(&mut client).await;
    let retried = next_workspace_errand(&mut provider).await;
    retried.succeed(json!({ "icon": "dev-rust" }));

    assert_eq!(
        next_workspace_icon_changed(&mut client).await.icon,
        Some("dev-rust".to_owned()),
        "a failed Errand records nothing, so the next Session created there tries again"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn the_prompt_carries_the_readme_of_a_workspace_with_a_known_main_root() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    std::fs::create_dir_all(workspace.path()).unwrap();
    git(workspace.path(), &["init", "-b", "main"]);
    std::fs::write(
        workspace.path().join("README.md"),
        "Suru is an interactive workspace for collaborating with an agent.",
    )
    .unwrap();
    git(workspace.path(), &["add", "README.md"]);
    git(
        workspace.path(),
        &["-c", "commit.gpgsign=false", "commit", "-m", "add README"],
    );
    let (runtime, mut provider) = workspace_icon_provider();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "workspace-icon-readme").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let client = connected_client(state_dir.path(), "workspace-icon-readme").await;

    client
        .create_session(create_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create Session");
    answer_title_errand(&mut provider, "Explain the Provider seam").await;
    let errand = next_workspace_errand(&mut provider).await;
    assert!(
        errand
            .prompt()
            .contains("Suru is an interactive workspace for collaborating with an agent."),
        "the Workspace Errand carries the README's opening text: {:?}",
        errand.prompt()
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn the_prompt_carries_only_the_name_for_a_workspace_with_no_repository() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = workspace_icon_provider();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "workspace-icon-no-readme").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let client = connected_client(state_dir.path(), "workspace-icon-no-readme").await;

    client
        .create_session(create_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create Session");
    answer_title_errand(&mut provider, "Explain the Provider seam").await;
    let errand = next_workspace_errand(&mut provider).await;
    let name = workspace
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(
        errand.prompt().contains(&name),
        "the Workspace Errand carries the Workspace's presented name: {:?}",
        errand.prompt()
    );
    assert!(
        !errand.prompt().contains("README"),
        "a Workspace with no Repository carries no README: {:?}",
        errand.prompt()
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn derivation_turned_off_asks_for_neither_errand() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let config_dir = config_root_pinning(&DerivationErrand::Off);
    let (runtime, mut provider) = workspace_icon_provider();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "workspace-icon-off")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
        runtime,
    )
    .await
    .expect("spawn server");
    let client = connected_client(state_dir.path(), "workspace-icon-off").await;

    client
        .create_session(create_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create Session");

    work_the_first_turn(&mut provider).await;

    assert!(
        provider.try_next_errand().is_none(),
        "a user who turned derivation off is charged for no Provider call on Suru's behalf"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_derived_workspace_icon_outlives_a_restart() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = workspace_icon_provider();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "workspace-icon-restart").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let mut client = connected_client(state_dir.path(), "workspace-icon-restart").await;

    let created = client
        .create_session(create_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create Session");
    answer_title_errand(&mut provider, "Explain the Provider seam").await;
    next_derived_title(&mut client).await;
    next_workspace_errand(&mut provider)
        .await
        .succeed(json!({ "icon": "dev-rust" }));
    next_workspace_icon_changed(&mut client).await;

    drop(client);
    server.shutdown().await.expect("shut down server");

    let (runtime, mut provider) = workspace_icon_provider();
    let restarted = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "workspace-icon-restart").expect("reconfigure server"),
        runtime,
    )
    .await
    .expect("respawn server");
    let client = connected_client(state_dir.path(), "workspace-icon-restart").await;

    let reopened = client
        .read_session(created.session.id)
        .await
        .expect("reopen the Session");
    assert_eq!(
        reopened.session.workspace.icon.as_deref(),
        Some("dev-rust"),
        "the Workspace's derived Icon survives a restart"
    );

    assert!(
        provider.try_next_errand().is_none(),
        "restarting derives no Icon again for a Workspace that already has one"
    );

    restarted.shutdown().await.expect("shut down server");
}
