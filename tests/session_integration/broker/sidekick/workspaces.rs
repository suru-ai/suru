//! `list_workspaces` and `set_workspace_description`: a Sidekick learning
//! which Workspaces there are on its own Server — each by its identity, its
//! name, its presented root's path, its Description and its Icon — and
//! recording what it learned a Workspace is for as that Workspace's
//! Description.
//!
//! A Description a Sidekick sets is set through the very operation the
//! Workspace endpoint performs for the user, so it stands against every later
//! derivation as the user's does, reaches every Client in the same catalog
//! change, and outlives a restart; empty text clears it, so it may be derived
//! again. Any other caller neither lists either Tool nor may call it.
//!
//! Each test acts as the MCP client a Sidekick's harness is and asserts on
//! what the Tools answer it, and on what Clients of the Session API observe.

use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, ManagedEvent},
    protocol::{Workspace, WorkspaceDescription, WorkspaceDescriptionChanged},
};

use super::*;
use crate::{
    broker::sidekick_acts::{acted, refused},
    server_support::next_workspace_description_changed,
};

/// The Tools through which a Sidekick learns and describes Workspaces.
const WORKSPACE_TOOLS: [&str; 2] = ["list_workspaces", "set_workspace_description"];

/// `list_workspaces`' rows, in its order.
async fn list_workspaces(client: &mut McpClient) -> Vec<Value> {
    acted(client, "list_workspaces", json!({})).await["workspaces"]
        .as_array()
        .expect("a listing lists Workspaces")
        .clone()
}

/// The row `list_workspaces` gives `workspace`.
async fn listed_row(client: &mut McpClient, workspace: &Workspace) -> Value {
    list_workspaces(client)
        .await
        .into_iter()
        .find(|row| row["workspace_id"] == json!(workspace.id))
        .unwrap_or_else(|| panic!("{} is listed", workspace.path.display()))
}

/// The row `list_workspaces` gives `workspace` while it carries `description`
/// and the Icon named `icon`.
fn row(workspace: &Workspace, description: Value, icon: Value) -> Value {
    json!({
        "workspace_id": workspace.id,
        "name": workspace
            .path
            .file_name()
            .expect("a Workspace's directory has a name")
            .to_string_lossy(),
        "path": workspace.path,
        "description": description,
        "icon": icon,
    })
}

/// A Description as a row gives it: its text, and whether it was set rather
/// than derived.
fn description(text: &str, set: bool) -> Value {
    json!({ "text": text, "set": set })
}

/// A Session in `directory` that names no Agent Selection, so it asks for no
/// Errand and its Workspace carries only what a test gives it; answers with
/// that Workspace as the Session API gives it.
async fn quiet_session(descriptor: &RuntimeDescriptor, directory: &Path, title: &str) -> Workspace {
    let request = CreateSessionRequest {
        agent_selection: None,
        ..session_request(directory, default_selection(&claude_models()), title)
    };
    create_session(descriptor, &request).await.session.workspace
}

/// A Session on Codex in `directory`, whose creation asks Codex for its Title
/// and then for its Workspace's Icon and Description; answers with its
/// Workspace as the Session API gives it.
async fn deriving_session(
    descriptor: &RuntimeDescriptor,
    directory: &Path,
    title: &str,
) -> Workspace {
    create_session(
        descriptor,
        &session_request(directory, default_selection(&codex_models()), title),
    )
    .await
    .session
    .workspace
}

/// Answers the Title Errand a fresh Session's creation asks of `provider`,
/// known by its schema rather than by when it arrives.
async fn answer_title_errand(provider: &mut ControlledProvider) {
    let errand = timeout(PROGRESS_DEADLINE, provider.next_errand())
        .await
        .expect("the Title Errand reaches the Provider");
    assert!(
        errand.schema()["properties"]["title"].is_object(),
        "the Title Errand comes first: {}",
        errand.schema()
    );
    errand.succeed(json!({ "title": "Chart the atlas", "icon": "md-bug" }));
}

/// The Errand asking `provider` for a Workspace's Icon and Description,
/// which a fresh Session's creation asks behind its Title Errand.
async fn next_workspace_errand(
    provider: &mut ControlledProvider,
) -> crate::provider_support::ErrandRequest {
    let errand = timeout(PROGRESS_DEADLINE, provider.next_errand())
        .await
        .expect("the Workspace Errand reaches the Provider");
    assert!(
        errand.schema()["properties"]["description"].is_object(),
        "the Workspace Errand asks for a Description: {}",
        errand.schema()
    );
    errand
}

/// A Client of the Server `state_dir` and `channel` name, connected.
async fn connected_client(state_dir: &Path, channel: &str) -> ManagedClient {
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir, channel).expect("configure the Client"),
    )
    .await
    .expect("connect the Client");
    crate::support::receive_managed_client_initial_state(&mut client).await;
    client
}

