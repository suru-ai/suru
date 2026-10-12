//! Discovery: every Repository root directly within a Workspace Root — main
//! or bare — is entered in the Workspace Registry, known by discovery under
//! that Root, so it is listed before any Session has begun there (ADR 0051).
//! Plain directories, hidden ones, anything deeper, a linked Worktree of a
//! Repository elsewhere, the Sidekick Workspace and the Root itself are left
//! alone. Discovery runs at start, when the Roots change, and whenever
//! Workspaces are listed, reusing a scan only as old as the Server's memo
//! allows, which these Servers shorten to nothing or lengthen past any test.
//!
//! Each test drives the Server as a Client does, with real Git Repositories
//! in a temporary Root, and asserts on what the Client's Workspace listing
//! answers, or on what the Registry left on disk.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use suru::{
    protocol::{RepositoryId, SettingMutation, Workspace, WorkspaceId},
    server::{self, ServerConfig, ServerTimings},
};

use crate::{
    failing_provider_support::FailingProviderRuntime,
    repositories::git,
    workspace_registry::{
        BY_SESSION, canonical, connected_client, entries_on_disk, listed, quiet_request, repository,
    },
};

/// A memo no test outlasts: every listing after the first reuses its scan.
const NEVER_AGAIN: Duration = Duration::from_secs(60 * 60);

/// The Config Document a Server reads its Workspace Roots from, naming
/// `roots`, written into `config_dir`.
fn name_roots(config_dir: &Path, roots: &[&str]) {
    std::fs::create_dir_all(config_dir).expect("create the config root");
    let document = serde_json::json!({ "workspace": { "roots": roots } });
    std::fs::write(config_dir.join("suru.jsonc"), document.to_string())
        .expect("write the Config Document");
}

/// A path as a Workspace Root names it.
fn spelled(path: &Path) -> &str {
    path.to_str().expect("the fixture's path is UTF-8")
}

/// A Server beneath `home` whose Workspace Roots are `roots`, reusing a scan
/// for `memo`, and which leaves the Worktrees it knows alone, so no Git it
/// runs keeps a Repository's directory open when a test lets go of it, which
/// Windows refuses to delete.
async fn server_with_roots(
    home: &Path,
    channel: &str,
    roots: &[&str],
    memo: Duration,
) -> (ServerConfig, server::RunningServer) {
    name_roots(&home.join("config"), roots);
    let config = ServerConfig::new(home.join("state"), channel)
        .expect("configure")
        .with_config_dir(home.join("config"))
        .with_home_dir(home);
    let server = server::spawn_with_provider_and_timings(
        config.clone(),
        Arc::new(FailingProviderRuntime),
        ServerTimings::default()
            .with_checkout_observation_interval(Duration::from_secs(60 * 60))
            .with_root_discovery_memo(memo),
    )
    .await
    .expect("spawn server");
    (config, server)
}

/// The identity of the Repository whose shared metadata is `metadata`.
fn repository_id(metadata: &Path) -> WorkspaceId {
    RepositoryId::from_metadata("git", metadata).workspace_id()
}

/// What a listing says of a Workspace a reader tells apart: its identity and
/// where it is presented.
fn located(workspace: &Workspace) -> (WorkspaceId, PathBuf) {
    (workspace.id.clone(), workspace.path.clone())
}

