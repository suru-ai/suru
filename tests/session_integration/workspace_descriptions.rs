//! A Workspace's Description: derived in the same Errand as its Icon, set or
//! cleared by the user over the protocol, and carried to every client on
//! every copy of the Workspace. Prior art: `workspace_icons.rs` covers the
//! Errand this extends, and `workspace_icon_choice.rs` the Icon's own choice
//! this stands beside.

use crate::server_support::PROGRESS_DEADLINE;
use crate::{
    failing_provider_support::spawn_with_failing_provider,
    provider_support::ControlledProvider,
    server_support::{
        next_derived_title, next_workspace_description_changed, next_workspace_icon_changed,
    },
    support::{hosted_model, hosted_selection},
};
use serde_json::json;
use suru::{
    managed_client::ManagedClient,
    protocol::{
        CreateSessionRequest, InitialPrompt, PromptId, ProviderId, SessionError, SessionErrorCode,
        SetWorkspaceDescriptionRequest, WorkspaceDescription, WorkspaceDescriptionChanged,
        WorkspaceId,
    },
    server::{self, ServerConfig},
};
use tokio::time::timeout;

const PROVIDER: &str = "controlled";
const MODEL: &str = "controlled-default";

/// A first Prompt that asks for Errands: the Session selects the controlled
/// Provider, which derives its Title and its Workspace's Icon and
/// Description.
fn errand_request(workspace: &std::path::Path, prompt: &str) -> CreateSessionRequest {
    CreateSessionRequest {
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

/// A first Prompt that asks for no Errand at all: with no Agent Selection,
/// the only Icon or Description the Workspace carries is whichever a test
/// sets for it.
fn quiet_request(workspace: &std::path::Path, prompt: &str) -> CreateSessionRequest {
    CreateSessionRequest {
        agent_selection: None,
        ..errand_request(workspace, prompt)
    }
}

fn controlled_provider() -> (
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
        suru::managed_client::ManagedClientConfig::new(state_dir, channel)
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    crate::support::receive_managed_client_initial_state(&mut client).await;
    client
}

/// Answers the Title Errand a fresh Session's creation spawns, identified by
/// its schema rather than by arrival order.
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

/// The Workspace Errand a fresh Session's creation spawns behind its Title
/// Errand, identified by its schema, which asks for no Title.
async fn next_workspace_errand(
    provider: &mut ControlledProvider,
) -> crate::provider_support::ErrandRequest {
    let errand = timeout(PROGRESS_DEADLINE, provider.next_errand())
        .await
        .expect("the Workspace Errand reaches the Provider");
    assert!(
        !errand.schema()["properties"]
            .as_object()
            .unwrap()
            .contains_key("title"),
        "the Workspace Errand's schema asks for no Title: {}",
        errand.schema()
    );
    errand
}

/// The Description a listing's Workspace carries for `workspace_id`, read off
/// any Session the listing holds rooted there.
async fn listed_description(
    client: &ManagedClient,
    workspace_id: &WorkspaceId,
) -> Option<WorkspaceDescription> {
    client
        .list_sessions(None)
        .await
        .expect("list Sessions")
        .into_iter()
        .find_map(|item| {
            let workspace = item.workspace()?;
            (&workspace.id == workspace_id).then(|| workspace.description.clone())?
        })
}

/// The Icon a listing's Workspace carries for `workspace_id`.
async fn listed_icon(client: &ManagedClient, workspace_id: &WorkspaceId) -> Option<String> {
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

/// Every Description change the catalog announces until a Workspace's Icon
/// lands, in order — which, as a Workspace Errand commits its Description
/// ahead of its Icon, is every one its derivation made.
async fn description_changes_until_the_icon_lands(
    client: &mut ManagedClient,
) -> Vec<WorkspaceDescriptionChanged> {
    timeout(PROGRESS_DEADLINE, async {
        let mut changes = Vec::new();
        loop {
            match client.next().await {
                Some(suru::managed_client::ManagedEvent::WorkspaceIconChanged(_)) => {
                    return changes;
                }
                Some(suru::managed_client::ManagedEvent::WorkspaceDescriptionChanged(changed)) => {
                    changes.push(changed);
                }
                _ => {}
            }
        }
    })
    .await
    .expect("the Workspace's Icon lands")
}

fn derived(text: &str) -> Option<WorkspaceDescription> {
    Some(WorkspaceDescription {
        text: text.to_owned(),
        set: false,
    })
}

fn set(text: &str) -> Option<WorkspaceDescription> {
    Some(WorkspaceDescription {
        text: text.to_owned(),
        set: true,
    })
}

async fn set_description_response(
    descriptor: &suru::protocol::RuntimeDescriptor,
    workspace_id: &WorkspaceId,
    path: Option<&std::path::Path>,
    description: &str,
) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/workspaces/description", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&SetWorkspaceDescriptionRequest {
            workspace_id: workspace_id.clone(),
            path: path.map(std::path::Path::to_path_buf),
            description: description.to_owned(),
        })
        .send()
        .await
        .expect("send Workspace Description command")
}

