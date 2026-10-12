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
};

/// A memo no test outlasts: every listing after the first reuses its scan.
const NEVER_AGAIN: Duration = Duration::from_secs(60 * 60);

/// A bound on waiting for discovery that no test reaches, so whatever asks
/// for a scan of the Roots answers with what it found, however slowly it
/// runs on a loaded machine.
pub(crate) const AS_LONG_AS_IT_TAKES: Duration = Duration::from_secs(60 * 60);

/// The Config Document a Server reads its Workspace Roots from, naming
/// `roots`, written into `config_dir`.
fn name_roots(config_dir: &Path, roots: &[&str]) {
    std::fs::create_dir_all(config_dir).expect("create the config root");
    let document = json!({ "workspace": { "roots": roots } });
    std::fs::write(config_dir.join("suru.jsonc"), document.to_string())
        .expect("write the Config Document");
}

/// A path as a Workspace Root names it.
fn spelled(path: &Path) -> &str {
    path.to_str().expect("the fixture's path is UTF-8")
}

/// A Server beneath `home` whose Workspace Roots are `roots`, reusing a scan
/// for `memo` and waiting on each as long as it takes, and which leaves the
/// Worktrees it knows alone, so no Git it runs keeps a Repository's
/// directory open when a test lets go of it, which Windows refuses to
/// delete.
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
            .with_root_discovery_memo(memo)
            .with_root_discovery_wait(AS_LONG_AS_IT_TAKES),
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
/// down, a hidden Repository, a linked Worktree of a Repository presented
/// elsewhere, and — where links are made — a link to a Repository elsewhere
/// are each left alone, and so is the Root itself, which is a Repository
/// here, so every plain directory within it, and a link to it, is the
/// Root's.
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

/// Stands in for Git, but for every directory within `stalled`, whose
/// reading never finishes, as a Root on a network mount gone quiet.
struct StallsWithin {
    git: GitSourceControl,
    stalled: PathBuf,
}

#[async_trait::async_trait]
impl SourceControl for StallsWithin {
    async fn discover(&self, directory: &Path) -> ResolvedWorkspace {
        if directory.starts_with(&self.stalled) {
            std::future::pending::<()>().await;
        }
        self.git.discover(directory).await
    }
}

/// A Root whose reading stalls holds back neither a change of the Roots —
/// taking it away included — nor a listing, past the Server's bound on
/// waiting for discovery: each answers, the listing from the Registry as it
/// stands, and a Root named in its place is discovered all the same.
#[tokio::test]
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

    timeout(PROGRESS_DEADLINE, client.mutate_setting(name(&[&stalled])))
        .await
        .expect("naming a Root whose reading stalls answers")
        .expect("name the stalled Root");
    let listing = timeout(PROGRESS_DEADLINE, listed(&client))
        .await
        .expect("a listing answers while the scan stalls");
    assert_eq!(
        listing.iter().map(located).collect::<Vec<_>>(),
        [located(&noted)],
        "the listing answers with what the Registry holds"
    );

    timeout(PROGRESS_DEADLINE, client.mutate_setting(name(&[&projects])))
        .await
        .expect("replacing the Root whose reading stalls answers")
        .expect("name another Root in its place");
    timeout(PROGRESS_DEADLINE, async {
        while !listed(&client)
            .await
            .iter()
            .any(|workspace| workspace.path == atlas)
        {}
    })
    .await
    .expect("the Root named in place of the stalled one is discovered");

    drop(client);
    server.shutdown().await.expect("shut down server");
}
