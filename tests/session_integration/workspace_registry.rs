//! The Workspace Registry: one entry for every Workspace a Server has known,
//! made the first time it is seen — a Session created there, or a Session's
//! directory re-resolved at start, or a Description or Icon set for it — and
//! the one list the Server's Workspace listing reads (ADR 0051). Each test
//! drives the Server as a Client does and asserts on what the Client's
//! Workspace listing answers, or on what the Registry left on disk.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use diesel::{Connection, RunQueryDsl, SqliteConnection, connection::SimpleConnection};
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig},
    protocol::{
        CreateSessionRequest, ExecutionDirectory, InitialPrompt, Outlook, PromptId,
        ResolveWorkspaceRequest, Workspace, WorkspaceDescription, WorkspaceId,
    },
    server::{self, ServerConfig, ServerTimings},
};
use tokio::time::timeout;

use crate::{
    failing_provider_support::{FailingProviderRuntime, spawn_with_failing_provider},
    repositories::git,
    server_support::PROGRESS_DEADLINE,
    support,
};

/// A first Prompt naming no Agent Selection, so its Session asks for no
/// Errand and its Workspace carries only what a test gives it.
fn quiet_request(directory: &Path, prompt: &str) -> CreateSessionRequest {
    CreateSessionRequest {
        session_id: None,
        preparation_id: None,
        agent_selection: None,
        execution_directory: ExecutionDirectory {
            path: directory.to_owned(),
        },
        prompt: InitialPrompt {
            id: PromptId::new(),
            text: prompt.to_owned(),
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
        },
    }
}

async fn connected_client(state_dir: &Path, channel: &str) -> ManagedClient {
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir, channel).expect("configure client"),
    )
    .await
    .expect("connect client");
    support::receive_managed_client_initial_state(&mut client).await;
    client
}

/// The Workspace `directory` lies in, as the Landing asks the Server for it
/// before any Session has begun there.
async fn landing_workspace(client: &ManagedClient, directory: &Path) -> Workspace {
    client
        .outlook(Outlook::Local)
        .resolve_workspace(ResolveWorkspaceRequest {
            checkout_id: None,
            remembered_execution_directory: None,
            workspace_id: None,
            base: None,
            path: directory.to_owned(),
        })
        .await
        .expect("resolve the Landing's Workspace")
        .workspace
}

/// The Workspaces the Client's Workspace listing answers with, in its order.
async fn listed(client: &ManagedClient) -> Vec<Workspace> {
    client
        .list_workspaces()
        .await
        .expect("list Workspaces")
        .workspaces
}

/// A directory as the Server reads it, so a path the listing gives compares
/// equal to it on every platform.
fn canonical(path: &Path) -> PathBuf {
    suru::paths::canonical(path).expect("read the directory canonically")
}

/// What a listing says of a Workspace a reader tells apart: its identity,
/// where it is presented, and what it carries.
fn named(
    workspace: &Workspace,
) -> (
    WorkspaceId,
    PathBuf,
    Option<String>,
    Option<WorkspaceDescription>,
) {
    (
        workspace.id.clone(),
        workspace.path.clone(),
        workspace.icon.clone(),
        workspace.description.clone(),
    )
}

/// A Repository at `root` with one commit on `main`.
fn repository(root: &Path) {
    std::fs::create_dir_all(root).expect("create the Repository");
    git(root, &["init", "-b", "main"]);
    git(
        root,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ],
    );
}

/// A Server that leaves the Worktrees it knows alone, so no Git it runs
/// keeps a Repository's directory open when a test lets go of it, which
/// Windows refuses to delete.
async fn quiet_server(config: ServerConfig) -> server::RunningServer {
    server::spawn_with_provider_and_timings(
        config,
        Arc::new(FailingProviderRuntime),
        ServerTimings::default().with_checkout_observation_interval(Duration::from_secs(60 * 60)),
    )
    .await
    .expect("spawn server")
}

/// One Registry entry as the `workspaces` table holds it.
#[derive(diesel::QueryableByName)]
struct StoredEntry {
    #[diesel(sql_type = diesel::sql_types::Text)]
    id: String,
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    path: Option<String>,
    #[diesel(sql_type = diesel::sql_types::Text)]
    provenance: String,
}

/// The Server's database, opened beside it or after it.
fn database(config: &ServerConfig) -> SqliteConnection {
    SqliteConnection::establish(
        config
            .data_dir()
            .join("suru.db")
            .to_str()
            .expect("the fixture's database path is UTF-8"),
    )
    .expect("open the Server's database")
}

/// What the Registry left on disk for each Workspace, by its identity: the
/// path it is presented by and how it came to be known, as the table spells
/// them.
fn entries_on_disk(config: &ServerConfig) -> HashMap<WorkspaceId, (Option<PathBuf>, String)> {
    diesel::sql_query("SELECT id, path, provenance FROM workspaces")
        .load::<StoredEntry>(&mut database(config))
        .expect("read the Registry")
        .into_iter()
        .map(|entry| {
            (
                WorkspaceId(entry.id),
                (entry.path.map(PathBuf::from), entry.provenance),
            )
        })
        .collect()
}