#[tokio::test]
async fn the_workspace_errand_asks_for_a_description_and_lands_it_beside_the_icon() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = controlled_provider();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "workspace-description-derived")
            .expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let mut client = connected_client(state_dir.path(), "workspace-description-derived").await;

    let created = client
        .create_session(errand_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create Session");
    let workspace_id = created.session.workspace.id.clone();
    assert_eq!(created.session.workspace.description, None);

    answer_title_errand(&mut provider, "Explain the Provider seam").await;
    next_derived_title(&mut client).await;
    let errand = next_workspace_errand(&mut provider).await;
    assert!(
        errand.schema()["properties"]["description"].is_object(),
        "the Workspace Errand's schema asks for a Description: {}",
        errand.schema()
    );
    assert!(
        errand.schema()["required"]
            .as_array()
            .is_some_and(|required| required.contains(&json!("description"))),
        "the Workspace Errand requires a Description: {}",
        errand.schema()
    );
    errand.succeed(json!({
        "icon": "dev-rust",
        "description": "Where the Provider seam is explained.",
    }));

    assert_eq!(
        next_workspace_description_changed(&mut client).await,
        WorkspaceDescriptionChanged {
            workspace_id: workspace_id.clone(),
            description: derived("Where the Provider seam is explained."),
        }
    );
    assert_eq!(
        listed_description(&client, &workspace_id).await,
        derived("Where the Provider seam is explained."),
        "the listed Session's Workspace carries the derived Description"
    );
    assert_eq!(
        listed_icon(&client, &workspace_id).await,
        Some("dev-rust".to_owned()),
        "the Icon lands from the same reply"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_workspace_that_already_carries_an_icon_still_gains_a_description() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = controlled_provider();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "workspace-description-after-icon")
            .expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let mut client = connected_client(state_dir.path(), "workspace-description-after-icon").await;

    let created = client
        .create_session(errand_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create the first Session");
    let workspace_id = created.session.workspace.id.clone();
    answer_title_errand(&mut provider, "Explain the Provider seam").await;
    next_derived_title(&mut client).await;
    // The first answer names an Icon but describes nothing, so the Icon lands
    // alone and the Description stays absent.
    next_workspace_errand(&mut provider)
        .await
        .succeed(json!({ "icon": "dev-rust", "description": "  " }));
    next_workspace_icon_changed(&mut client).await;

    client
        .create_session(errand_request(workspace.path(), "Ship the picker"))
        .await
        .expect("create the second Session");
    answer_title_errand(&mut provider, "Ship the Provider picker").await;
    next_derived_title(&mut client).await;
    next_workspace_errand(&mut provider).await.succeed(json!({
        "icon": "md-bug",
        "description": "Where the Provider picker ships from.",
    }));

    assert_eq!(
        next_workspace_description_changed(&mut client).await,
        WorkspaceDescriptionChanged {
            workspace_id: workspace_id.clone(),
            description: derived("Where the Provider picker ships from."),
        },
        "a Workspace with an Icon is still asked for its absent Description"
    );
    assert_eq!(
        listed_icon(&client, &workspace_id).await,
        Some("dev-rust".to_owned()),
        "a derived Icon never replaces the Icon the Workspace already carries"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_set_description_stands_against_a_later_derivation() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = controlled_provider();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "workspace-description-precedence")
            .expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let mut client = connected_client(state_dir.path(), "workspace-description-precedence").await;

    let created = client
        .create_session(errand_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create Session");
    let workspace_id = created.session.workspace.id.clone();
    answer_title_errand(&mut provider, "Explain the Provider seam").await;
    next_derived_title(&mut client).await;
    let errand = next_workspace_errand(&mut provider).await;

    client
        .set_workspace_description(&workspace_id, None, "Where the release notes are drafted.")
        .await
        .expect("set the Workspace's Description while its Errand is outstanding");
    assert_eq!(
        next_workspace_description_changed(&mut client).await,
        WorkspaceDescriptionChanged {
            workspace_id: workspace_id.clone(),
            description: set("Where the release notes are drafted."),
        },
        "the set Description reaches the catalog before the Errand answers"
    );

    errand.succeed(json!({
        "icon": "dev-rust",
        "description": "Something the Errand made up.",
    }));
    // The Errand's Description is attempted before its Icon, and the Icon was
    // still absent, so the Icon landing says the derivation has finished with
    // the Description — which it announced nothing about.
    assert_eq!(
        description_changes_until_the_icon_lands(&mut client).await,
        Vec::new(),
        "the completed derivation announced no Description over the set one"
    );

    assert_eq!(
        listed_description(&client, &workspace_id).await,
        set("Where the release notes are drafted."),
        "the listing shows the set Description rather than the later derivation"
    );
    let reopened = client
        .read_session(created.session.id)
        .await
        .expect("reopen the Session");
    assert_eq!(
        reopened.session.workspace.description,
        set("Where the release notes are drafted."),
        "the open Session's Workspace shows the set Description too"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");

    // The Icon landed in the row the set Description had already made, and
    // both are read back from it after a restart.
    let restarted = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "workspace-description-precedence")
            .expect("reconfigure server"),
    )
    .await
    .expect("respawn server");
    let client = connected_client(state_dir.path(), "workspace-description-precedence").await;
    assert_eq!(
        listed_description(&client, &workspace_id).await,
        set("Where the release notes are drafted.")
    );
    assert_eq!(
        listed_icon(&client, &workspace_id).await,
        Some("dev-rust".to_owned())
    );

    restarted.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn clearing_a_set_description_lets_the_next_session_derive_one() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = controlled_provider();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "workspace-description-clear").expect("configure"),
        runtime,
    )
    .await
    .expect("spawn server");
    let mut client = connected_client(state_dir.path(), "workspace-description-clear").await;

    // A chosen Icon and a set Description leave the Workspace nothing to
    // derive, so only clearing the Description can bring its Errand back.
    let created = client
        .create_session(quiet_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create the first Session");
    let workspace_id = created.session.workspace.id.clone();
    client
        .set_workspace_icon(&workspace_id, "md-bug")
        .await
        .expect("choose the Workspace's Icon");
    client
        .set_workspace_description(&workspace_id, None, "Kept by hand.")
        .await
        .expect("set the Workspace's Description");
    next_workspace_description_changed(&mut client).await;

    client
        .create_session(errand_request(workspace.path(), "Ship the picker"))
        .await
        .expect("create the second Session");
    answer_title_errand(&mut provider, "Ship the Provider picker").await;
    next_derived_title(&mut client).await;
    assert!(
        provider.try_next_errand().is_none(),
        "a Workspace with a chosen Icon and a set Description asks for no Workspace Errand"
    );

    client
        .set_workspace_description(&workspace_id, None, "   ")
        .await
        .expect("clear the Workspace's Description");
    assert_eq!(
        next_workspace_description_changed(&mut client).await,
        WorkspaceDescriptionChanged {
            workspace_id: workspace_id.clone(),
            description: None,
        },
        "blank text clears the Description"
    );
    assert_eq!(listed_description(&client, &workspace_id).await, None);

    client
        .create_session(errand_request(workspace.path(), "Polish the picker"))
        .await
        .expect("create the third Session");
    answer_title_errand(&mut provider, "Polish the Provider picker").await;
    next_derived_title(&mut client).await;
    next_workspace_errand(&mut provider).await.succeed(json!({
        "icon": "dev-rust",
        "description": "Where the Provider picker is polished.",
    }));

    assert_eq!(
        next_workspace_description_changed(&mut client)
            .await
            .description,
        derived("Where the Provider picker is polished."),
        "a cleared Description is derived again by the next Session created there"
    );
    assert_eq!(
        listed_icon(&client, &workspace_id).await,
        Some("md-bug".to_owned()),
        "the chosen Icon stands against the derivation that filled the Description"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_set_description_reaches_every_client_on_one_line_and_outlives_a_restart() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "workspace-description-set").expect("configure"),
    )
    .await
    .expect("spawn server");
    let mut setter = connected_client(state_dir.path(), "workspace-description-set").await;
    let mut onlooker = connected_client(state_dir.path(), "workspace-description-set").await;

    let created = setter
        .create_session(quiet_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create Session");
    let workspace_id = created.session.workspace.id.clone();

    setter
        .set_workspace_description(
            &workspace_id,
            None,
            "  Where the release\nnotes are   drafted.\n",
        )
        .await
        .expect("set the Workspace's Description");

    let expected = WorkspaceDescriptionChanged {
        workspace_id: workspace_id.clone(),
        description: set("Where the release notes are drafted."),
    };
    assert_eq!(
        next_workspace_description_changed(&mut setter).await,
        expected,
        "the Description is kept on one line, its whitespace collapsed"
    );
    assert_eq!(
        next_workspace_description_changed(&mut onlooker).await,
        expected,
        "every client hears of the Description, not only the one that set it"
    );
    assert_eq!(
        onlooker
            .read_session(created.session.id)
            .await
            .expect("reopen the Session")
            .session
            .workspace
            .description,
        set("Where the release notes are drafted.")
    );

    drop(setter);
    drop(onlooker);
    server.shutdown().await.expect("shut down server");

    let restarted = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "workspace-description-set").expect("reconfigure"),
    )
    .await
    .expect("respawn server");
    let client = connected_client(state_dir.path(), "workspace-description-set").await;
    assert_eq!(
        listed_description(&client, &workspace_id).await,
        set("Where the release notes are drafted."),
        "a set Description, and that it was set, survive a restart"
    );

    restarted.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_derived_description_outlives_a_restart_and_is_not_derived_again() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = controlled_provider();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "workspace-description-restart").expect("configure"),
        runtime,
    )
    .await
    .expect("spawn server");
    let mut client = connected_client(state_dir.path(), "workspace-description-restart").await;

    let created = client
        .create_session(errand_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create Session");
    answer_title_errand(&mut provider, "Explain the Provider seam").await;
    next_derived_title(&mut client).await;
    next_workspace_errand(&mut provider).await.succeed(json!({
        "icon": "dev-rust",
        "description": "Where the Provider seam is explained.",
    }));
    next_workspace_description_changed(&mut client).await;

    drop(client);
    server.shutdown().await.expect("shut down server");

    let (runtime, mut provider) = controlled_provider();
    let restarted = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "workspace-description-restart").expect("reconfigure"),
        runtime,
    )
    .await
    .expect("respawn server");
    let mut client = connected_client(state_dir.path(), "workspace-description-restart").await;

    let reopened = client
        .read_session(created.session.id)
        .await
        .expect("reopen the Session");
    assert_eq!(
        reopened.session.workspace.description,
        derived("Where the Provider seam is explained."),
        "the derived Description, and that it was derived, survive a restart"
    );

    client
        .create_session(errand_request(workspace.path(), "Ship the picker"))
        .await
        .expect("create a Session after the restart");
    answer_title_errand(&mut provider, "Ship the Provider picker").await;
    next_derived_title(&mut client).await;
    assert!(
        provider.try_next_errand().is_none(),
        "a Workspace that carries an Icon and a Description asks for neither again"
    );

    restarted.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn setting_a_description_for_an_unknown_workspace_is_refused() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "workspace-description-unknown").expect("configure"),
    )
    .await
    .expect("spawn server");

    let unknown = WorkspaceId("not-a-workspace-this-server-knows".to_owned());
    let response = set_description_response(server.descriptor(), &unknown, None, "Anything").await;
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
async fn an_overlong_description_is_refused_and_changes_nothing() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "workspace-description-overlong").expect("configure"),
    )
    .await
    .expect("spawn server");
    let client = connected_client(state_dir.path(), "workspace-description-overlong").await;

    let created = client
        .create_session(quiet_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create Session");
    let workspace_id = created.session.workspace.id.clone();

    let response = set_description_response(
        server.descriptor(),
        &workspace_id,
        None,
        &"word ".repeat(200),
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    let refusal = response
        .json::<SessionError>()
        .await
        .expect("decode the rejection");
    assert_eq!(refusal.code, SessionErrorCode::InvalidDescription);
    assert!(
        refusal.message.contains("at most 300 characters"),
        "the refusal says what a Description may run to: {}",
        refusal.message
    );
    assert_eq!(
        listed_description(&client, &workspace_id).await,
        None,
        "a refused Description changes nothing"
    );

    server.shutdown().await.expect("shut down server");
}

/// The Sidekick Workspace is a plain directory Workspace Suru owns, and
/// nothing about Descriptions sets it apart: its first Session's Errand asks
/// for its Icon and Description from its name alone — it holds no README,
/// standing outside any Repository — and a set Description stands there as
/// it does anywhere else.
#[tokio::test]
async fn the_sidekick_workspace_gains_a_description_like_any_other() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let (runtime, mut provider) = controlled_provider();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "workspace-description-sidekick").expect("configure"),
        runtime,
    )
    .await
    .expect("spawn server");
    let mut client = connected_client(state_dir.path(), "workspace-description-sidekick").await;

    let sidekick = client
        .sidekick_workspace()
        .await
        .expect("ask for the Sidekick Workspace");
    let directory = sidekick
        .execution_directory
        .expect("the Sidekick Workspace is somewhere a Session can work")
        .path;
    let created = client
        .create_session(errand_request(&directory, "What is everything doing?"))
        .await
        .expect("begin a Sidekick's Session");
    let workspace_id = created.session.workspace.id.clone();
    assert_eq!(workspace_id, sidekick.workspace.id);

    answer_title_errand(&mut provider, "Check on everything").await;
    next_derived_title(&mut client).await;
    let errand = next_workspace_errand(&mut provider).await;
    assert!(
        errand.schema()["properties"]["description"].is_object(),
        "the Sidekick Workspace is asked for a Description too: {}",
        errand.schema()
    );
    assert!(
        errand.prompt().contains("\"sidekick\"") && !errand.prompt().contains("README"),
        "it is described from its name alone: {:?}",
        errand.prompt()
    );
    errand.succeed(json!({
        "icon": "md-bug",
        "description": "Where a Sidekick works across Suru.",
    }));
    assert_eq!(
        next_workspace_description_changed(&mut client).await,
        WorkspaceDescriptionChanged {
            workspace_id: workspace_id.clone(),
            description: derived("Where a Sidekick works across Suru."),
        }
    );

    client
        .set_workspace_description(&workspace_id, None, "My assistant for all of Suru.")
        .await
        .expect("set the Sidekick Workspace's Description");
    assert_eq!(
        next_workspace_description_changed(&mut client)
            .await
            .description,
        set("My assistant for all of Suru.")
    );

    server.shutdown().await.expect("shut down server");
}