/// A Repository directly within a Workspace Root is listed by its main root
/// before any Session has begun there, carrying no Icon and no Description.
#[tokio::test]
async fn a_repository_directly_within_a_root_is_listed_before_any_session_begins_there() {
    const CHANNEL: &str = "workspace-discovery-listed";
    let home = tempfile::tempdir().expect("create a home");
    let home = canonical(home.path());
    let projects = home.join("projects");
    let atlas = projects.join("atlas");
    repository(&atlas);
    let (_, server) =
        server_with_roots(&home, CHANNEL, &[spelled(&projects)], Duration::ZERO).await;
    let client = connected_client(&home.join("state"), CHANNEL).await;

    let listing = listed(&client).await;
    assert_eq!(
        listing.iter().map(located).collect::<Vec<_>>(),
        [(repository_id(&atlas.join(".git")), atlas.clone())],
        "the Repository is listed by its main root"
    );
    assert_eq!(
        (&listing[0].icon, &listing[0].description),
        (&None, &None),
        "discovery derives no Icon and no Description"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

/// Of a Root's contents, only the Repository roots directly within it are
/// discovered, main or bare: a plain directory, a Repository two levels
/// down, a hidden Repository, and a linked Worktree of a Repository
/// presented elsewhere are each left alone, and so is the Root itself, which
/// is a Repository here, so every plain directory within it is the Root's.
#[tokio::test]
async fn only_the_repository_roots_directly_within_a_root_are_discovered_main_or_bare() {
    const CHANNEL: &str = "workspace-discovery-shapes";
    let home = tempfile::tempdir().expect("create a home");
    let home = canonical(home.path());
    let projects = home.join("projects");
    repository(&projects);
    let atlas = projects.join("atlas");
    repository(&atlas);
    let ledger = projects.join("ledger.git");
    git(
        &projects,
        &["init", "--bare", "-b", "main", spelled(&ledger)],
    );
    std::fs::create_dir(projects.join("notes")).expect("create a plain directory");
    repository(&projects.join("vendor").join("inner"));
    repository(&projects.join(".tooling"));
    std::fs::write(projects.join("README.md"), "Projects").expect("write a file");
    let elsewhere = home.join("elsewhere");
    repository(&elsewhere);
    git(
        &elsewhere,
        &[
            "worktree",
            "add",
            "-b",
            "topic",
            spelled(&projects.join("topic")),
        ],
    );
    let (_, server) =
        server_with_roots(&home, CHANNEL, &[spelled(&projects)], Duration::ZERO).await;
    let client = connected_client(&home.join("state"), CHANNEL).await;

    assert_eq!(
        listed(&client)
            .await
            .iter()
            .map(located)
            .collect::<Vec<_>>(),
        [
            (repository_id(&atlas.join(".git")), atlas.clone()),
            (repository_id(&ledger), ledger.clone()),
        ],
        "the main and the bare Repository directly within the Root are listed, and nothing \
         else within it, nor the Root"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

/// A listing reuses a scan of the same Roots that is only moments old, so a
/// Repository made since is not listed yet; a change of the Roots discovers
/// what they hold there and then, before any listing asks.
#[tokio::test]
async fn a_scan_moments_old_is_reused_until_the_roots_change() {
    const CHANNEL: &str = "workspace-discovery-memo";
    let home = tempfile::tempdir().expect("create a home");
    let home = canonical(home.path());
    let projects = home.join("projects");
    let (_, server) = server_with_roots(&home, CHANNEL, &[], NEVER_AGAIN).await;
    let client = connected_client(&home.join("state"), CHANNEL).await;
    assert_eq!(listed(&client).await, [], "no Root names anything yet");

    let atlas = projects.join("atlas");
    repository(&atlas);
    client
        .mutate_setting(SettingMutation::WorkspaceRoots {
            value: Some(vec![spelled(&projects).to_owned()]),
        })
        .await
        .expect("name the Workspace Root");
    let ledger = projects.join("ledger");
    repository(&ledger);

    assert_eq!(
        listed(&client)
            .await
            .iter()
            .map(located)
            .collect::<Vec<_>>(),
        [(repository_id(&atlas.join(".git")), atlas.clone())],
        "the Root was discovered as it was named, and that scan is reused, so the Repository \
         made since is not listed yet"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

/// Discovery at start enters each Repository root within a Root in the
/// Registry before anything is listed, known by discovery under that Root
/// as the Setting spells it. A Workspace a Session began in before it was
/// within a Root stays known by that Session.
#[tokio::test]
async fn discovery_at_start_enters_each_repository_known_by_the_root_it_was_found_under() {
    const CHANNEL: &str = "workspace-discovery-provenance";
    let home = tempfile::tempdir().expect("create a home");
    let home = canonical(home.path());
    let projects = home.join("projects");
    let atlas = projects.join("atlas");
    repository(&atlas);
    let ledger = projects.join("ledger");
    repository(&ledger);
    let (config, server) = server_with_roots(&home, CHANNEL, &[], Duration::ZERO).await;
    let client = connected_client(&home.join("state"), CHANNEL).await;
    client
        .create_session(quiet_request(&ledger, "Balance the ledger"))
        .await
        .expect("begin a Session in the ledger before it is within a Root");
    drop(client);
    server.shutdown().await.expect("shut down server");

    let (_, restarted) = server_with_roots(&home, CHANNEL, &["~/projects"], NEVER_AGAIN).await;
    restarted.workspace_discovery_settled().await;
    restarted.shutdown().await.expect("shut down server");

    let entries = entries_on_disk(&config);
    assert_eq!(
        entries.get(&repository_id(&atlas.join(".git"))),
        Some(&(
            Some(atlas.clone()),
            r#"{"discovered":{"root":"~/projects"}}"#.to_owned()
        )),
        "the Repository is entered at start, known by the Root it was found under"
    );
    assert_eq!(
        entries.get(&repository_id(&ledger.join(".git"))),
        Some(&(Some(ledger.clone()), BY_SESSION.to_owned())),
        "a Workspace a Session began in stays known by that Session"
    );
}

/// A Root that cannot be read is skipped — nothing is made where it names —
/// and the Roots that can be read are discovered all the same, one beginning
/// with `~` beneath the Server's home.
#[tokio::test]
async fn a_root_that_cannot_be_read_is_skipped_and_never_made() {
    const CHANNEL: &str = "workspace-discovery-unreadable-root";
    let home = tempfile::tempdir().expect("create a home");
    let home = canonical(home.path());
    let unplugged = home.join("unplugged").join("projects");
    let atlas = home.join("projects").join("atlas");
    repository(&atlas);
    let (_, server) = server_with_roots(
        &home,
        CHANNEL,
        &[spelled(&unplugged), "~/projects"],
        Duration::ZERO,
    )
    .await;
    let client = connected_client(&home.join("state"), CHANNEL).await;

    assert_eq!(
        listed(&client)
            .await
            .iter()
            .map(located)
            .collect::<Vec<_>>(),
        [(repository_id(&atlas.join(".git")), atlas.clone())],
        "the Root that can be read is discovered"
    );
    assert!(
        !home.join("unplugged").exists(),
        "nothing is made where the Root that cannot be read names"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

/// A Repository cloned by hand into a Root while the Server runs is listed
/// the next time Workspaces are listed, without a restart.
#[tokio::test]
async fn a_repository_cloned_by_hand_after_start_is_listed_at_the_next_listing() {
    const CHANNEL: &str = "workspace-discovery-cloned-by-hand";
    let home = tempfile::tempdir().expect("create a home");
    let home = canonical(home.path());
    let projects = home.join("projects");
    let atlas = projects.join("atlas");
    repository(&atlas);
    let (_, server) =
        server_with_roots(&home, CHANNEL, &[spelled(&projects)], Duration::ZERO).await;
    let client = connected_client(&home.join("state"), CHANNEL).await;
    assert_eq!(
        listed(&client)
            .await
            .iter()
            .map(located)
            .collect::<Vec<_>>(),
        [(repository_id(&atlas.join(".git")), atlas.clone())]
    );

    let copy = projects.join("atlas-copy");
    git(
        &projects,
        &["clone", "--quiet", spelled(&atlas), spelled(&copy)],
    );

    assert_eq!(
        listed(&client)
            .await
            .iter()
            .map(located)
            .collect::<Vec<_>>(),
        [
            (repository_id(&atlas.join(".git")), atlas.clone()),
            (repository_id(&copy.join(".git")), copy.clone()),
        ],
        "the clone is listed beside the Repository it was cloned from, as a Workspace of its own"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

/// The Sidekick Workspace is never discovered, even made a Repository
/// directly within a Root: it is listed as itself and nothing else is
/// presented at its directory.
#[tokio::test]
async fn the_sidekick_workspace_is_never_discovered() {
    const CHANNEL: &str = "workspace-discovery-sidekick";
    let home = tempfile::tempdir().expect("create a home");
    let home = canonical(home.path());
    let (config, server) = server_with_roots(&home, CHANNEL, &[], Duration::ZERO).await;
    let client = connected_client(&home.join("state"), CHANNEL).await;
    let sidekick = client
        .sidekick_workspace()
        .await
        .expect("make the Sidekick Workspace")
        .workspace;
    repository(&sidekick.path);
    client
        .mutate_setting(SettingMutation::WorkspaceRoots {
            value: Some(vec![spelled(&canonical(config.data_dir())).to_owned()]),
        })
        .await
        .expect("name the directory the Sidekick Workspace is in as a Root");

    let at_sidekick = listed(&client)
        .await
        .into_iter()
        .filter(|workspace| workspace.path == sidekick.path)
        .map(|workspace| workspace.id)
        .collect::<Vec<_>>();
    assert!(
        at_sidekick.iter().all(|id| *id == sidekick.id),
        "nothing but the Sidekick Workspace is presented at its directory: {at_sidekick:?}"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}
