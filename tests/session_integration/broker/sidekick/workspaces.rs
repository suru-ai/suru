//! `list_workspaces` and `set_workspace_description`: a Sidekick learning
//! which Workspaces its own Server knows — each by its identity, its name, its
//! presented root's path, its Description and its Icon — and recording what
//! it learned a Workspace is for as that Workspace's Description. A Remote's
//! Workspaces are not reached through them yet.
//!
//! A Server knows a Workspace that a Session works in, and one it holds a
//! Description or Icon for though no Session works there. A Description a
//! Sidekick sets is set through the very operations the Workspace endpoint
//! performs for the user — a directory no Session has worked in resolved as
//! the endpoint resolves one — so it stands against every later derivation
//! as the user's does, reaches every Client in the same catalog change, and
//! outlives a restart; text with nothing in it clears it, so it may be
//! derived again. Any other caller neither lists either Tool nor may call it.
//!
//! Each test acts as the MCP client a Sidekick's harness is and asserts on
//! what the Tools answer it, and on what Clients of the Session API observe.

use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, ManagedEvent},
    protocol::{Workspace, WorkspaceDescription, WorkspaceDescriptionChanged},
};

use super::*;
use crate::{
    broker::{
        sidekick_acts::{acted, refused},
        subsessions::{committed, resolve},
    },
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
    let rows = list_workspaces(client).await;
    rows.iter()
        .find(|row| row["workspace_id"] == json!(workspace.id))
        .unwrap_or_else(|| panic!("{} is listed: {rows:?}", workspace.path.display()))
        .clone()
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

/// `set_workspace_description`'s answer to naming `workspace` with `text`.
async fn describe(client: &mut McpClient, workspace: Value, text: &str) -> Value {
    acted(
        client,
        "set_workspace_description",
        json!({ "workspace": workspace, "text": text }),
    )
    .await
}

/// What `set_workspace_description` answers once `workspace` carries
/// `description`.
fn described(workspace: &Workspace, description: Value) -> Value {
    json!({
        "workspace_id": workspace.id,
        "path": workspace.path,
        "description": description,
    })
}

/// A Session in `directory` that names no Agent Selection, so it asks for no
/// Errand and its Workspace carries only what a test gives it; answers with
/// the Session as the Session API gives it.
async fn quiet_session(
    descriptor: &RuntimeDescriptor,
    directory: &Path,
    title: &str,
) -> SessionSnapshot {
    let request = CreateSessionRequest {
        agent_selection: None,
        ..session_request(directory, default_selection(&claude_models()), title)
    };
    create_session(descriptor, &request).await
}

/// A Session on Codex in `directory`, whose creation asks Codex for its Title
/// and for its Workspace's Icon and Description; answers with its Workspace
/// as the Session API gives it.
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

/// The Errand asking `provider` for a Workspace's Icon and Description, told
/// apart from a Title Errand by its schema; any Title Errand met first is
/// answered on the way.
async fn next_workspace_errand(
    provider: &mut ControlledProvider,
) -> crate::provider_support::ErrandRequest {
    timeout(PROGRESS_DEADLINE, async {
        loop {
            let errand = provider.next_errand().await;
            if errand.schema()["properties"]["title"].is_object() {
                errand.succeed(json!({ "title": "Chart the atlas", "icon": "md-bug" }));
                continue;
            }
            assert!(
                errand.schema()["properties"]["description"].is_object(),
                "the Workspace Errand asks for a Description: {}",
                errand.schema()
            );
            return errand;
        }
    })
    .await
    .expect("the Workspace Errand reaches the Provider")
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

/// Deletes `session_id`, as a Client does.
async fn delete_session(descriptor: &RuntimeDescriptor, session_id: SessionId) {
    reqwest::Client::new()
        .delete(format!("{}/v1/sessions/{session_id}", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("delete the Session")
        .error_for_status()
        .expect("the Session is deleted");
}

/// The Sidekick of a restarted Server, as its Provider is relaunched for a
/// Prompt the user sends it, and that relaunch, which keeps the Sidekick's
/// token live for as long as it is held.
async fn relaunched_sidekick(
    descriptor: &RuntimeDescriptor,
    claude: &mut ControlledProvider,
    sidekick_id: SessionId,
) -> (McpClient, crate::provider_support::StartRequest) {
    admit_prompt(descriptor, sidekick_id, "Where were we?").await;
    let relaunch = next_start(claude).await;
    let handoff = relaunch
        .broker()
        .cloned()
        .expect("the relaunched Provider is handed the Broker");
    let mut sidekick = McpClient::handed(&handoff);
    sidekick.initialize().await;
    (sidekick, relaunch)
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

    let notes_workspace = quiet_session(&descriptor, notes.path(), "Keep the notes")
        .await
        .session
        .workspace;
    let atlas_workspace = quiet_session(&descriptor, atlas.path(), "Chart the atlas")
        .await
        .session
        .workspace;
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

/// A Workspace is known to the Server without a Session once it holds a
/// Description for it: one the user described on the Landing before beginning
/// work there, one a Sidekick described by naming its directory, and one
/// whose every Session has since been deleted. Each is listed, after a
/// restart too; a path that names no directory names no Workspace.
#[tokio::test]
async fn a_workspace_known_without_a_session_is_listed_and_a_sidekick_may_describe_one_before_work_begins_there()
 {
    const CHANNEL: &str = "sidekick-workspace-without-session";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), CHANNEL, None).await;
    let descriptor = hosted.server.descriptor().clone();
    let (sidekick_id, mut sidekick, sidekick_provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let sidekick_workspace = read_session(&descriptor, sidekick_id)
        .await
        .session
        .workspace;
    let drafts = tempfile::tempdir().expect("create the drafts Workspace");
    let atlas = tempfile::tempdir().expect("create the atlas Workspace");
    let maps = tempfile::tempdir().expect("create the maps Workspace");
    let mut user = connected_client(state_dir.path(), CHANNEL).await;

    let drafts_workspace = resolve(&descriptor, drafts.path()).await.workspace;
    user.set_workspace_description(
        &drafts_workspace.id,
        Some(&drafts_workspace.path),
        "Where drafts wait.",
    )
    .await
    .expect("the user describes the Landing's Workspace before beginning work there");
    next_workspace_description_changed(&mut user).await;

    let atlas_workspace = resolve(&descriptor, atlas.path()).await.workspace;
    assert_eq!(
        describe(
            &mut sidekick,
            json!(atlas.path()),
            "Where the atlas will be charted."
        )
        .await,
        described(
            &atlas_workspace,
            description("Where the atlas will be charted.", true)
        ),
        "a directory no Session works in names the Workspace the Server resolves it to"
    );
    assert_eq!(
        next_workspace_description_changed(&mut user).await,
        WorkspaceDescriptionChanged {
            workspace_id: atlas_workspace.id.clone(),
            description: Some(WorkspaceDescription {
                text: "Where the atlas will be charted.".to_owned(),
                set: true,
            }),
        },
        "every Client hears of it as of any Description"
    );

    let (mapping, _mapping_handoff, mapping_provider) = start_session(
        &descriptor,
        &mut hosted.codex,
        maps.path(),
        default_selection(&codex_models()),
    )
    .await;
    let maps_workspace = read_session(&descriptor, mapping).await.session.workspace;
    describe(
        &mut sidekick,
        json!(maps_workspace.id),
        "Where the maps are drawn.",
    )
    .await;
    mapping_provider.emit(ProviderEvent::TurnCompleted);
    latest_turn_settles(&descriptor, mapping, TurnStatus::Completed).await;
    delete_session(&descriptor, mapping).await;

    let missing = maps_workspace.path.join("missing");
    assert_eq!(
        refused(
            &mut sidekick,
            "set_workspace_description",
            json!({ "workspace": missing, "text": "Anything." }),
        )
        .await,
        format!(
            "Suru knows no Workspace `{}` on this server, and no directory is there to find one \
             in; name a Workspace by the workspace_id or the path list_workspaces gives it, or \
             by the absolute path of a directory in it.",
            missing.display()
        )
    );

    let mut unworked = vec![
        row(
            &drafts_workspace,
            description("Where drafts wait.", true),
            Value::Null,
        ),
        row(
            &atlas_workspace,
            description("Where the atlas will be charted.", true),
            Value::Null,
        ),
        row(
            &maps_workspace,
            description("Where the maps are drawn.", true),
            Value::Null,
        ),
    ];
    unworked.sort_by(|left, right| left["path"].as_str().cmp(&right["path"].as_str()));
    let expected = std::iter::once(row(&sidekick_workspace, Value::Null, Value::Null))
        .chain(unworked)
        .collect::<Vec<_>>();
    assert_eq!(
        list_workspaces(&mut sidekick).await,
        expected,
        "the Workspaces Sessions work in come first, then those known without one, by path"
    );

    let begun = quiet_session(&descriptor, atlas.path(), "Chart the atlas").await;
    assert_eq!(
        begun.session.workspace.description,
        Some(WorkspaceDescription {
            text: "Where the atlas will be charted.".to_owned(),
            set: true,
        }),
        "work begun there finds the Description the Sidekick set beforehand"
    );

    sidekick_provider.emit(ProviderEvent::TurnCompleted);
    latest_turn_settles(&descriptor, sidekick_id, TurnStatus::Completed).await;
    drop((user, sidekick_provider));
    hosted.server.shutdown().await.expect("stop the server");

    let mut hosted = host_providers(state_dir.path(), CHANNEL, None).await;
    let descriptor = hosted.server.descriptor().clone();
    let (mut sidekick, _relaunch) =
        relaunched_sidekick(&descriptor, &mut hosted.claude, sidekick_id).await;
    for (workspace, text) in [
        (&drafts_workspace, "Where drafts wait."),
        (&maps_workspace, "Where the maps are drawn."),
    ] {
        assert_eq!(
            listed_row(&mut sidekick, workspace).await,
            row(workspace, description(text, true), Value::Null),
            "a Workspace known without a Session is listed after a restart"
        );
    }

    hosted.server.shutdown().await.expect("shut down server");
}

/// On a filesystem that keeps them apart, `atlas` and `atlas ` are two
/// directories, and so two Workspaces: each is named by its path exactly as
/// the listing gives it.
#[cfg(unix)]
#[tokio::test]
async fn a_workspace_is_named_by_its_path_exactly_as_given() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "sidekick-workspace-exact-path", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let (_sidekick, mut sidekick, _provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let home = tempfile::tempdir().expect("create a home for the Workspaces");
    let plain = home.path().join("atlas");
    let spaced = home.path().join("atlas ");
    std::fs::create_dir(&plain).expect("create the plain directory");
    std::fs::create_dir(&spaced).expect("create the spaced directory");
    let plain_workspace = quiet_session(&descriptor, &plain, "Chart the atlas")
        .await
        .session
        .workspace;
    let spaced_workspace = quiet_session(&descriptor, &spaced, "Chart the spaced atlas")
        .await
        .session
        .workspace;
    let spaced_path = listed_row(&mut sidekick, &spaced_workspace).await["path"].clone();
    assert!(
        spaced_path
            .as_str()
            .is_some_and(|path| path.ends_with("atlas ")),
        "the listing gives the path as it is: {spaced_path}"
    );

    assert_eq!(
        describe(&mut sidekick, spaced_path.clone(), "The spaced one.").await,
        described(&spaced_workspace, description("The spaced one.", true)),
        "the Workspace at the path given is described, and no other"
    );
    assert_eq!(
        listed_row(&mut sidekick, &plain_workspace).await["description"],
        Value::Null
    );
    assert_eq!(
        titles(
            &list_sessions(
                &mut sidekick,
                json!({ "workspace": spaced_path, "liveness": "all" })
            )
            .await
        ),
        ["Chart the spaced atlas"],
        "list_sessions narrows to the Workspace at the path given, and no other"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

/// A Managed Worktree's Session works in its Repository's Workspace, which is
/// listed once, by its presented root's path; a directory within it, the
/// Worktree's own included, names that Workspace.
#[tokio::test]
async fn a_managed_worktrees_session_is_listed_under_its_repositorys_workspace() {
    let temporary = tempfile::tempdir().expect("create a home for the Repository");
    let main = suru::paths::canonical(temporary.path())
        .expect("read the home")
        .join("auth");
    committed(&main);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "sidekick-workspace-worktree", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let (_sidekick, mut sidekick, _provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;

    let begun = acted(
        &mut sidekick,
        "begin_session",
        json!({ "directory": main, "prompt": "Bump the auth dependency.", "new_worktree": true }),
    )
    .await;
    let worktree = PathBuf::from(
        begun["directory"]
            .as_str()
            .expect("begin_session says where the Session works"),
    );
    assert_ne!(worktree, main, "the Session works in a Worktree of its own");
    let repository = resolve(&descriptor, &main).await.workspace;

    let rows = list_workspaces(&mut sidekick).await;
    assert_eq!(
        rows.iter()
            .filter(|row| row["workspace_id"] == json!(repository.id))
            .cloned()
            .collect::<Vec<_>>(),
        [row(&repository, Value::Null, Value::Null)],
        "the Repository's Workspace is listed once, by its main checkout's root: {rows:?}"
    );
    assert!(
        rows.iter().all(|row| row["path"] != json!(worktree)),
        "no row names the Worktree: {rows:?}"
    );

    assert_eq!(
        describe(&mut sidekick, json!(worktree), "Where auth is kept.").await,
        described(&repository, description("Where auth is kept.", true)),
        "the Worktree's directory names its Repository's Workspace"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

/// A row's `workspace_id` and `path` name its Workspace to every Tool that
/// takes one, and a Workspace whose every Session is settled is listed still,
/// as the Workspace Picker lists it.
#[tokio::test]
async fn a_row_names_its_workspace_to_each_tool_that_takes_one() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut hosted = host_providers(state_dir.path(), "sidekick-workspace-round-trip", None).await;
    let descriptor = hosted.server.descriptor().clone();
    let (_sidekick, mut sidekick, _provider) =
        start_sidekick(&descriptor, &mut hosted.claude).await;
    let atlas = tempfile::tempdir().expect("create the atlas Workspace");
    let notes = tempfile::tempdir().expect("create the notes Workspace");
    let atlas_workspace = quiet_session(&descriptor, atlas.path(), "Chart the atlas")
        .await
        .session
        .workspace;
    let noting = quiet_session(&descriptor, notes.path(), "Keep the notes").await;
    acted(
        &mut sidekick,
        "settle_session",
        json!({ "session_id": noting.session.id }),
    )
    .await;

    let atlas_row = listed_row(&mut sidekick, &atlas_workspace).await;
    let notes_row = listed_row(&mut sidekick, &noting.session.workspace).await;
    assert_eq!(
        notes_row,
        row(&noting.session.workspace, Value::Null, Value::Null),
        "a Workspace whose every Session is settled is listed"
    );

    assert_eq!(
        describe(
            &mut sidekick,
            atlas_row["workspace_id"].clone(),
            "Where the atlas is charted."
        )
        .await,
        described(
            &atlas_workspace,
            description("Where the atlas is charted.", true)
        )
    );
    assert_eq!(
        describe(&mut sidekick, atlas_row["workspace_id"].clone(), " \n\t ").await,
        described(&atlas_workspace, Value::Null),
        "text with nothing in it clears the Description"
    );
    assert_eq!(
        listed_row(&mut sidekick, &atlas_workspace).await["description"],
        Value::Null
    );

    for named in [atlas_row["workspace_id"].clone(), atlas_row["path"].clone()] {
        assert_eq!(
            titles(&list_sessions(&mut sidekick, json!({ "workspace": named })).await),
            ["Chart the atlas"],
            "list_sessions narrows to the Workspace a row names by {named}"
        );
    }
    assert_eq!(
        titles(
            &list_sessions(
                &mut sidekick,
                json!({ "workspace": notes_row["path"], "liveness": "settled" })
            )
            .await
        ),
        ["Keep the notes"]
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
    let atlas_workspace = quiet_session(&descriptor, atlas.path(), "Chart the atlas")
        .await
        .session
        .workspace;
    let mut onlooker = connected_client(state_dir.path(), CHANNEL).await;

    assert_eq!(
        describe(
            &mut sidekick,
            json!(atlas_workspace.id),
            "  Where the atlas\nis   charted.\n"
        )
        .await,
        described(
            &atlas_workspace,
            description("Where the atlas is charted.", true)
        ),
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

    assert_eq!(
        describe(
            &mut sidekick,
            json!(atlas_workspace.path),
            "Where the atlas is drawn."
        )
        .await["description"],
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
    let (mut sidekick, _relaunch) =
        relaunched_sidekick(&descriptor, &mut hosted.claude, sidekick_id).await;
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
    let errand = next_workspace_errand(&mut hosted.codex).await;
    describe(
        &mut sidekick,
        json!(atlas_workspace.id),
        "Where the atlas is charted.",
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

    assert_eq!(
        describe(&mut sidekick, json!(atlas_workspace.id), "").await,
        described(&atlas_workspace, Value::Null),
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
    let atlas_workspace = quiet_session(&descriptor, atlas.path(), "Chart the atlas")
        .await
        .session
        .workspace;
    describe(
        &mut sidekick,
        json!(atlas_workspace.id),
        "Where the atlas is charted.",
    )
    .await;
    let id = json!(atlas_workspace.id);
    let unknown = |named: &str| {
        format!(
            "Suru knows no Workspace `{named}` on this server, and no directory is there to find \
             one in; name a Workspace by the workspace_id or the path list_workspaces gives it, or \
             by the absolute path of a directory in it."
        )
    };

    for (arguments, refusal) in [
        (
            json!({ "workspace": "directory:unheard-of", "text": "Anything." }),
            unknown("directory:unheard-of"),
        ),
        (
            json!({ "workspace": "atlas", "text": "Anything." }),
            unknown("atlas"),
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
            json!({ "workspace": "  ", "text": "Anything." }),
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