/// The limit is measured on the Description as it is kept — on one line, its
/// whitespace collapsed — so text that only looks long for its line breaks
/// and runs of spaces fits, and text that is long once collapsed does not.
#[tokio::test]
async fn a_description_is_measured_once_it_is_kept_on_one_line() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "workspace-description-measured").expect("configure"),
    )
    .await
    .expect("spawn server");
    let mut client = connected_client(state_dir.path(), "workspace-description-measured").await;
    let created = client
        .create_session(quiet_request(workspace.path(), "Explain the seam"))
        .await
        .expect("create Session");
    let workspace_id = created.session.workspace.id.clone();

    // 60 words of four letters and the spaces between them: 299 characters
    // kept on one line, though nearly twice that as written.
    let kept = vec!["word"; 60].join(" ");
    assert_eq!(kept.chars().count(), 299);
    let written = vec!["word"; 60].join(" \n   ");
    assert!(written.chars().count() > 300);
    client
        .set_workspace_description(&workspace_id, None, &written)
        .await
        .expect("a Description that fits once kept on one line is kept");
    assert_eq!(
        next_workspace_description_changed(&mut client)
            .await
            .description,
        set(&kept)
    );

    let response = set_description_response(
        server.descriptor(),
        &workspace_id,
        None,
        &format!("{kept} more"),
    )
    .await;
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    let refusal = response
        .json::<SessionError>()
        .await
        .expect("decode the rejection");
    assert_eq!(refusal.code, SessionErrorCode::InvalidDescription);
    assert!(
        refusal.message.contains("at most 300 characters") && refusal.message.contains("304"),
        "the refusal says the limit and how far past it this one runs: {}",
        refusal.message
    );

    server.shutdown().await.expect("shut down server");
}

