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
    managed_client::{ManagedClient, ManagedClientConfig, ManagedEvent},
    protocol::{
        CreateSessionRequest, ExecutionDirectory, InitialPrompt, Outlook, PromptId,
        ResolveWorkspaceRequest, RuntimeDescriptor, SessionId, SessionListItem,
        WORKSPACE_LISTING_EVENT, Workspace, WorkspaceDescription, WorkspaceId, WorkspaceListing,
        WorkspaceUnavailability,
    },
    server::{self, RunningServer, ServerConfig, ServerTimings},
};

use crate::{
    failing_provider_support::{FailingProviderRuntime, spawn_with_failing_provider},
    repositories::git,
    support,
};

/// A first Prompt naming no Agent Selection, so its Session asks for no
/// Errand and its Workspace carries only what a test gives it.
pub(super) fn quiet_request(directory: &Path, prompt: &str) -> CreateSessionRequest {
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

pub(super) async fn connected_client(state_dir: &Path, channel: &str) -> ManagedClient {
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
pub(super) async fn landing_workspace(client: &ManagedClient, directory: &Path) -> Workspace {
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
pub(super) async fn listed(client: &ManagedClient) -> Vec<Workspace> {
    client
        .list_workspaces()
        .await
        .expect("list Workspaces")
        .workspaces
}

/// The Workspaces the Client's Workspace listing answers once `server` has
/// caught up with what it reads behind a listing (ADR 0052): with what
/// changed before this asked, and with the reading the listing itself
/// began, so nothing that listing began is still reading when the caller
/// goes on to change the disk or the Roots — where a scan of the Roots, a
/// change of them superseding it, would leave its reading outstanding, and
/// the change's own scan pass over what it read.
pub(super) async fn listed_caught_up(
    server: &RunningServer,
    client: &ManagedClient,
) -> Vec<Workspace> {
    server.workspace_listing_caught_up().await;
    let listing = listed(client).await;
    server.workspace_listing_caught_up().await;
    listing
}

/// A directory as the Server reads it, so a path the listing gives compares
/// equal to it on every platform.
pub(super) fn canonical(path: &Path) -> PathBuf {
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
pub(super) fn repository(root: &Path) {
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
pub(super) fn database(config: &ServerConfig) -> SqliteConnection {
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
pub(super) fn entries_on_disk(
    config: &ServerConfig,
) -> HashMap<WorkspaceId, (Option<PathBuf>, String)> {
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

/// Takes the Server's database back to before the workspaces table was the
/// Registry, with `then` run against it there: the migration that makes it
/// one runs again at the next start.
fn age_to_before_the_registry(config: &ServerConfig, then: &str) {
    database(config)
        .batch_execute(&format!(
            "ALTER TABLE workspaces DROP COLUMN provenance;
             ALTER TABLE workspaces DROP COLUMN presentation;
             DELETE FROM __diesel_schema_migrations WHERE version = '20261012000000';
             {then}"
        ))
        .expect("age the database back to before the Registry");
}

/// Leaves the Session `session_id` stored but unreadable — its status no
/// longer reads — while what it says of its Workspace still does.
fn unreadable(config: &ServerConfig, session_id: SessionId) {
    diesel::sql_query("UPDATE sessions SET status = 'no longer a status' WHERE id = ?")
        .bind::<diesel::sql_types::Text, _>(session_id.to_string())
        .execute(&mut database(config))
        .expect("leave the Session unreadable");
}

/// How the table spells a Workspace known by a Session.
pub(super) const BY_SESSION: &str = "\"session\"";
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

/// The Workspaces the Client's Workspace listing presents at `path`.
async fn listed_at(client: &ManagedClient, path: &Path) -> Vec<Workspace> {
    listed(client)
        .await
        .into_iter()
        .filter(|workspace| workspace.path == path)
        .collect()
}

/// A Description as a user sets it.
pub(super) fn set(text: &str) -> Option<WorkspaceDescription> {
    Some(WorkspaceDescription {
        text: text.to_owned(),
        set: true,
    })
}

/// A Workspace a Session is found working in when its directory is
/// resolved again at start — one it was never grouped under, since its
/// directory became a Repository while the Server was down — is entered then,
/// by its presented root and known by a Session, and listed from then on,
/// its Sessions or no. The plain directory's own Workspace it supersedes at
/// that path is dropped, and the Description the user gave it passes to the
/// Repository's, so the one directory is listed once and nothing the user
/// described is lost.
#[tokio::test]
async fn a_workspace_a_session_is_found_in_when_resolved_again_at_start_supersedes_its_directorys()
{
    const CHANNEL: &str = "workspace-registry-re-resolution";
    let home = tempfile::tempdir().expect("create a home for the Workspaces");
    let home = canonical(home.path());
    let notes = home.join("notes");
    std::fs::create_dir(&notes).expect("create the notes directory");
    let config = ServerConfig::new(home.join("state"), CHANNEL).expect("configure");
    let server = quiet_server(config.clone()).await;
    let client = connected_client(&home.join("state"), CHANNEL).await;
    let created = client
        .create_session(quiet_request(&notes, "Keep the notes"))
        .await
        .expect("create a Session in the notes directory");
    let directory = created.session.workspace.clone();
    assert!(directory.repository.is_none());
    client
        .set_workspace_description(&directory.id, None, "Where the notes are kept.")
        .await
        .expect("describe the notes directory");
    drop(client);
    server.shutdown().await.expect("shut down server");
    repository(&notes);

    let restarted = quiet_server(config.clone()).await;
    restarted.workspace_discovery_settled().await;
    let client = connected_client(&home.join("state"), CHANNEL).await;
    let regrouped = client
        .read_session(created.session.id)
        .await
        .expect("read the Session")
        .session
        .workspace;
    assert!(
        regrouped.repository.is_some(),
        "the Session works in the Repository its directory became"
    );
    assert_eq!(
        regrouped.description,
        set("Where the notes are kept."),
        "the Session's Workspace wears the Description its directory was given"
    );
    client
        .delete_session(created.session.id)
        .await
        .expect("delete the Workspace's only Session");
    assert_eq!(
        listed_at(&client, &notes)
            .await
            .iter()
            .map(named)
            .collect::<Vec<_>>(),
        [(
            regrouped.id.clone(),
            notes.clone(),
            None,
            set("Where the notes are kept.")
        )],
        "the directory is listed once, as the Repository's, with the Description it was given, \
         and with no Session left there"
    );

    drop(client);
    restarted.shutdown().await.expect("shut down server");
    let entries = entries_on_disk(&config);
    assert_eq!(
        entries.get(&regrouped.id),
        Some(&(Some(notes), BY_SESSION.to_owned())),
        "the entry re-resolution made is known by a Session"
    );
    assert!(
        !entries.contains_key(&directory.id),
        "the superseded directory's entry is dropped for good"
    );
}

/// A Session begun in a directory that has become a Repository's finds the
/// directory's own Workspace superseded there, but an entry a Session still
/// works in stands until that Session's directory is resolved again. Once
/// none does, the next Session begun there drops it, and what it carried
/// passes to the Repository's Workspace, for every Session working there.
#[tokio::test]
async fn a_session_begun_where_a_directory_became_a_repository_supersedes_the_directorys_entry() {
    const CHANNEL: &str = "workspace-registry-superseded-at-creation";
    let home = tempfile::tempdir().expect("create a home for the Workspaces");
    let home = canonical(home.path());
    let notes = home.join("notes");
    std::fs::create_dir(&notes).expect("create the notes directory");
    let config = ServerConfig::new(home.join("state"), CHANNEL).expect("configure");
    let server = quiet_server(config).await;
    let client = connected_client(&home.join("state"), CHANNEL).await;
    let first = client
        .create_session(quiet_request(&notes, "Keep the notes"))
        .await
        .expect("create a Session in the notes directory")
        .session;
    client
        .set_workspace_icon(&first.workspace.id, "dev-rust")
        .await
        .expect("choose the notes directory's Icon");
    client
        .set_workspace_description(&first.workspace.id, None, "Where the notes are kept.")
        .await
        .expect("describe the notes directory");
    repository(&notes);

    let second = client
        .create_session(quiet_request(&notes, "File the notes"))
        .await
        .expect("create a Session in the Repository the directory became")
        .session;
    assert_ne!(second.workspace.id, first.workspace.id);
    assert_eq!(
        listed_at(&client, &notes)
            .await
            .iter()
            .map(|workspace| workspace.id.clone())
            .collect::<Vec<_>>(),
        [second.workspace.id.clone(), first.workspace.id.clone()],
        "the directory's entry stands while a Session still works in it"
    );

    client
        .delete_session(first.id)
        .await
        .expect("delete the Session the directory's entry was kept for");
    let third = client
        .create_session(quiet_request(&notes, "Sort the notes"))
        .await
        .expect("create another Session in the Repository")
        .session;
    let carried = (
        third.workspace.id.clone(),
        notes.clone(),
        Some("dev-rust".to_owned()),
        set("Where the notes are kept."),
    );
    assert_eq!(
        named(&third.workspace),
        carried,
        "the Session begun now wears what the directory's entry carried"
    );
    assert_eq!(
        listed_at(&client, &notes)
            .await
            .iter()
            .map(named)
            .collect::<Vec<_>>(),
        std::slice::from_ref(&carried),
        "the directory is listed once, as the Repository's"
    );
    assert_eq!(
        named(
            &client
                .read_session(second.id)
                .await
                .expect("read the Session begun before")
                .session
                .workspace
        ),
        carried,
        "a Session already working in the Repository wears it too"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

/// A Description set by naming a directory whose own Workspace has become a
/// Repository's lands on the Repository's Workspace and drops the directory's
/// entry no Session works in, its Icon passing over where the Repository's
/// has none.
#[tokio::test]
async fn describing_a_directory_that_became_a_repository_supersedes_the_directorys_entry() {
    const CHANNEL: &str = "workspace-registry-superseded-at-description";
    let home = tempfile::tempdir().expect("create a home for the Workspaces");
    let home = canonical(home.path());
    let notes = home.join("notes");
    std::fs::create_dir(&notes).expect("create the notes directory");
    let config = ServerConfig::new(home.join("state"), CHANNEL).expect("configure");
    let server = quiet_server(config).await;
    let client = connected_client(&home.join("state"), CHANNEL).await;
    let directory = landing_workspace(&client, &notes).await;
    client
        .set_workspace_description(&directory.id, Some(&notes), "Where the notes wait.")
        .await
        .expect("describe the notes directory before any Session begins there");
    client
        .set_workspace_icon(&directory.id, "dev-rust")
        .await
        .expect("choose the notes directory's Icon");
    repository(&notes);

    let repository_workspace = landing_workspace(&client, &notes).await;
    assert_ne!(repository_workspace.id, directory.id);
    client
        .set_workspace_description(
            &repository_workspace.id,
            Some(&notes),
            "Where the notes are kept.",
        )
        .await
        .expect("describe the Repository the directory became");

    assert_eq!(
        listed_at(&client, &notes)
            .await
            .iter()
            .map(named)
            .collect::<Vec<_>>(),
        [(
            repository_workspace.id,
            notes.clone(),
            Some("dev-rust".to_owned()),
            set("Where the notes are kept.")
        )],
        "the directory is listed once, as the Repository's, keeping the Icon it was given"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

/// A Session stored before the workspaces table was the Registry may work in
/// a Workspace no Icon or Description ever landed for, so with no row, or one
/// whose row predates rows keeping a path. The migration enters each as its
/// Sessions present it, known by a Session, so it is listed at the first
/// start, before any Session's directory is resolved again — even where the
/// only Session there cannot be read, and so is never resolved again at all.
#[tokio::test]
async fn a_workspace_a_stored_session_works_in_is_listed_from_the_first_start_with_the_registry() {
    const CHANNEL: &str = "workspace-registry-seeding";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let notes = tempfile::tempdir().expect("create the notes Workspace");
    let atlas = tempfile::tempdir().expect("create the atlas Workspace");
    let config = ServerConfig::new(state_dir.path(), CHANNEL).expect("configure");
    let server = spawn_with_failing_provider(config.clone())
        .await
        .expect("spawn server");
    let client = connected_client(state_dir.path(), CHANNEL).await;
    let noted = client
        .create_session(quiet_request(notes.path(), "Keep the notes"))
        .await
        .expect("create a Session in the notes Workspace")
        .session;
    let charted = client
        .create_session(quiet_request(atlas.path(), "Chart the atlas"))
        .await
        .expect("create a Session in the atlas Workspace")
        .session;
    client
        .set_workspace_description(&charted.workspace.id, None, "Where the atlas is charted.")
        .await
        .expect("describe the atlas Workspace");
    drop(client);
    server.shutdown().await.expect("shut down server");
    unreadable(&config, noted.id);
    age_to_before_the_registry(
        &config,
        &format!(
            "DELETE FROM workspaces WHERE id = '{}';
             UPDATE workspaces SET path = NULL WHERE id = '{}';",
            noted.workspace.id.0, charted.workspace.id.0
        ),
    );

    let migrated = spawn_with_failing_provider(config.clone())
        .await
        .expect("spawn server over the aged database");
    let client = connected_client(state_dir.path(), CHANNEL).await;
    let listing = listed(&client).await;
    let described = Some(WorkspaceDescription {
        text: "Where the atlas is charted.".to_owned(),
        set: true,
    });
    assert_eq!(
        listing.iter().map(named).collect::<Vec<_>>(),
        [
            (
                charted.workspace.id.clone(),
                charted.workspace.path.clone(),
                None,
                described
            ),
            (
                noted.workspace.id.clone(),
                noted.workspace.path.clone(),
                None,
                None
            ),
        ],
        "both are listed at once, each where its Sessions present it"
    );
    assert!(
        client
            .list_sessions(None)
            .await
            .expect("list Sessions")
            .iter()
            .any(|item| matches!(item, SessionListItem::Unreadable(_))
                && item.workspace() == Some(&noted.workspace)),
        "the unreadable Session still groups under its own copy of its Workspace"
    );
    drop(client);
    migrated.shutdown().await.expect("shut down server");

    let entries = entries_on_disk(&config);
    assert_eq!(
        entries.get(&noted.workspace.id),
        Some(&(Some(noted.workspace.path), BY_SESSION.to_owned())),
        "a Workspace a Session works in without a row is entered, known by that Session"
    );
    assert_eq!(
        entries.get(&charted.workspace.id),
        Some(&(Some(charted.workspace.path), BY_SESSION.to_owned())),
        "a row without a path is presented where its Session presents it"
    );
}

/// The migration says how each row it found came to be known: by a Session
/// where one is grouped under its Workspace, and by its Description
/// otherwise. Each keeps what it carries, and is listed as before.
#[tokio::test]
async fn a_row_stored_before_the_registry_is_known_by_a_session_or_by_its_description() {
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
    age_to_before_the_registry(&config, "");

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

/// A Workspace presented by its repository metadata location, its main
/// checkout not known, is named so wherever it is listed, as the Sessions
/// working there name it — even where its only Session cannot be read, so
/// its Repository is never read again: its entry says how its path presents
/// it, from the Session that entered it and from the migration alike.
#[tokio::test]
async fn an_entry_whose_main_checkout_is_unknown_is_named_so_without_its_repository_being_read() {
    const CHANNEL: &str = "workspace-registry-presentation";
    let home = tempfile::tempdir().expect("create a home for the Repository");
    let home = canonical(home.path());
    let main = home.join("main checkout");
    let metadata = home.join("atlas.git");
    git(
        &home,
        &[
            "init",
            "-b",
            "main",
            "--separate-git-dir",
            metadata.to_str().expect("the fixture's path is UTF-8"),
            main.to_str().expect("the fixture's path is UTF-8"),
        ],
    );
    git(
        &main,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ],
    );
    let linked = home.join("linked");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-b",
            "topic",
            linked.to_str().expect("the fixture's path is UTF-8"),
        ],
    );
    let config = ServerConfig::new(home.join("state"), CHANNEL).expect("configure");
    let server = quiet_server(config.clone()).await;
    let client = connected_client(&home.join("state"), CHANNEL).await;
    let created = client
        .create_session(quiet_request(&linked, "Chart the atlas"))
        .await
        .expect("create a Session in the linked Worktree")
        .session;
    assert!(
        created.workspace.main_unknown(),
        "the Repository's main checkout is not known"
    );
    let paths = client
        .list_workspaces()
        .await
        .expect("list Workspaces")
        .workspace_paths;
    let name = paths.workspace_name(&created.workspace);
    assert_eq!(name, "atlas.git (main checkout unknown)");
    drop(client);
    server.shutdown().await.expect("shut down server");
    unreadable(&config, created.id);

    for (restart, age) in [("the next start", false), ("the migration", true)] {
        if age {
            age_to_before_the_registry(&config, "");
        }
        let restarted = quiet_server(config.clone()).await;
        restarted.workspace_discovery_settled().await;
        let client = connected_client(&home.join("state"), CHANNEL).await;
        let listing = client.list_workspaces().await.expect("list Workspaces");
        let listed = listing
            .workspaces
            .iter()
            .find(|workspace| workspace.id == created.workspace.id)
            .unwrap_or_else(|| panic!("the Workspace is listed after {restart}"));
        assert_eq!(
            listing.workspace_paths.workspace_name(listed),
            name,
            "after {restart}, the Workspace is named as its Session names it"
        );
        drop(client);
        restarted.shutdown().await.expect("shut down server");
    }
}

/// The Workspace listings a Server pushes on a Session catalog stream
/// opened now: the one its connect snapshot carries, then each it pushes
/// as what it lists moves (ADR 0052).
async fn pushed_listings(
    descriptor: &RuntimeDescriptor,
) -> impl futures_util::Stream<Item = WorkspaceListing> + Unpin + use<> {
    crate::server_support::open_catalog_events(descriptor, WORKSPACE_LISTING_EVENT).await
}

/// The next Workspace listing `pushed` carries.
async fn next_pushed(
    pushed: &mut (impl futures_util::Stream<Item = WorkspaceListing> + Unpin),
) -> WorkspaceListing {
    use futures_util::StreamExt;

    tokio::time::timeout(crate::server_support::PROGRESS_DEADLINE, pushed.next())
        .await
        .expect("a Workspace listing is pushed")
        .expect("the catalog stream stays open")
}

/// The Session catalog stream carries the Server's Workspace listing as it
/// connects, as a request for it answers, and then pushes it again as what
/// it lists moves: a Workspace a Session made known, and a Description set.
/// A move that leaves the listing as it was — another Session made in a
/// Workspace already listed — pushes nothing.
#[tokio::test]
async fn the_catalog_stream_carries_the_listing_and_pushes_each_change_of_it_once() {
    const CHANNEL: &str = "workspace-registry-pushed";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let notes = tempfile::tempdir().expect("create the notes Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), CHANNEL).expect("configure"),
    )
    .await
    .expect("spawn server");
    let client = connected_client(state_dir.path(), CHANNEL).await;
    let mut pushed = pushed_listings(server.descriptor()).await;

    assert_eq!(
        next_pushed(&mut pushed).await,
        client.list_workspaces().await.expect("list Workspaces"),
        "the stream's snapshot carries the listing a request answers"
    );

    let workspace = client
        .create_session(quiet_request(notes.path(), "Keep the notes"))
        .await
        .expect("create a Session in the notes Workspace")
        .session
        .workspace;
    assert_eq!(
        next_pushed(&mut pushed).await.workspaces,
        std::slice::from_ref(&workspace),
        "the Workspace a Session made known is pushed"
    );

    client
        .create_session(quiet_request(notes.path(), "Keep more notes"))
        .await
        .expect("create another Session in the notes Workspace");
    client
        .set_workspace_description(
            &workspace.id,
            Some(&workspace.path),
            "Where notes are kept.",
        )
        .await
        .expect("describe the notes Workspace");
    assert_eq!(
        next_pushed(&mut pushed)
            .await
            .workspaces
            .into_iter()
            .map(|workspace| workspace.description)
            .collect::<Vec<_>>(),
        [set("Where notes are kept.")],
        "the next push is the Description's, the Session made between them having left the \
         listing as it was"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

/// A listing answers with what was last found of each entry's directory,
/// and has it read behind the answer: what the reading finds is pushed once
/// it lands. Here the directory of a Workspace a Session belongs to is
/// deleted, and the listing answers it available, not yet read since; the
/// push says it is gone.
#[tokio::test]
async fn what_a_reading_behind_a_listing_finds_is_pushed_once_it_lands() {
    const CHANNEL: &str = "workspace-registry-reading-pushed";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let notes = tempfile::tempdir().expect("create the notes Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), CHANNEL).expect("configure"),
    )
    .await
    .expect("spawn server");
    server.workspace_discovery_settled().await;
    let client = connected_client(state_dir.path(), CHANNEL).await;
    client
        .create_session(quiet_request(notes.path(), "Keep the notes"))
        .await
        .expect("create a Session in the notes Workspace");
    let mut pushed = pushed_listings(server.descriptor()).await;
    next_pushed(&mut pushed).await;

    notes
        .close()
        .expect("delete the notes Workspace's directory");
    let answered = client.list_workspaces().await.expect("list Workspaces");

    assert_eq!(
        answered
            .workspaces
            .iter()
            .map(|workspace| workspace.unavailable)
            .collect::<Vec<_>>(),
        [None],
        "the listing answers before the directory is read, as it was last found"
    );
    let read = next_pushed(&mut pushed).await;
    assert_eq!(
        read.workspaces
            .iter()
            .map(|workspace| workspace.unavailable)
            .collect::<Vec<_>>(),
        [Some(WorkspaceUnavailability::Gone)],
        "what the reading behind it found is pushed once it lands"
    );
    assert!(
        read.revision.is_no_older_than(&answered.revision)
            && !answered.revision.is_no_older_than(&read.revision),
        "at a revision newer than the answer's, so a client hearing both in either order keeps \
         it: {:?} then {:?}",
        answered.revision,
        read.revision
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

/// Listings given while nothing they list moves stand at one revision; a
/// Server started again gives its listings in a run of its own, so no
/// count of the run before is read against them.
#[tokio::test]
async fn a_server_started_again_gives_its_listings_in_a_run_of_its_own() {
    const CHANNEL: &str = "workspace-registry-runs";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config = ServerConfig::new(state_dir.path(), CHANNEL).expect("configure");
    let revision = async || {
        let server = spawn_with_failing_provider(config.clone())
            .await
            .expect("spawn server");
        server.workspace_discovery_settled().await;
        let client = connected_client(state_dir.path(), CHANNEL).await;
        let first = client.list_workspaces().await.expect("list Workspaces");
        let again = client.list_workspaces().await.expect("list Workspaces");
        assert_eq!(
            first.revision, again.revision,
            "nothing listed moved between the two"
        );
        drop(client);
        server.shutdown().await.expect("shut down server");
        first.revision
    };

    let before = revision().await;
    let after = revision().await;

    assert_ne!(before.run, after.run);
}

/// A client hears its Server's Workspace listing as the Server pushes it,
/// once it hears it is connected, and again as it moves.
#[tokio::test]
async fn a_client_hears_each_workspace_listing_its_server_pushes() {
    const CHANNEL: &str = "workspace-registry-client-keeps";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let notes = tempfile::tempdir().expect("create the notes Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), CHANNEL).expect("configure"),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), CHANNEL).expect("configure client"),
    )
    .await
    .expect("connect client");
    assert!(matches!(
        client.next().await,
        Some(ManagedEvent::Connecting)
    ));
    assert!(
        matches!(client.next().await, Some(ManagedEvent::Connected(_))),
        "the client hears it is connected before any Workspace listing"
    );
    let mut next_heard = async || {
        crate::server_support::next_workspace_listing(async || client.next().await)
            .await
            .workspaces
    };
    assert_eq!(next_heard().await, [], "the listing as the client connects");
    let workspace = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), CHANNEL).expect("configure another client"),
    )
    .await
    .expect("connect another client")
    .create_session(quiet_request(notes.path(), "Keep the notes"))
    .await
    .expect("create a Session in the notes Workspace")
    .session
    .workspace;
    assert_eq!(
        next_heard().await,
        [workspace],
        "and the listing a Session moved"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

/// Waits until the Session `session_id` has stopped working, which a
/// Session whose Provider fails it does at once.
async fn settled(client: &ManagedClient, session_id: SessionId) {
    tokio::time::timeout(crate::server_support::PROGRESS_DEADLINE, async {
        while client
            .read_session(session_id)
            .await
            .expect("read the Session")
            .session
            .working_since
            .is_some()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the Session stops working");
}

/// Prompts the Session `session_id` again, and waits until it has stopped
/// working.
async fn prompt_again(client: &ManagedClient, session_id: SessionId) {
    client
        .admit_prompt(
            session_id,
            suru::protocol::AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Once more".to_owned(),
                    skill_invocations: Vec::new(),
                    attachments: Vec::new(),
                },
                delivery: suru::protocol::PromptDelivery::Queue,
            },
        )
        .await
        .expect("prompt the Session again");
    settled(client, session_id).await;
}

/// The listing stands the Workspaces Sessions work in by which held work
/// most recently, so work in one that stood behind another moves it ahead,
/// and the listing is pushed in its new order — while work that leaves the
/// order as it stood pushes nothing: the next push here is a Description's.
#[tokio::test]
async fn work_that_moves_a_workspace_ahead_pushes_the_listing_in_its_new_order() {
    const CHANNEL: &str = "workspace-registry-reordered";
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let notes = tempfile::tempdir().expect("create the notes Workspace");
    let drafts = tempfile::tempdir().expect("create the drafts Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), CHANNEL).expect("configure"),
    )
    .await
    .expect("spawn server");
    let client = connected_client(state_dir.path(), CHANNEL).await;
    let mut sessions = Vec::new();
    for directory in [notes.path(), drafts.path()] {
        let session = client
            .create_session(quiet_request(directory, "Begin"))
            .await
            .expect("create a Session")
            .session;
        settled(&client, session.id).await;
        sessions.push(session);
    }
    let mut pushed = pushed_listings(server.descriptor()).await;
    let order = |listing: &WorkspaceListing| {
        listing
            .workspaces
            .iter()
            .map(|workspace| workspace.id.clone())
            .collect::<Vec<_>>()
    };
    let (noted, drafted) = (&sessions[0], &sessions[1]);
    assert_eq!(
        order(&next_pushed(&mut pushed).await),
        [drafted.workspace.id.clone(), noted.workspace.id.clone()],
        "the Workspace worked in last stands first"
    );

    prompt_again(&client, noted.id).await;
    assert_eq!(
        order(&next_pushed(&mut pushed).await),
        [noted.workspace.id.clone(), drafted.workspace.id.clone()],
        "work in the Workspace behind moves it ahead, and is pushed"
    );

    prompt_again(&client, noted.id).await;
    client
        .set_workspace_description(
            &drafted.workspace.id,
            Some(&drafted.workspace.path),
            "Where drafts wait.",
        )
        .await
        .expect("describe the drafts Workspace");
    let described = next_pushed(&mut pushed).await;
    assert_eq!(
        (
            order(&described),
            described.workspaces[1].description.clone()
        ),
        (
            vec![noted.workspace.id.clone(), drafted.workspace.id.clone()],
            set("Where drafts wait.")
        ),
        "work leaving the order as it stood pushed nothing before the Description"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}