/// How the table spells a Workspace known by a Session.
const BY_SESSION: &str = "\"session\"";
/// How the table spells a Workspace known by its Description.
const BY_DESCRIPTION: &str = "\"description\"";

/// A Workspace a Session began in is the Server's to list from then on: once
/// its every Session is deleted it is listed still, by the path it is
/// presented by, and after a restart it is listed from the Registry alone,
/// with no Session there whose directory could be re-resolved.
#[tokio::test]
async fn a_workspace_is_listed_after_its_sessions_are_deleted_and_the_server_restarts() {
    const CHANNEL: &str = "workspace-registry-outlives-sessions";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let notes = tempfile::tempdir().expect("create the notes Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), CHANNEL).expect("configure"),
    )
    .await
    .expect("spawn server");
    let client = connected_client(state_dir.path(), CHANNEL).await;

    let created = client
        .create_session(quiet_request(notes.path(), "Keep the notes"))
        .await
        .expect("create a Session in the notes Workspace");
    let workspace = created.session.workspace.clone();
    assert_eq!(workspace.path, canonical(notes.path()));
    client
        .delete_session(created.session.id)
        .await
        .expect("delete the Workspace's only Session");

    assert_eq!(
        listed(&client).await,
        std::slice::from_ref(&workspace),
        "a Workspace a Session began in is listed once no Session works there"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
    let restarted = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), CHANNEL).expect("configure"),
    )
    .await
    .expect("respawn server");
    let client = connected_client(state_dir.path(), CHANNEL).await;

    assert!(
        client
            .list_sessions(None)
            .await
            .expect("list Sessions")
            .is_empty(),
        "no Session is left to name the Workspace"
    );
    assert_eq!(
        listed(&client).await,
        [workspace],
        "the Registry lists the Workspace after a restart, by its presented root"
    );

    restarted.shutdown().await.expect("shut down server");
}