/// Every Description change `client` hears of until a Workspace's Icon lands.
/// A Workspace Errand lands its Description before its Icon, so once the Icon
/// has landed these are every change its derivation made.
async fn description_changes_until_the_icon_lands(
    client: &mut ManagedClient,
) -> Vec<WorkspaceDescriptionChanged> {
    timeout(PROGRESS_DEADLINE, async {
        let mut changes = Vec::new();
        loop {
            match client.next().await {
                Some(ManagedEvent::WorkspaceIconChanged(_)) => return changes,
                Some(ManagedEvent::WorkspaceDescriptionChanged(changed)) => changes.push(changed),
                _ => {}
            }
        }
    })
    .await
    .expect("the Workspace's Icon lands")
}

#[tokio::test]
async fn a_sidekick_is_offered_the_workspace_tools_and_any_other_agent_is_not() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "sidekick-workspace-tools", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let workspace = hosted.workspace.path().to_owned();
    let (_sidekick, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let (_ordinary, ordinary_handoff, _ordinary_provider) = start_session(
        &descriptor,
        &mut hosted.codex,
        &workspace,
        default_selection(&codex_models()),
    )
    .await;
    let mut ordinary = McpClient::handed(&ordinary_handoff);
    ordinary.initialize().await;

    let listed = sidekick.request("tools/list", json!({})).await;
    let described = |tool: &str| {
        listed["tools"]
            .as_array()
            .expect("tools/list lists Tools")
            .iter()
            .find(|described| described["name"] == json!(tool))
            .unwrap_or_else(|| panic!("a Sidekick is offered {tool}: {listed}"))
            .clone()
    };
    let listing = described("list_workspaces");
    assert_eq!(
        listing["annotations"]["readOnlyHint"],
        json!(true),
        "list_workspaces only reads: {listing}"
    );
    let setting = described("set_workspace_description");
    assert_eq!(
        setting["annotations"]["readOnlyHint"],
        json!(false),
        "set_workspace_description changes what Suru holds: {setting}"
    );
    assert_eq!(
        setting["inputSchema"]["required"],
        json!(["workspace", "text"]),
        "set_workspace_description names the Workspace and the text: {setting}"
    );

    let ordinary_tools = listed_tools(&mut ordinary).await;
    for tool in WORKSPACE_TOOLS {
        assert!(
            !ordinary_tools.iter().any(|listed| listed == tool),
            "a Session elsewhere is not offered {tool}: {ordinary_tools:?}"
        );
        let refused = unoffered(
            &mut ordinary,
            tool,
            json!({ "workspace": workspace, "text": "Mine now." }),
        )
        .await;
        assert_eq!(
            refused["message"],
            json!(format!("The Broker offers no Tool named `{tool}`")),
            "any other caller is answered as though {tool} did not exist: {refused}"
        );
    }

    hosted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn list_workspaces_names_each_workspace_once_with_its_path_description_and_icon_most_recently_worked_in_first()
 {
    const CHANNEL: &str = "sidekick-workspace-listing";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), CHANNEL, None).await;
    let descriptor = hosted.server.descriptor().clone();
    let (sidekick_id, mut sidekick, _provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let sidekick_workspace = read_session(&descriptor, sidekick_id)
        .await
        .session
        .workspace;
    let notes = tempfile::tempdir().expect("create the notes Workspace");
    let atlas = tempfile::tempdir().expect("create the atlas Workspace");

    let notes_workspace = quiet_session(&descriptor, notes.path(), "Keep the notes").await;
    let atlas_workspace = quiet_session(&descriptor, atlas.path(), "Chart the atlas").await;
    let user = connected_client(state_dir.path(), CHANNEL).await;
    user.set_workspace_icon(&notes_workspace.id, "dev-rust")
        .await
        .expect("the user chooses the notes Workspace's Icon");
    user.set_workspace_description(&notes_workspace.id, None, "Where the user keeps notes.")
        .await
        .expect("the user describes the notes Workspace");
    quiet_session(&descriptor, atlas.path(), "Chart the atlas again").await;

    assert_eq!(
        list_workspaces(&mut sidekick).await,
        [
            row(&atlas_workspace, Value::Null, Value::Null),
            row(
                &notes_workspace,
                description("Where the user keeps notes.", true),
                json!("dev-rust"),
            ),
            row(&sidekick_workspace, Value::Null, Value::Null),
        ],
        "every Workspace a Session works in is listed once, most recently worked in first, \
         the Sidekick's own among them, each with what it carries"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_description_a_sidekick_sets_reaches_every_client_as_the_users_does_and_outlives_a_restart()
 {
    const CHANNEL: &str = "sidekick-workspace-description-set";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), CHANNEL, None).await;
    let descriptor = hosted.server.descriptor().clone();
    let (sidekick_id, mut sidekick, sidekick_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let atlas = tempfile::tempdir().expect("create the atlas Workspace");
    let atlas_workspace = quiet_session(&descriptor, atlas.path(), "Chart the atlas").await;
    let mut onlooker = connected_client(state_dir.path(), CHANNEL).await;

    let answer = acted(
        &mut sidekick,
        "set_workspace_description",
        json!({
            "workspace": atlas_workspace.id,
            "text": "  Where the atlas\nis   charted.\n",
        }),
    )
    .await;
    assert_eq!(
        answer,
        json!({
            "workspace_id": atlas_workspace.id,
            "description": description("Where the atlas is charted.", true),
        }),
        "the Description is kept on one line, and set rather than derived"
    );
    let charted = Some(WorkspaceDescription {
        text: "Where the atlas is charted.".to_owned(),
        set: true,
    });
    assert_eq!(
        next_workspace_description_changed(&mut onlooker).await,
        WorkspaceDescriptionChanged {
            workspace_id: atlas_workspace.id.clone(),
            description: charted.clone(),
        },
        "a Client hears of it in the very change the user's own setting makes"
    );
    let latecomer = connected_client(state_dir.path(), CHANNEL).await;
    assert_eq!(
        latecomer
            .list_sessions(None)
            .await
            .expect("list Sessions")
            .iter()
            .find_map(|item| item
                .workspace()
                .filter(|workspace| workspace.id == atlas_workspace.id)
                .map(|workspace| workspace.description.clone())),
        Some(charted),
        "a Client connecting later is given it with the Workspace"
    );
    assert_eq!(
        listed_row(&mut sidekick, &atlas_workspace).await,
        row(
            &atlas_workspace,
            description("Where the atlas is charted.", true),
            Value::Null,
        )
    );

    let answer = acted(
        &mut sidekick,
        "set_workspace_description",
        json!({ "workspace": atlas_workspace.path, "text": "Where the atlas is drawn." }),
    )
    .await;
    assert_eq!(
        answer["description"],
        description("Where the atlas is drawn.", true),
        "a Workspace may be named by its path, as the listing gives it, too"
    );
    assert_eq!(
        next_workspace_description_changed(&mut onlooker)
            .await
            .description
            .map(|description| description.text),
        Some("Where the atlas is drawn.".to_owned())
    );

    sidekick_provider.emit(ProviderEvent::TurnCompleted);
    latest_turn_settles(&descriptor, sidekick_id, TurnStatus::Completed).await;
    drop((onlooker, latecomer, sidekick_provider));
    hosted.server.shutdown().await.expect("stop the server");

    let mut hosted = host_providers(state_dir.path(), CHANNEL, None).await;
    let descriptor = hosted.server.descriptor().clone();
    admit_prompt(&descriptor, sidekick_id, "Where were we?").await;
    let relaunch = next_start(&mut hosted.claude).await;
    let handoff = relaunch
        .broker()
        .cloned()
        .expect("the relaunched Provider is handed the Broker");
    let mut sidekick = McpClient::handed(&handoff);
    sidekick.initialize().await;
    assert_eq!(
        listed_row(&mut sidekick, &atlas_workspace).await["description"],
        description("Where the atlas is drawn.", true),
        "a Description a Sidekick set, and that it was set, outlive a restart"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_description_a_sidekick_sets_stands_against_derivation_and_empty_text_clears_it_to_be_derived_again()
 {
    const CHANNEL: &str = "sidekick-workspace-description-derivation";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), CHANNEL, None).await;
    let descriptor = hosted.server.descriptor().clone();
    let (_sidekick_id, mut sidekick, _sidekick_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let mut user = connected_client(state_dir.path(), CHANNEL).await;
    let atlas = tempfile::tempdir().expect("create the atlas Workspace");

    let atlas_workspace = deriving_session(&descriptor, atlas.path(), "Chart the atlas").await;
    answer_title_errand(&mut hosted.codex).await;
    let errand = next_workspace_errand(&mut hosted.codex).await;
    acted(
        &mut sidekick,
        "set_workspace_description",
        json!({ "workspace": atlas_workspace.id, "text": "Where the atlas is charted." }),
    )
    .await;
    errand.succeed(json!({
        "icon": "dev-rust",
        "description": "Something the Errand made up.",
    }));
    assert_eq!(
        description_changes_until_the_icon_lands(&mut user).await,
        [WorkspaceDescriptionChanged {
            workspace_id: atlas_workspace.id.clone(),
            description: Some(WorkspaceDescription {
                text: "Where the atlas is charted.".to_owned(),
                set: true,
            }),
        }],
        "the derivation announced nothing over the Sidekick's Description"
    );
    assert_eq!(
        listed_row(&mut sidekick, &atlas_workspace).await,
        row(
            &atlas_workspace,
            description("Where the atlas is charted.", true),
            json!("dev-rust"),
        ),
        "the Sidekick's Description stands against derivation, as the user's would, while the \
         absent Icon is derived"
    );

    let answer = acted(
        &mut sidekick,
        "set_workspace_description",
        json!({ "workspace": atlas_workspace.id, "text": "" }),
    )
    .await;
    assert_eq!(
        answer,
        json!({ "workspace_id": atlas_workspace.id, "description": null }),
        "empty text clears the Description"
    );
    assert_eq!(
        next_workspace_description_changed(&mut user).await,
        WorkspaceDescriptionChanged {
            workspace_id: atlas_workspace.id.clone(),
            description: None,
        }
    );
    assert_eq!(
        listed_row(&mut sidekick, &atlas_workspace).await["description"],
        Value::Null
    );

    deriving_session(&descriptor, atlas.path(), "Chart the atlas again").await;
    answer_title_errand(&mut hosted.codex).await;
    next_workspace_errand(&mut hosted.codex)
        .await
        .succeed(json!({ "icon": "md-bug", "description": "Where the atlas is drawn." }));
    assert_eq!(
        next_workspace_description_changed(&mut user)
            .await
            .description,
        Some(WorkspaceDescription {
            text: "Where the atlas is drawn.".to_owned(),
            set: false,
        })
    );
    assert_eq!(
        listed_row(&mut sidekick, &atlas_workspace).await,
        row(
            &atlas_workspace,
            description("Where the atlas is drawn.", false),
            json!("dev-rust"),
        ),
        "a cleared Description is derived again, and read as derived; the Icon that stood is kept"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn every_workspace_refusal_is_a_tool_error_in_words_the_sidekick_can_relay() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "sidekick-workspace-refusals", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let (_sidekick, mut sidekick, _provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let atlas = tempfile::tempdir().expect("create the atlas Workspace");
    let atlas_workspace = quiet_session(&descriptor, atlas.path(), "Chart the atlas").await;
    acted(
        &mut sidekick,
        "set_workspace_description",
        json!({ "workspace": atlas_workspace.id, "text": "Where the atlas is charted." }),
    )
    .await;
    let elsewhere = tempfile::tempdir().expect("create a directory no Session works in");
    let elsewhere = suru::paths::canonical(elsewhere.path()).expect("read the directory");
    let id = json!(atlas_workspace.id);

    for (arguments, refusal) in [
        (
            json!({ "workspace": "directory:unheard-of", "text": "Anything." }),
            "Suru knows no Workspace `directory:unheard-of` on this server; name one by the \
             workspace_id or the path list_workspaces gives it."
                .to_owned(),
        ),
        (
            json!({ "workspace": elsewhere, "text": "Anything." }),
            format!(
                "Suru knows no Workspace `{}` on this server; name one by the workspace_id or \
                 the path list_workspaces gives it.",
                elsewhere.display()
            ),
        ),
        (
            json!({ "text": "Anything." }),
            "set_workspace_description needs `workspace`, the workspace_id or the path of a \
             Workspace as list_workspaces gives it."
                .to_owned(),
        ),
        (
            json!({ "workspace": 7, "text": "Anything." }),
            "set_workspace_description's `workspace` must be the workspace_id or the path of a \
             Workspace as list_workspaces gives it."
                .to_owned(),
        ),
        (
            json!({ "workspace": id }),
            "set_workspace_description needs `text`, the Description to set; give \"\" to clear \
             it, so Suru may derive one again."
                .to_owned(),
        ),
        (
            json!({ "workspace": id, "text": 7 }),
            "set_workspace_description's `text` must be a string: the Description to set, or \"\" \
             to clear it."
                .to_owned(),
        ),
        (
            json!({ "workspace": id, "description": "Anything." }),
            "set_workspace_description takes no argument `description`; it takes `workspace`, \
             `text`."
                .to_owned(),
        ),
        (
            json!({ "workspace": id, "text": "word ".repeat(70) }),
            "A Description runs to at most 300 characters, and this one runs to 349; say it in a \
             sentence or two."
                .to_owned(),
        ),
    ] {
        assert_eq!(
            refused(
                &mut sidekick,
                "set_workspace_description",
                arguments.clone()
            )
            .await,
            refusal,
            "{arguments}"
        );
    }
    assert_eq!(
        refused(&mut sidekick, "list_workspaces", json!({ "limit": 5 })).await,
        "list_workspaces takes no arguments; call it again without limit."
    );
    assert_eq!(
        listed_row(&mut sidekick, &atlas_workspace).await["description"],
        description("Where the atlas is charted.", true),
        "no refused call changed the Description"
    );

    hosted.server.shutdown().await.expect("shut down server");
}
