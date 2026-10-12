//! Discovery: every Repository root directly within a Workspace Root — main
//! or bare — is entered in the Workspace Registry, known by discovery under
//! that Root, so it is listed before any Session has begun there (ADR 0051);
//! a linked Worktree there counts for its Repository's Workspace, wherever
//! that is presented. Plain directories, hidden ones, anything deeper, the
//! Sidekick Workspace, the Root itself and any Root within it are left
//! alone, and so is everything within Suru's own data directory named as a
//! Root. Discovery runs at start, when the Roots change, and whenever
//! Workspaces are listed, reusing a scan only as old as the Server's memo
//! allows, which these Servers shorten to nothing or lengthen past any test.
//!
//! Each test drives the Server as a Client does, with real Git Repositories
//! in a temporary Root, and asserts on what the Client's Workspace listing
//! answers, or on what the Registry left on disk.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use serde_json::json;
use suru::{
    protocol::{
        ProviderId, RepositoryId, ResolvedWorkspace, SettingMutation, Workspace,
        WorkspaceDescription, WorkspaceId,
    },
    server::{self, ServerConfig, ServerTimings},
    source_control::{GitSourceControl, SourceControl},
};
use tokio::time::timeout;

use crate::{
    failing_provider_support::FailingProviderRuntime,
    provider_support::ControlledProvider,
    repositories::git,
    server_support::{
        PROGRESS_DEADLINE, next_derived_title, next_workspace_description_changed,
        next_workspace_icon_changed,
    },
    support::{hosted_model, hosted_selection},
    workspace_registry::{
        BY_SESSION, canonical, connected_client, entries_on_disk, landing_workspace, listed,
        quiet_request, repository, set,
    },
    workspace_standing::delete_once_settled,
};

/// A memo no test outlasts: every listing after the first reuses its scan.
const NEVER_AGAIN: Duration = Duration::from_secs(60 * 60);

/// A bound on waiting for discovery that no test reaches, so whatever asks
/// for a scan of the Roots answers with what it found, however slowly it
/// runs on a loaded machine.
pub(crate) const AS_LONG_AS_IT_TAKES: Duration = Duration::from_secs(60 * 60);

/// The Config Document a Server reads its Workspace Roots from, naming
/// `roots`, written into `config_dir`.
pub(super) fn name_roots(config_dir: &Path, roots: &[&str]) {
    std::fs::create_dir_all(config_dir).expect("create the config root");
    let document = json!({ "workspace": { "roots": roots } });
    std::fs::write(config_dir.join("suru.jsonc"), document.to_string())
        .expect("write the Config Document");
}

/// A path as a Workspace Root names it.
pub(super) fn spelled(path: &Path) -> &str {
    path.to_str().expect("the fixture's path is UTF-8")
}

/// A Server beneath `home` whose Workspace Roots are `roots`, reusing a scan
/// for `memo` and waiting on each as long as it takes, and which leaves the
/// Worktrees it knows alone, so no Git it runs keeps a Repository's
/// directory open when a test lets go of it, which Windows refuses to
/// delete.
pub(super) async fn server_with_roots(
    home: &Path,
    channel: &str,
    roots: &[&str],
    memo: Duration,
) -> (ServerConfig, server::RunningServer) {
    name_roots(&home.join("config"), roots);
    server_beneath(home, channel, memo).await
}

/// A Server beneath `home`, as [`server_with_roots`] makes one, reading
/// whatever Config Document its config root holds.
pub(super) async fn server_beneath(
    home: &Path,
    channel: &str,
    memo: Duration,
) -> (ServerConfig, server::RunningServer) {
    let config = ServerConfig::new(home.join("state"), channel)
        .expect("configure")
        .with_config_dir(home.join("config"))
        .with_home_dir(home);
    let server = server::spawn_with_provider_and_timings(
        config.clone(),
        Arc::new(FailingProviderRuntime),
        ServerTimings::default()
            .with_checkout_observation_interval(Duration::from_secs(60 * 60))
            .with_root_discovery_memo(memo)
            .with_root_discovery_wait(AS_LONG_AS_IT_TAKES),
    )
    .await
    .expect("spawn server");
    (config, server)
}