/// Creating a Session enters its Workspace in the Registry by the root it is
/// presented by, known by a Session. A Workspace described before any
/// Session began there was known by its Description, and stays so once one
/// does, keeping its Icon and Description.
#[tokio::test]
async fn a_session_enters_its_workspace_by_its_presented_root_and_an_entry_keeps_how_it_was_known()
{
    const CHANNEL: &str = "workspace-registry-provenance";
    let home = tempfile::tempdir().expect("create a home for the Workspaces");
    let home = canonical(home.path());
    let atlas = home.join("atlas");
    repository(&atlas);
    let src = atlas.join("src");
    std::fs::create_dir(&src).expect("create a directory within the Repository");
    let drafts = home.join("drafts");
    std::fs::create_dir(&drafts).expect("create the drafts Workspace");
    let config = ServerConfig::new(home.join("state"), CHANNEL).expect("configure");
    let server = quiet_server(config.clone()).await;
    let client = connected_client(&home.join("state"), CHANNEL).await;

    let drafts_id = landing_workspace(&client, &drafts).await.id;
    client
        .set_workspace_description(&drafts_id, Some(&drafts), "Where drafts wait.")
        .await
        .expect("describe the drafts Workspace before any Session begins there");
    client
        .set_workspace_icon(&drafts_id, "dev-rust")
        .await
        .expect("choose the drafts Workspace's Icon");
    let drafted = client
        .create_session(quiet_request(&drafts, "Draft the notes"))
        .await
        .expect("begin a Session in the drafts Workspace")
        .session
        .workspace;
    let described = Some(WorkspaceDescription {
        text: "Where drafts wait.".to_owned(),
        set: true,
    });
    assert_eq!(
        named(&drafted),
        (
            drafts_id.clone(),
            drafts.clone(),
            Some("dev-rust".to_owned()),
            described.clone()
        ),
        "the Session finds the Icon and Description its Workspace's entry carries"
    );
    let charted = client
        .create_session(quiet_request(&src, "Chart the atlas"))
        .await
        .expect("begin a Session within the Repository")
        .session
        .workspace;
    assert_eq!(
        charted.path, atlas,
        "the Repository is presented by its root"
    );

    assert_eq!(
        listed(&client).await.iter().map(named).collect::<Vec<_>>(),
        [named(&charted), named(&drafted)],
        "each Workspace is listed once, most recently worked in first, by its presented root"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
    let entries = entries_on_disk(&config);
    assert_eq!(
        entries.get(&charted.id),
        Some(&(Some(atlas), BY_SESSION.to_owned())),
        "the Session entered its Workspace by its presented root"
    );
    assert_eq!(
        entries.get(&drafts_id),
        Some(&(Some(drafts), BY_DESCRIPTION.to_owned())),
        "a Session beginning in a described Workspace leaves it known by its Description"
    );
}

/// A Session whose Workspace the Registry holds no entry for — one stored
/// before entries were kept for every Workspace — still groups under its own
/// copy of its Workspace, and re-resolving its directory at start enters the
/// Workspace, by its presented root and known by a Session, so it is listed
/// from then on, its Sessions or no.
#[tokio::test]
async fn re_resolving_a_sessions_directory_at_start_enters_its_workspace() {
    const CHANNEL: &str = "workspace-registry-re-resolution";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let notes = tempfile::tempdir().expect("create the notes Workspace");
    let config = ServerConfig::new(state_dir.path(), CHANNEL).expect("configure");
    let server = spawn_with_failing_provider(config.clone())
        .await
        .expect("spawn server");
    let client = connected_client(state_dir.path(), CHANNEL).await;
    let created = client
        .create_session(quiet_request(notes.path(), "Keep the notes"))
        .await
        .expect("create a Session in the notes Workspace");
    let workspace = created.session.workspace.clone();
    drop(client);
    server.shutdown().await.expect("shut down server");
    database(&config)
        .batch_execute("DELETE FROM workspaces;")
        .expect("forget every Registry entry, as a store from before the Registry would");

    let restarted = spawn_with_failing_provider(config.clone())
        .await
        .expect("respawn server");
    let client = connected_client(state_dir.path(), CHANNEL).await;
    let sessions = client.list_sessions(None).await.expect("list Sessions");
    assert_eq!(
        sessions
            .iter()
            .filter_map(suru::protocol::SessionListItem::workspace)
            .collect::<Vec<_>>(),
        [&workspace],
        "the Session groups under its own copy of its Workspace"
    );
    timeout(PROGRESS_DEADLINE, async {
        while !listed(&client).await.contains(&workspace) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("re-resolving the Session's directory enters its Workspace");
    client
        .delete_session(created.session.id)
        .await
        .expect("delete the Workspace's only Session");
    assert_eq!(
        listed(&client).await,
        std::slice::from_ref(&workspace),
        "the Workspace is listed by its entry, with no Session left there"
    );

    drop(client);
    restarted.shutdown().await.expect("shut down server");
    assert_eq!(
        entries_on_disk(&config).get(&workspace.id),
        Some(&(Some(workspace.path.clone()), BY_SESSION.to_owned())),
        "the entry re-resolution made is known by a Session"
    );
}

/// The migration that made the workspaces table the Registry says how each
/// row it found came to be known: by a Session where one is grouped under
/// its Workspace, and by its Description otherwise. Each keeps what it
/// carries, and is listed as before.
#[tokio::test]
async fn a_row_stored_before_entries_kept_their_provenance_is_known_by_a_session_or_by_its_description()
 {
    const CHANNEL: &str = "workspace-registry-backfill";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let atlas = tempfile::tempdir().expect("create the atlas Workspace");
    let drafts = tempfile::tempdir().expect("create the drafts Workspace");
    let drafts = canonical(drafts.path());
    let config = ServerConfig::new(state_dir.path(), CHANNEL).expect("configure");
    let server = spawn_with_failing_provider(config.clone())
        .await
        .expect("spawn server");
    let client = connected_client(state_dir.path(), CHANNEL).await;
    let charted = client
        .create_session(quiet_request(atlas.path(), "Chart the atlas"))
        .await
        .expect("create a Session in the atlas Workspace")
        .session
        .workspace;
    client
        .set_workspace_description(&charted.id, None, "Where the atlas is charted.")
        .await
        .expect("describe the atlas Workspace");
    let drafts_id = landing_workspace(&client, &drafts).await.id;
    client
        .set_workspace_description(&drafts_id, Some(&drafts), "Where drafts wait.")
        .await
        .expect("describe the drafts Workspace, where no Session works");
    let before = listed(&client).await;
    drop(client);
    server.shutdown().await.expect("shut down server");
    database(&config)
        .batch_execute(
            "ALTER TABLE workspaces DROP COLUMN provenance;
             DELETE FROM __diesel_schema_migrations WHERE version = '20261012000000';",
        )
        .expect("age the table back to before entries kept their provenance");

    let migrated = spawn_with_failing_provider(config.clone())
        .await
        .expect("spawn server over the aged database");
    let client = connected_client(state_dir.path(), CHANNEL).await;
    assert_eq!(
        listed(&client).await,
        before,
        "every row is listed as it was, carrying what it carried"
    );
    drop(client);
    migrated.shutdown().await.expect("shut down server");

    let entries = entries_on_disk(&config);
    assert_eq!(
        entries
            .get(&charted.id)
            .map(|(_, provenance)| provenance.as_str()),
        Some(BY_SESSION),
        "a row a Session is grouped under was known by that Session"
    );
    assert_eq!(
        entries
            .get(&drafts_id)
            .map(|(_, provenance)| provenance.as_str()),
        Some(BY_DESCRIPTION),
        "a row no Session is grouped under was known by its Description"
    );
}