/// A fresh Landing offers its Workspace in the Workspace Picker before any
/// Session has been begun there, so the Server accepts a Description for a
/// Workspace it knows only by resolving where it is presented — and that
/// Description stands when the first Session is begun and its Errand runs.
#[tokio::test]
async fn a_description_set_before_the_first_session_stands_once_one_is_begun() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let directory = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = controlled_provider();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "workspace-description-before").expect("configure"),
        runtime,
    )
    .await
    .expect("spawn server");
    let mut client = connected_client(state_dir.path(), "workspace-description-before").await;
    let landing = client
        .outlook(suru::protocol::Outlook::Local)
        .resolve_workspace(suru::protocol::ResolveWorkspaceRequest {
            checkout_id: None,
            remembered_execution_directory: None,
            workspace_id: None,
            base: None,
            path: directory.path().to_owned(),
        })
        .await
        .expect("resolve the Landing's Workspace")
        .workspace;

    client
        .set_workspace_description(&landing.id, Some(&landing.path), "Kept before any Session.")
        .await
        .expect("describe a Workspace with no Session yet");
    assert_eq!(
        next_workspace_description_changed(&mut client).await,
        WorkspaceDescriptionChanged {
            workspace_id: landing.id.clone(),
            description: set("Kept before any Session."),
        }
    );

    let created = client
        .create_session(errand_request(directory.path(), "Explain the seam"))
        .await
        .expect("begin the first Session there");
    assert_eq!(created.session.workspace.id, landing.id);
    assert_eq!(
        created.session.workspace.description,
        set("Kept before any Session."),
        "the first Session's Workspace carries the Description set before it"
    );
    answer_title_errand(&mut provider, "Explain the Provider seam").await;
    next_derived_title(&mut client).await;
    next_workspace_errand(&mut provider).await.succeed(json!({
        "icon": "dev-rust",
        "description": "Derived instead.",
    }));
    assert_eq!(
        description_changes_until_the_icon_lands(&mut client).await,
        Vec::new(),
        "the first Session's derivation announced no Description over the set one"
    );
    assert_eq!(
        listed_description(&client, &landing.id).await,
        set("Kept before any Session."),
        "the derivation the first Session ran left the set Description standing"
    );

    server.shutdown().await.expect("shut down server");
}

/// The path a Description names its Workspace by must resolve to that very
/// Workspace on this Server; one that resolves to another is refused, and
/// describes nothing.
#[tokio::test]
async fn a_description_naming_its_workspace_by_another_s_path_is_refused() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let named = tempfile::tempdir().expect("create the named Workspace");
    let elsewhere = tempfile::tempdir().expect("create another Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "workspace-description-mismatch").expect("configure"),
    )
    .await
    .expect("spawn server");

    let named_id = WorkspaceId::directory(
        &suru::paths::canonical(named.path()).expect("canonicalize the named Workspace"),
    );
    let response = set_description_response(
        server.descriptor(),
        &named_id,
        Some(elsewhere.path()),
        "Anything",
    )
    .await;
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