/// The identity of the Repository whose shared metadata is `metadata`.
pub(super) fn repository_id(metadata: &Path) -> WorkspaceId {
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
/// discovered, main or bare, and a linked Worktree directly within it counts
/// for its Repository's Workspace, presented by that Repository's main root
/// outside the Root: a plain directory, a Repository two levels down, a
/// hidden Repository, and — where links are made — a link to a Repository
/// elsewhere are each left alone, and so is the Root itself, which is a
/// Repository here, so every plain directory within it, and a link to it,
/// is the Root's.
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
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(".", projects.join("self")).expect("link the Root to itself");
        std::os::unix::fs::symlink(&elsewhere, projects.join("alias"))
            .expect("link a Repository elsewhere into the Root");
    }
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
            (repository_id(&elsewhere.join(".git")), elsewhere.clone()),
            (repository_id(&atlas.join(".git")), atlas.clone()),
            (repository_id(&ledger), ledger.clone()),
        ],
        "the main and the bare Repository directly within the Root are listed, and the \
         Repository the linked Worktree within it belongs to, by its main root; nothing else \
         within it, nor the Root"
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

/// Suru's own data directory named as a Workspace Root is ignored, so the
/// Sidekick Workspace, kept directly within it, is never within a Root:
/// neither it, though made a Repository, nor a Repository put directly
/// beside it is discovered, and nothing but the Sidekick Workspace is
/// presented within the data directory. The Root named beside it is
/// discovered all the same.
#[tokio::test]
async fn a_root_naming_suru_s_own_data_directory_is_ignored() {
    const CHANNEL: &str = "workspace-discovery-data-directory";
    let home = tempfile::tempdir().expect("create a home");
    let home = canonical(home.path());
    let projects = home.join("projects");
    let ledger = projects.join("ledger");
    repository(&ledger);
    let (config, server) = server_with_roots(&home, CHANNEL, &[], Duration::ZERO).await;
    let client = connected_client(&home.join("state"), CHANNEL).await;
    let sidekick = client
        .sidekick_workspace()
        .await
        .expect("make the Sidekick Workspace")
        .workspace;
    repository(&sidekick.path);
    let data = canonical(config.data_dir());
    let atlas = data.join("atlas");
    repository(&atlas);
    client
        .mutate_setting(SettingMutation::WorkspaceRoots {
            value: Some(vec![
                spelled(&data).to_owned(),
                spelled(&projects).to_owned(),
            ]),
        })
        .await
        .expect("name the data directory as a Root beside another");

    let listing = listed(&client).await;
    assert!(
        listing
            .iter()
            .filter(|workspace| workspace.path.starts_with(&data))
            .all(|workspace| workspace.id == sidekick.id),
        "nothing but the Sidekick Workspace is presented within the data directory: {listing:?}"
    );
    assert!(
        listing
            .iter()
            .any(|workspace| located(workspace)
                == (repository_id(&ledger.join(".git")), ledger.clone())),
        "the Root named beside it is discovered: {listing:?}"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

/// A Root standing directly within another is a Root and never a
/// Workspace, however it is spelled: the other Root's scan enters it as
/// nothing, though it is a Repository, nor counts a linked Worktree of it
/// beside it for it, while the Repositories within it are discovered as
/// its own. One entered before it was named a Root is dropped once it is —
/// unless a Session belongs to it, which keeps it until none does.
#[tokio::test]
async fn a_root_within_another_root_is_a_root_and_never_a_workspace() {
    const CHANNEL: &str = "workspace-discovery-nested-roots";
    let home = tempfile::tempdir().expect("create a home");
    let home = canonical(home.path());
    let projects = home.join("projects");
    let atlas = projects.join("atlas");
    repository(&atlas);
    let maps = atlas.join("maps");
    repository(&maps);
    git(
        &atlas,
        &[
            "worktree",
            "add",
            "-b",
            "topic",
            spelled(&projects.join("topic")),
        ],
    );
    let ledger = projects.join("ledger");
    repository(&ledger);
    let books = ledger.join("books");
    repository(&books);
    let (config, server) =
        server_with_roots(&home, CHANNEL, &[spelled(&projects)], Duration::ZERO).await;
    let client = connected_client(&home.join("state"), CHANNEL).await;
    let located_sorted = async || {
        let mut located = listed(&client)
            .await
            .iter()
            .map(located)
            .collect::<Vec<_>>();
        located.sort_by(|(_, left), (_, right)| left.cmp(right));
        located
    };
    let (atlas_id, ledger_id) = (
        repository_id(&atlas.join(".git")),
        repository_id(&ledger.join(".git")),
    );
    assert_eq!(
        located_sorted().await,
        [
            (atlas_id.clone(), atlas.clone()),
            (ledger_id.clone(), ledger.clone()),
        ],
        "each Repository directly within the Root is discovered while it is no Root itself"
    );
    let balanced = client
        .create_session(quiet_request(&ledger, "Balance the ledger"))
        .await
        .expect("begin a Session in a discovered Repository")
        .session;

    client
        .mutate_setting(SettingMutation::WorkspaceRoots {
            value: Some(vec![
                spelled(&projects).to_owned(),
                format!("~{0}projects{0}atlas{0}", std::path::MAIN_SEPARATOR),
                spelled(&ledger).to_owned(),
            ]),
        })
        .await
        .expect("name the Repositories within the Root as Roots of their own");

    let books_id = repository_id(&books.join(".git"));
    let maps_id = repository_id(&maps.join(".git"));
    assert_eq!(
        located_sorted().await,
        [
            (maps_id.clone(), maps.clone()),
            (ledger_id.clone(), ledger.clone()),
            (books_id.clone(), books.clone()),
        ],
        "a Root within another is no Workspace, but one a Session belongs to is kept, and the \
         Repositories within each are discovered as its own"
    );

    delete_once_settled(&client, balanced.id).await;
    assert_eq!(
        located_sorted().await,
        [(maps_id, maps.clone()), (books_id, books.clone())],
        "once no Session belongs to it, the Root's entry is dropped too"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
    let entries = entries_on_disk(&config);
    assert!(
        !entries.contains_key(&atlas_id) && !entries.contains_key(&ledger_id),
        "and dropped from disk: {entries:?}"
    );
}

/// A linked Worktree directly within a Root counts for its Repository's
/// Workspace, presented by its main root outside the Root and known by
/// discovery under the Root, and is entered as no Workspace of its own; one
/// of a Repository whose main root is unknown counts for nothing. One of a
/// Repository presented directly within another Root counts there alone, so
/// taking away the Root the linked Worktree is in leaves it, with what it
/// carries, while what was discovered only through that Root goes.
#[tokio::test]
async fn a_linked_worktree_within_a_root_counts_for_its_repositorys_workspace() {
    const CHANNEL: &str = "workspace-discovery-linked-worktree";
    let home = tempfile::tempdir().expect("create a home");
    let home = canonical(home.path());
    let projects = home.join("projects");
    std::fs::create_dir(&projects).expect("create the Workspace Root");
    let atlas = home.join("atlas");
    repository(&atlas);
    let topic = projects.join("topic");
    git(&atlas, &["worktree", "add", "-b", "topic", spelled(&topic)]);
    let away = home.join("away");
    git(
        &home,
        &[
            "init",
            "-b",
            "main",
            "--separate-git-dir",
            spelled(&home.join("away.git")),
            spelled(&away),
        ],
    );
    git(
        &away,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ],
    );
    let orphan = projects.join("orphan");
    git(
        &away,
        &["worktree", "add", "-b", "orphan", spelled(&orphan)],
    );
    let repos = home.join("repos");
    let ledger = repos.join("ledger");
    repository(&ledger);
    let books = projects.join("books");
    git(
        &ledger,
        &["worktree", "add", "-b", "books", spelled(&books)],
    );
    let (config, server) = server_with_roots(
        &home,
        CHANNEL,
        &[spelled(&projects), spelled(&repos)],
        Duration::ZERO,
    )
    .await;
    let client = connected_client(&home.join("state"), CHANNEL).await;

    let (atlas_id, ledger_id) = (
        repository_id(&atlas.join(".git")),
        repository_id(&ledger.join(".git")),
    );
    assert_eq!(
        listed(&client)
            .await
            .iter()
            .map(located)
            .collect::<Vec<_>>(),
        [
            (atlas_id.clone(), atlas.clone()),
            (ledger_id.clone(), ledger.clone()),
        ],
        "each Repository a linked Worktree within the Root belongs to is listed by its main \
         root, but the one whose main root is unknown"
    );
    client
        .set_workspace_description(&ledger_id, Some(&ledger), "Where the books balance.")
        .await
        .expect("describe the Repository within the other Root");

    client
        .mutate_setting(SettingMutation::WorkspaceRoots {
            value: Some(vec![spelled(&repos).to_owned()]),
        })
        .await
        .expect("take away the Root the linked Worktrees are in");
    assert_eq!(
        listed(&client)
            .await
            .iter()
            .map(|workspace| (located(workspace), workspace.description.clone()))
            .collect::<Vec<_>>(),
        [(
            (ledger_id.clone(), ledger.clone()),
            set("Where the books balance.")
        )],
        "what was discovered only through the Root taken away goes, and the Repository the \
         other Root holds stays with what it carries"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
    let entries = entries_on_disk(&config);
    let repos_spelled = spelled(&repos).replace('\\', "\\\\");
    assert_eq!(
        entries.get(&ledger_id),
        Some(&(
            Some(ledger.clone()),
            format!(r#"{{"discovered":{{"root":"{repos_spelled}"}}}}"#)
        )),
        "known by discovery under the Root that holds its main root"
    );
    assert!(
        !entries.contains_key(&atlas_id)
            && entries.values().all(|(path, _)| [&topic, &orphan, &books]
                .iter()
                .all(|linked| path.as_ref() != Some(*linked))),
        "and no entry is presented at any linked Worktree: {entries:?}"
    );
}

/// A directory described before it became a Repository is superseded there
/// once discovery finds the Repository, as wherever a Workspace is first
/// seen: the directory is listed once, as the Repository's, carrying the
/// Description it was given.
#[tokio::test]
async fn a_repository_discovered_where_a_described_directory_was_supersedes_its_entry() {
    const CHANNEL: &str = "workspace-discovery-supersedes";
    let home = tempfile::tempdir().expect("create a home");
    let home = canonical(home.path());
    let projects = home.join("projects");
    let notes = projects.join("notes");
    std::fs::create_dir_all(&notes).expect("create the notes directory");
    let (_, server) = server_with_roots(&home, CHANNEL, &[], Duration::ZERO).await;
    let client = connected_client(&home.join("state"), CHANNEL).await;
    let directory = landing_workspace(&client, &notes).await;
    client
        .set_workspace_description(&directory.id, Some(&notes), "Where the notes are kept.")
        .await
        .expect("describe the notes directory");
    repository(&notes);
    client
        .mutate_setting(SettingMutation::WorkspaceRoots {
            value: Some(vec![spelled(&projects).to_owned()]),
        })
        .await
        .expect("name the Workspace Root");

    assert_eq!(
        listed(&client)
            .await
            .iter()
            .map(|workspace| (located(workspace), workspace.description.clone()))
            .collect::<Vec<_>>(),
        [(
            (repository_id(&notes.join(".git")), notes.clone()),
            set("Where the notes are kept.")
        )],
        "the directory is listed once, as the Repository's, with the Description it was given"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

/// The Provider hosted for the Errand test, and its one Model.
const PROVIDER: &str = "controlled";
const MODEL: &str = "controlled-default";

/// Discovery derives nothing, so no Errand is asked on a discovered
/// Workspace's account; the first Session begun there asks for its Icon and
/// Description as a first Session anywhere does, and they land on its entry.
#[tokio::test]
async fn no_errand_is_asked_for_a_discovered_workspace_until_a_session_begins_there() {
    const CHANNEL: &str = "workspace-discovery-errand";
    let home = tempfile::tempdir().expect("create a home");
    let home = canonical(home.path());
    let projects = home.join("projects");
    let atlas = projects.join("atlas");
    repository(&atlas);
    name_roots(&home.join("config"), &[spelled(&projects)]);
    let (runtime, mut provider) = ControlledProvider::with_provider(
        ProviderId::new(PROVIDER),
        vec![hosted_model(PROVIDER, MODEL)],
    );
    let server = server::spawn_with_provider_and_timings(
        ServerConfig::new(home.join("state"), CHANNEL)
            .expect("configure")
            .with_config_dir(home.join("config")),
        runtime,
        ServerTimings::default()
            .with_checkout_observation_interval(Duration::from_secs(60 * 60))
            .with_root_discovery_memo(Duration::ZERO)
            .with_root_discovery_wait(AS_LONG_AS_IT_TAKES),
    )
    .await
    .expect("spawn server");
    server.workspace_discovery_settled().await;
    let mut client = connected_client(&home.join("state"), CHANNEL).await;
    let discovered = listed(&client).await;
    assert_eq!(
        discovered.iter().map(located).collect::<Vec<_>>(),
        [(repository_id(&atlas.join(".git")), atlas.clone())]
    );
    assert!(
        provider.try_next_errand().is_none(),
        "discovery asks no Errand for the Workspaces it finds"
    );

    let mut request = quiet_request(&atlas, "Chart the atlas");
    request.agent_selection = Some(hosted_selection(PROVIDER, MODEL));
    client
        .create_session(request)
        .await
        .expect("begin the first Session in the discovered Workspace");
    let title = timeout(PROGRESS_DEADLINE, provider.next_errand())
        .await
        .expect("the Title Errand reaches the Provider");
    assert!(
        title.schema()["properties"]["title"].is_object(),
        "the first Errand asked is the Session's Title: {}",
        title.schema()
    );
    title.succeed(json!({ "title": "Chart the atlas", "icon": "md-bug" }));
    next_derived_title(&mut client).await;
    let workspace_errand = timeout(PROGRESS_DEADLINE, provider.next_errand())
        .await
        .expect("the Workspace Errand reaches the Provider");
    workspace_errand.succeed(json!({
        "icon": "dev-rust",
        "description": "Where the atlas is charted.",
    }));
    next_workspace_description_changed(&mut client).await;
    next_workspace_icon_changed(&mut client).await;

    let described = listed(&client).await;
    assert_eq!(
        described
            .iter()
            .map(|workspace| (
                located(workspace),
                workspace.icon.clone(),
                workspace.description.clone()
            ))
            .collect::<Vec<_>>(),
        [(
            located(&discovered[0]),
            Some("dev-rust".to_owned()),
            Some(WorkspaceDescription {
                text: "Where the atlas is charted.".to_owned(),
                set: false,
            })
        )],
        "the first Session's Errand lands the Icon and Description on the discovered entry"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

/// Holds every thread that waits on it until it is opened.
#[derive(Default)]
struct Gate {
    open: std::sync::Mutex<bool>,
    opened: std::sync::Condvar,
}

impl Gate {
    fn wait(&self) {
        let mut open = self.open.lock().expect("the gate is never poisoned");
        while !*open {
            open = self.opened.wait(open).expect("the gate is never poisoned");
        }
    }

    fn open(&self) {
        *self.open.lock().expect("the gate is never poisoned") = true;
        self.opened.notify_all();
    }
}

/// Opens its gate once dropped, so no thread is left held when a test ends,
/// whether or not it ends well.
struct OpensOnDrop(Arc<Gate>);

impl Drop for OpensOnDrop {
    fn drop(&mut self) {
        self.0.open();
    }
}

/// Stands in for Git, but for every directory within `stalled`, whose
/// reading blocks the thread it is read on until the test opens `gate` — as
/// a filesystem probe of a network mount gone quiet does — counting each
/// such reading begun in `stalled_readings`.
struct StallsWithin {
    git: GitSourceControl,
    stalled: PathBuf,
    gate: Arc<Gate>,
    stalled_readings: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl SourceControl for StallsWithin {
    fn clone_capability(&self) -> suru::protocol::SourceControlCapability {
        self.git.clone_capability()
    }
    async fn clone_repository(
        &self,
        url: &str,
        destination: &Path,
        branch: Option<&str>,
        timeout: std::time::Duration,
        staging: &suru::source_control::CloneStaging,
    ) -> Result<suru::source_control::CloneLanding, suru::source_control::CloneFailure> {
        self.git
            .clone_repository(url, destination, branch, timeout, staging)
            .await
    }
    async fn settle_clones(&self) {
        self.git.settle_clones().await;
    }
    async fn sweep_abandoned_clones(&self, staging: &suru::source_control::CloneStaging) {
        self.git.sweep_abandoned_clones(staging).await;
    }
    async fn discover(&self, directory: &Path) -> ResolvedWorkspace {
        if directory.starts_with(&self.stalled) {
            self.stalled_readings.fetch_add(1, Ordering::SeqCst);
            self.gate.wait();
        }
        self.git.discover(directory).await
    }
}

/// A Root whose reading blocks the thread it is read on holds back neither
/// a change of the Roots — taking it away included — nor a listing, past
/// the Server's bound on waiting for discovery, even where the Server runs
/// on one thread: each answers, the listing from the Registry as it stands.
/// However often the Roots change, the directory within it that does not
/// answer is read once while that read is outstanding, rather than once
/// more for each scan. The Root named beside it goes on being discovered on
/// its own, a Repository cloned into it later included.
#[tokio::test(flavor = "current_thread")]
async fn a_root_whose_reading_stalls_holds_back_no_change_of_the_roots_and_no_listing() {
    const CHANNEL: &str = "workspace-discovery-stalled-root";
    let home = tempfile::tempdir().expect("create a home");
    let home = canonical(home.path());
    let stalled = home.join("stalled");
    std::fs::create_dir_all(stalled.join("inner")).expect("create the stalled Root");
    let projects = home.join("projects");
    let atlas = projects.join("atlas");
    repository(&atlas);
    let notes = home.join("notes");
    std::fs::create_dir(&notes).expect("create the notes directory");
    name_roots(&home.join("config"), &[]);
    let gate = Arc::new(Gate::default());
    let stall = OpensOnDrop(gate.clone());
    let stalled_readings = Arc::new(AtomicUsize::new(0));
    let server = server::spawn_with_source_control(
        ServerConfig::new(home.join("state"), CHANNEL)
            .expect("configure")
            .with_config_dir(home.join("config")),
        vec![Arc::new(FailingProviderRuntime)],
        ServerTimings::default()
            .with_checkout_observation_interval(Duration::from_secs(60 * 60))
            .with_root_discovery_memo(Duration::ZERO)
            .with_root_discovery_wait(Duration::from_millis(20)),
        Arc::new(StallsWithin {
            git: GitSourceControl::new("git"),
            stalled: stalled.clone(),
            gate,
            stalled_readings: stalled_readings.clone(),
        }),
    )
    .await
    .expect("spawn server");
    let client = connected_client(&home.join("state"), CHANNEL).await;
    let noted = client
        .create_session(quiet_request(&notes, "Keep the notes"))
        .await
        .expect("begin a Session in the notes directory")
        .session
        .workspace;
    let name = |roots: &[&Path]| SettingMutation::WorkspaceRoots {
        value: Some(roots.iter().map(|root| spelled(root).to_owned()).collect()),
    };
    let listed_at = async |path: &Path| {
        listed(&client)
            .await
            .iter()
            .any(|workspace| workspace.path == path)
    };

    timeout(
        PROGRESS_DEADLINE,
        client.mutate_setting(name(&[&stalled, &projects])),
    )
    .await
    .expect("naming a Root whose reading stalls answers")
    .expect("name the stalled Root beside another");
    timeout(PROGRESS_DEADLINE, async {
        while stalled_readings.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the directory within the stalled Root is read");
    let (stalled_root, projects_root) = (stalled.as_path(), projects.as_path());
    for roots in [[projects_root, stalled_root], [stalled_root, projects_root]]
        .iter()
        .cycle()
        .take(6)
    {
        timeout(PROGRESS_DEADLINE, client.mutate_setting(name(roots)))
            .await
            .expect("changing the Roots again answers")
            .expect("change the Roots again");
    }
    let listing = timeout(PROGRESS_DEADLINE, listed(&client))
        .await
        .expect("a listing answers while the stalled Root's scan blocks its thread");
    assert!(
        listing
            .iter()
            .any(|workspace| located(workspace) == located(&noted)),
        "the listing answers with what the Registry holds: {listing:?}"
    );
    timeout(PROGRESS_DEADLINE, async {
        while !listed_at(&atlas).await {}
    })
    .await
    .expect("the Root beside the stalled one is discovered");
    let copy = projects.join("atlas-copy");
    git(
        &projects,
        &["clone", "--quiet", spelled(&atlas), spelled(&copy)],
    );
    timeout(PROGRESS_DEADLINE, async {
        while !listed_at(&copy).await {}
    })
    .await
    .expect("a clone made later in the Root beside the stalled one is discovered too");
    assert_eq!(
        stalled_readings.load(Ordering::SeqCst),
        1,
        "the directory that does not answer is read once while that read is outstanding, \
         however often the Roots change"
    );

    timeout(PROGRESS_DEADLINE, client.mutate_setting(name(&[&projects])))
        .await
        .expect("taking the stalled Root away answers")
        .expect("take the stalled Root away");
    assert!(
        timeout(PROGRESS_DEADLINE, listed_at(&copy))
            .await
            .expect("a listing answers once the stalled Root is gone"),
        "what was discovered stays listed"
    );

    drop(stall);
    drop(client);
    server.shutdown().await.expect("shut down server");
}

/// A Repository a Session first knew through a linked Worktree, its main
/// checkout not known since its metadata lies apart, is named by its main
/// checkout once discovery finds that checkout directly within a Root —
/// rather than as one whose main checkout is unknown.
#[tokio::test]
async fn a_main_checkout_discovered_names_a_repository_a_session_knew_none_of() {
    const CHANNEL: &str = "workspace-discovery-main-checkout";
    let home = tempfile::tempdir().expect("create a home");
    let home = canonical(home.path());
    let projects = home.join("projects");
    std::fs::create_dir(&projects).expect("create the Workspace Root");
    let main = projects.join("atlas");
    let metadata = home.join("atlas.git");
    git(
        &home,
        &[
            "init",
            "-b",
            "main",
            "--separate-git-dir",
            spelled(&metadata),
            spelled(&main),
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
    git(&main, &["worktree", "add", "-b", "topic", spelled(&linked)]);
    let (_, server) = server_with_roots(&home, CHANNEL, &[], Duration::ZERO).await;
    let client = connected_client(&home.join("state"), CHANNEL).await;
    let created = client
        .create_session(quiet_request(&linked, "Chart the atlas"))
        .await
        .expect("begin a Session in the linked Worktree")
        .session;
    assert!(
        created.workspace.main_unknown(),
        "the Session knows no main checkout of its Repository"
    );

    client
        .mutate_setting(SettingMutation::WorkspaceRoots {
            value: Some(vec![spelled(&projects).to_owned()]),
        })
        .await
        .expect("name the Root the main checkout is in");

    let listing = client.list_workspaces().await.expect("list Workspaces");
    let listed = listing
        .workspaces
        .iter()
        .find(|workspace| workspace.id == created.workspace.id)
        .expect("the Repository is listed");
    assert_eq!(
        (
            listed.path.clone(),
            listing.workspace_paths.workspace_name(listed)
        ),
        (main.clone(), "atlas".to_owned()),
        "the Repository is named by the main checkout discovery found"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

/// Stands in for Git, but for `held`: its first reading is made by Git and
/// then held, read but not yet answered, until the test opens `gate`; every
/// later reading finds `held` no Repository's, as though its checkout had
/// moved on. Counts each reading of `held` begun in `readings`.
struct HoldsFirstReading {
    git: GitSourceControl,
    held: PathBuf,
    gate: Arc<Gate>,
    readings: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl SourceControl for HoldsFirstReading {
    fn clone_capability(&self) -> suru::protocol::SourceControlCapability {
        self.git.clone_capability()
    }
    async fn clone_repository(
        &self,
        url: &str,
        destination: &Path,
        branch: Option<&str>,
        timeout: std::time::Duration,
        staging: &suru::source_control::CloneStaging,
    ) -> Result<suru::source_control::CloneLanding, suru::source_control::CloneFailure> {
        self.git
            .clone_repository(url, destination, branch, timeout, staging)
            .await
    }
    async fn settle_clones(&self) {
        self.git.settle_clones().await;
    }
    async fn sweep_abandoned_clones(&self, staging: &suru::source_control::CloneStaging) {
        self.git.sweep_abandoned_clones(staging).await;
    }
    async fn discover(&self, directory: &Path) -> ResolvedWorkspace {
        if directory != self.held {
            return self.git.discover(directory).await;
        }
        if self.readings.fetch_add(1, Ordering::SeqCst) > 0 {
            return ResolvedWorkspace::directory(directory.to_owned());
        }
        let reading = self.git.discover(directory).await;
        self.gate.wait();
        reading
    }
}

/// What a scan read but did not enter before it was superseded teaches the
/// Server nothing. A Repository a Session knew without its main checkout
/// has that checkout read by a scan of a Root then taken away; the
/// checkout moves on before that reading ends, and the Repository is still
/// named as one whose main checkout is unknown, not by where it was.
#[tokio::test]
async fn a_main_checkout_read_by_a_superseded_scan_is_learned_nowhere() {
    const CHANNEL: &str = "workspace-discovery-superseded-reading";
    let home = tempfile::tempdir().expect("create a home");
    let home = canonical(home.path());
    let projects = home.join("projects");
    std::fs::create_dir(&projects).expect("create the Workspace Root");
    let main = projects.join("atlas");
    let metadata = home.join("atlas.git");
    git(
        &home,
        &[
            "init",
            "-b",
            "main",
            "--separate-git-dir",
            spelled(&metadata),
            spelled(&main),
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
    git(&main, &["worktree", "add", "-b", "topic", spelled(&linked)]);
    name_roots(&home.join("config"), &[]);
    let gate = Arc::new(Gate::default());
    let held = OpensOnDrop(gate.clone());
    let readings = Arc::new(AtomicUsize::new(0));
    let server = server::spawn_with_source_control(
        ServerConfig::new(home.join("state"), CHANNEL)
            .expect("configure")
            .with_config_dir(home.join("config")),
        vec![Arc::new(FailingProviderRuntime)],
        ServerTimings::default()
            .with_checkout_observation_interval(Duration::from_secs(60 * 60))
            .with_root_discovery_memo(Duration::ZERO)
            .with_root_discovery_wait(Duration::from_millis(20)),
        Arc::new(HoldsFirstReading {
            git: GitSourceControl::new("git"),
            held: main.clone(),
            gate,
            readings: readings.clone(),
        }),
    )
    .await
    .expect("spawn server");
    let client = connected_client(&home.join("state"), CHANNEL).await;
    let created = client
        .create_session(quiet_request(&linked, "Chart the atlas"))
        .await
        .expect("begin a Session in the linked Worktree")
        .session;
    assert!(created.workspace.main_unknown());
    let name = |roots: &[&Path]| SettingMutation::WorkspaceRoots {
        value: Some(roots.iter().map(|root| spelled(root).to_owned()).collect()),
    };

    client
        .mutate_setting(name(&[&projects]))
        .await
        .expect("name the Root the main checkout is in");
    timeout(PROGRESS_DEADLINE, async {
        while readings.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a scan reads the main checkout");
    client
        .mutate_setting(name(&[]))
        .await
        .expect("take the Root away while its reading is held");
    drop(held);
    client
        .mutate_setting(name(&[&projects]))
        .await
        .expect("name the Root again");
    timeout(PROGRESS_DEADLINE, async {
        while readings.load(Ordering::SeqCst) < 2 {
            listed(&client).await;
        }
    })
    .await
    .expect("a later scan reads the directory afresh, once the held reading has ended");

    let listing = client.list_workspaces().await.expect("list Workspaces");
    let listed = listing
        .workspaces
        .iter()
        .find(|workspace| workspace.id == created.workspace.id)
        .expect("the Repository is listed");
    assert_eq!(
        listing.workspace_paths.workspace_name(listed),
        "atlas.git (main checkout unknown)",
        "the main checkout the superseded scan read is learned nowhere"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}
