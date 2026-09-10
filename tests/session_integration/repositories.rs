//! Repository grouping through the owning Server's authenticated boundaries.
use crate::server_support::PROGRESS_DEADLINE;
use crate::{
    failing_provider_support::{FailingProviderRuntime, spawn_with_failing_provider},
    support,
};
use diesel::{Connection, RunQueryDsl};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};
use suru::{
    protocol::*,
    server::{self, ServerConfig, ServerTimings},
    source_control::GitSourceControl,
};

pub(super) fn git(directory: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Suru Test")
        .env("GIT_AUTHOR_EMAIL", "suru@example.invalid")
        .env("GIT_COMMITTER_NAME", "Suru Test")
        .env("GIT_COMMITTER_EMAIL", "suru@example.invalid")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn init(root: &Path) {
    std::fs::create_dir_all(root).unwrap();
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
fn request(
    descriptor: &RuntimeDescriptor,
    method: reqwest::Method,
    path: &str,
) -> reqwest::RequestBuilder {
    reqwest::Client::new()
        .request(method, format!("{}{path}", descriptor.base_url))
        .bearer_auth(&descriptor.token)
}
async fn resolve(
    descriptor: &RuntimeDescriptor,
    path: &Path,
    id: Option<WorkspaceId>,
) -> ResolvedWorkspace {
    request(descriptor, reqwest::Method::POST, "/v1/workspaces/resolve")
        .json(&ResolveWorkspaceRequest {
            checkout_id: None,
            remembered_execution_directory: None,
            workspace_id: id,
            base: None,
            path: path.to_owned(),
        })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}
async fn create(descriptor: &RuntimeDescriptor, path: &Path) -> SessionSnapshot {
    let created = support::create_session(
        descriptor,
        &CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: ExecutionDirectory {
                path: path.to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Retain this exact working directory".to_owned(),
                skill_invocations: vec![],
            },
        },
    )
    .await;
    support::read_session_at_least_revision(
        &reqwest::Client::new(),
        descriptor,
        created.session.id,
        SessionRevision(2),
    )
    .await
}
async fn list(descriptor: &RuntimeDescriptor, id: Option<&WorkspaceId>) -> Vec<SessionListItem> {
    let mut request = request(descriptor, reqwest::Method::GET, "/v1/sessions");
    if let Some(id) = id {
        request = request.query(&[("workspace_id", &id.0)]);
    }
    request
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}
fn canonical(path: &Path) -> PathBuf {
    suru::paths::canonical(path).unwrap()
}

#[tokio::test]
async fn main_linked_and_subdirectory_sessions_share_authenticated_listing_and_identity_filters() {
    let temporary = tempfile::tempdir().unwrap();
    let root = canonical(temporary.path());
    let main = root.join("main repo");
    init(&main);
    let linked = root.join("external checkout");
    git(
        &main,
        &["worktree", "add", "-b", "topic", linked.to_str().unwrap()],
    );
    let main_subdir = main.join("packages");
    let linked_subdir = linked.join("src");
    std::fs::create_dir(&main_subdir).unwrap();
    std::fs::create_dir(&linked_subdir).unwrap();
    let server =
        spawn_with_failing_provider(ServerConfig::new(root.join("state"), "repo-api").unwrap())
            .await
            .unwrap();
    let descriptor = server.descriptor();
    let mut sessions = vec![];
    for path in [&main, &linked, &main_subdir, &linked_subdir] {
        let created = create(descriptor, path).await;
        assert_eq!(&created.session.execution_directory.path, path);
        assert_eq!(created.session.workspace.path, main);
        sessions.push(created);
    }
    let id = &sessions[0].session.workspace.id;
    assert!(
        sessions
            .iter()
            .all(|session| &session.session.workspace.id == id)
    );
    let listing = list(descriptor, Some(id)).await;
    assert_eq!(
        listing.iter().map(SessionListItem::id).collect::<Vec<_>>(),
        sessions
            .iter()
            .rev()
            .map(|session| session.session.id)
            .collect::<Vec<_>>()
    );
    let via_path: Vec<SessionListItem> = request(descriptor, reqwest::Method::GET, "/v1/sessions")
        .query(&[("workspace", linked_subdir.to_str().unwrap())])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(via_path, listing);
    let resolved = resolve(descriptor, &linked_subdir, None).await;
    assert_eq!(&resolved.workspace.id, id);
    assert_eq!(resolved.execution_directory.unwrap().path, linked_subdir);
    let outside = root.join("plain directory");
    std::fs::create_dir(&outside).unwrap();
    let plain = create(descriptor, &outside).await;
    assert_ne!(&plain.session.workspace.id, id);
    assert_eq!(
        plain.session.workspace.source_control,
        SourceControlAvailability::NotDetected
    );
    assert_eq!(list(descriptor, Some(id)).await.len(), 4);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn learning_separate_main_updates_streamed_presentation_without_splitting_workspace_identity()
{
    let temporary = tempfile::tempdir().unwrap();
    let root = canonical(temporary.path());
    let main = root.join("main checkout");
    let metadata = root.join("separate metadata");
    git(
        &root,
        &[
            "init",
            "-b",
            "main",
            "--separate-git-dir",
            metadata.to_str().unwrap(),
            main.to_str().unwrap(),
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
    let linked = root.join("linked");
    git(
        &main,
        &["worktree", "add", "-b", "topic", linked.to_str().unwrap()],
    );
    let server =
        spawn_with_failing_provider(ServerConfig::new(root.join("state"), "repo-label").unwrap())
            .await
            .unwrap();
    let descriptor = server.descriptor();
    let original = create(descriptor, &linked).await;
    assert!(original.session.workspace.main_unknown());
    let mut catalog = request(descriptor, reqwest::Method::GET, "/v1/session-events")
        .send()
        .await
        .unwrap()
        .bytes_stream()
        .eventsource();
    let initial = tokio::time::timeout(PROGRESS_DEADLINE, catalog.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(initial.event, SESSION_CATALOG_SNAPSHOT_EVENT);
    let known = resolve(descriptor, &main, None).await;
    assert_eq!(known.workspace.id, original.session.workspace.id);
    assert_eq!(known.workspace.path, main);
    // Checkout observation runs on its own cadence and publishes its reading of
    // the Worktree to this same stream, so the invalidation learning the main
    // working copy causes is not necessarily the first update to arrive — which
    // of the two lands first is a race this test has no stake in. Wait for the
    // change being asserted rather than for whichever poller happened to tick
    // first; the session it names is still what proves the relabelling.
    let change = tokio::time::timeout(PROGRESS_DEADLINE, async {
        loop {
            let event = catalog.next().await.unwrap().unwrap();
            if event.event != SESSION_CATALOG_UPDATED_EVENT {
                continue;
            }
            let update: SessionCatalogUpdate = serde_json::from_str(&event.data).unwrap();
            if matches!(update.change, SessionCatalogChange::Invalidated { .. }) {
                break update.change;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        change,
        SessionCatalogChange::Invalidated {
            session_id: original.session.id
        }
    );
    let relabeled = support::read_session(descriptor, original.session.id).await;
    assert_eq!(relabeled.session.workspace, known.workspace);
    assert_eq!(
        relabeled.session.execution_directory,
        original.session.execution_directory
    );
    assert_eq!(relabeled.prompts, original.prompts);
    assert_eq!(relabeled.turns, original.turns);
    let selected = resolve(descriptor, &linked, Some(known.workspace.id.clone())).await;
    assert_eq!(selected.workspace.path, main);
    assert!(
        selected
            .checkouts
            .iter()
            .any(|checkout| checkout.association.root == main
                && checkout.association.kind == CheckoutKind::Main)
    );
    assert_eq!(list(descriptor, Some(&known.workspace.id)).await.len(), 1);
    std::fs::remove_dir_all(&main).unwrap();
    let missing_main = resolve(descriptor, &linked, None).await;
    assert_eq!(missing_main.workspace.id, known.workspace.id);
    assert!(
        missing_main
            .checkouts
            .iter()
            .any(|checkout| checkout.association.root == main
                && matches!(
                    checkout.availability,
                    SourceControlAvailability::Unavailable { .. }
                ))
    );
    drop(catalog);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn bare_root_is_grouping_only_while_missing_git_leaves_ordinary_session_creation_usable() {
    let temporary = tempfile::tempdir().unwrap();
    let root = canonical(temporary.path());
    let bare = root.join("bare.git");
    git(&root, &["init", "--bare", bare.to_str().unwrap()]);
    let config = ServerConfig::new(root.join("state"), "bare-api").unwrap();
    let server = spawn_with_failing_provider(config).await.unwrap();
    assert!(
        resolve(server.descriptor(), &bare, None)
            .await
            .execution_directory
            .is_none()
    );
    let response = request(server.descriptor(), reqwest::Method::POST, "/v1/sessions")
        .json(&CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: ExecutionDirectory { path: bare.clone() },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Never start here".to_owned(),
                skill_invocations: vec![],
            },
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    assert!(list(server.descriptor(), None).await.is_empty());
    server.shutdown().await.unwrap();
    let server = server::spawn_with_source_control(
        ServerConfig::new(root.join("no-git-state"), "no-git").unwrap(),
        vec![Arc::new(FailingProviderRuntime)],
        ServerTimings::default(),
        Arc::new(GitSourceControl::new(root.join("absent-git"))),
    )
    .await
    .unwrap();
    let ordinary = create(server.descriptor(), &root).await;
    assert!(
        matches!(ordinary.session.workspace.source_control, SourceControlAvailability::Unavailable { ref reason } if reason.contains("not installed"))
    );
    assert_eq!(ordinary.session.execution_directory.path, root);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn legacy_regroup_is_durable_and_lazy_with_missing_membership_unresolved() {
    let temporary = tempfile::tempdir().unwrap();
    let root = canonical(temporary.path());
    let main = root.join("main");
    init(&main);
    let linked = root.join("linked");
    git(
        &main,
        &["worktree", "add", "-b", "topic", linked.to_str().unwrap()],
    );
    let missing = root.join("missing legacy");
    std::fs::create_dir(&missing).unwrap();
    let config = ServerConfig::new(root.join("state"), "legacy-repositories").unwrap();
    let original = spawn_with_failing_provider(config.clone()).await.unwrap();
    let readable = create(original.descriptor(), &main).await;
    let corrupt_history = create(original.descriptor(), &linked).await;
    let missing_legacy = create(original.descriptor(), &missing).await;
    original.shutdown().await.unwrap();
    let mut db =
        diesel::SqliteConnection::establish(config.data_dir().join("suru.db").to_str().unwrap())
            .unwrap();
    for snapshot in [&readable, &corrupt_history, &missing_legacy] {
        diesel::sql_query("UPDATE sessions SET workspace = ? WHERE id = ?")
            .bind::<diesel::sql_types::Text, _>(
                serde_json::json!({"path": snapshot.session.execution_directory.path}).to_string(),
            )
            .bind::<diesel::sql_types::Text, _>(snapshot.session.id.to_string())
            .execute(&mut db)
            .unwrap();
    }
    diesel::sql_query("UPDATE prompts SET payload = '{' WHERE session_id = ?")
        .bind::<diesel::sql_types::Text, _>(corrupt_history.session.id.to_string())
        .execute(&mut db)
        .unwrap();
    std::fs::remove_dir(&missing).unwrap();
    let resumed = spawn_with_failing_provider(config.clone()).await.unwrap();
    let rows = list(resumed.descriptor(), Some(&readable.session.workspace.id)).await;
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .all(|row| matches!(row, SessionListItem::Readable(_))),
        "discovery must not hydrate malformed history"
    );
    let restored = support::read_session(resumed.descriptor(), readable.session.id).await;
    assert_eq!(restored.prompts, readable.prompts);
    assert_eq!(restored.turns, readable.turns);
    assert_eq!(
        restored.session.execution_directory,
        readable.session.execution_directory
    );
    assert_eq!(restored.session.checkout, readable.session.checkout);
    let unresolved = list(
        resumed.descriptor(),
        Some(&WorkspaceId::directory(&missing)),
    )
    .await;
    assert_eq!(unresolved.len(), 1);
    let SessionListItem::Readable(unresolved) = &unresolved[0] else {
        panic!("legacy summary remains readable")
    };
    assert!(unresolved.session.workspace.repository.is_none());
    assert_eq!(
        request(
            resumed.descriptor(),
            reqwest::Method::GET,
            &format!("/v1/sessions/{}", corrupt_history.session.id)
        )
        .send()
        .await
        .unwrap()
        .status(),
        reqwest::StatusCode::NOT_FOUND
    );
    resumed.shutdown().await.unwrap();
    let restarted = spawn_with_failing_provider(config).await.unwrap();
    let restored = support::read_session(restarted.descriptor(), readable.session.id).await;
    assert_eq!(restored.session.workspace, readable.session.workspace);
    assert_eq!(restored.prompts, readable.prompts);
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn missing_known_checkout_does_not_poison_readable_repository_on_restart() {
    let temporary = tempfile::tempdir().unwrap();
    let root = canonical(temporary.path());
    let main = root.join("main");
    init(&main);
    let linked = root.join("linked");
    git(
        &main,
        &["worktree", "add", "-b", "topic", linked.to_str().unwrap()],
    );
    let config = ServerConfig::new(root.join("state"), "missing-checkout").unwrap();
    let server = spawn_with_failing_provider(config.clone()).await.unwrap();
    let main_session = create(server.descriptor(), &main).await;
    let linked_session = create(server.descriptor(), &linked).await;
    server.shutdown().await.unwrap();
    std::fs::remove_dir_all(&linked).unwrap();
    let server = spawn_with_failing_provider(config.clone()).await.unwrap();
    for row in list(
        server.descriptor(),
        Some(&main_session.session.workspace.id),
    )
    .await
    {
        let SessionListItem::Readable(row) = row else {
            panic!("readable summary")
        };
        assert_eq!(
            row.session.workspace.source_control,
            SourceControlAvailability::Available
        );
    }
    let known = support::read_session(server.descriptor(), linked_session.session.id).await;
    assert_eq!(known.session.execution_directory.path, linked);
    let resolution = resolve(server.descriptor(), &main, None).await;
    assert!(
        resolution
            .checkouts
            .iter()
            .any(|checkout| checkout.association.root == linked
                && matches!(
                    checkout.availability,
                    SourceControlAvailability::Unavailable { .. }
                ))
    );
    server.shutdown().await.unwrap();
    std::fs::rename(main.join(".git"), root.join("moved-metadata")).unwrap();
    let server = spawn_with_failing_provider(config).await.unwrap();
    let rows = list(
        server.descriptor(),
        Some(&main_session.session.workspace.id),
    )
    .await;
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| matches!(row, SessionListItem::Readable(summary) if matches!(summary.session.workspace.source_control, SourceControlAvailability::Unavailable { .. }))));
    // An ordinary Session can still execute in the surviving filesystem directory.
    assert_eq!(
        create(server.descriptor(), &main)
            .await
            .session
            .execution_directory
            .path,
        main
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn session_creation_rejects_execution_directory_that_changed_since_discovery() {
    struct PreviousDirectory(PathBuf);
    #[async_trait::async_trait]
    impl suru::source_control::SourceControl for PreviousDirectory {
        async fn discover(&self, _directory: &Path) -> ResolvedWorkspace {
            ResolvedWorkspace::directory(self.0.clone())
        }
    }
    let temporary = tempfile::tempdir().unwrap();
    let root = canonical(temporary.path());
    let before = root.join("before");
    let after = root.join("after");
    std::fs::create_dir(&before).unwrap();
    std::fs::create_dir(&after).unwrap();
    let server = server::spawn_with_source_control(
        ServerConfig::new(root.join("state"), "changed-execution").unwrap(),
        vec![Arc::new(FailingProviderRuntime)],
        ServerTimings::default(),
        Arc::new(PreviousDirectory(before)),
    )
    .await
    .unwrap();
    let response = request(server.descriptor(), reqwest::Method::POST, "/v1/sessions")
        .json(&CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: ExecutionDirectory { path: after },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Do not mix directory contexts".to_owned(),
                skill_invocations: vec![],
            },
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    assert!(list(server.descriptor(), None).await.is_empty());
    server.shutdown().await.unwrap();
}

/// The next catalog change satisfying `wanted`, skipping the ones that do not.
/// Several publishers share this one stream, and the Worktree readings this
/// module waits on are announced on observation's own cadence.
macro_rules! await_catalog_change {
    ($catalog:expr, $wanted:expr) => {
        tokio::time::timeout(PROGRESS_DEADLINE, async {
            loop {
                let event = $catalog.next().await.unwrap().unwrap();
                if event.event != SESSION_CATALOG_UPDATED_EVENT {
                    continue;
                }
                let update: SessionCatalogUpdate = serde_json::from_str(&event.data).unwrap();
                if $wanted(&update.change) {
                    break;
                }
            }
        })
        .await
        .expect("the awaited catalog change is announced")
    };
}

/// A Worktree no Session works in is still the owning Server's to watch: once a
/// Workspace resolution makes the Repository known, every Worktree it has joins
/// the observed set, lands whole in the catalog snapshot a client joins on, and
/// announces its own external branch changes.
#[tokio::test]
async fn known_repository_worktrees_are_observed_without_any_session_referencing_them() {
    let temporary = tempfile::tempdir().unwrap();
    let root = canonical(temporary.path());
    let main = root.join("main");
    init(&main);
    let spare = root.join("spare");
    git(
        &main,
        &["worktree", "add", "-b", "spare", spare.to_str().unwrap()],
    );
    let timings = ServerTimings {
        shutdown_grace: std::time::Duration::from_millis(5),
        ..Default::default()
    }
    .with_checkout_observation_interval(std::time::Duration::from_millis(15));
    let server = server::spawn_with_provider_and_timings(
        ServerConfig::new(root.join("state"), "session-less-worktrees").unwrap(),
        Arc::new(FailingProviderRuntime),
        timings,
    )
    .await
    .unwrap();
    let descriptor = server.descriptor();
    // The Server knows this Repository through the resolution alone: no Session
    // exists here, or anywhere.
    let resolved = resolve(descriptor, &main, None).await;
    let checkout_id = |wanted: &Path| {
        resolved
            .checkouts
            .iter()
            .find(|checkout| checkout.association.root == wanted)
            .unwrap_or_else(|| panic!("{} is a Worktree of this Repository", wanted.display()))
            .association
            .id
            .clone()
    };
    let main_id = checkout_id(&main);
    let spare_id = checkout_id(&spare);
    let mut catalog = request(descriptor, reqwest::Method::GET, "/v1/session-events")
        .send()
        .await
        .unwrap()
        .bytes_stream()
        .eventsource();
    let opening = tokio::time::timeout(PROGRESS_DEADLINE, catalog.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(opening.event, SESSION_CATALOG_SNAPSHOT_EVENT);
    fn reading(change: &SessionCatalogChange, wanted_id: &CheckoutId, wanted: &str) -> bool {
        matches!(
            change,
            SessionCatalogChange::CheckoutStateChanged { checkout_id, checkout_state: Some(state) }
                if checkout_id == wanted_id
                    && state.availability == SourceControlAvailability::Available
                    && matches!(&state.revision, Some(CheckoutRevision::Branch { name, .. }) if name == wanted)
        )
    }
    // Which Worktree's reading lands first is a race this test has no stake in.
    let mut seen = std::collections::HashSet::new();
    await_catalog_change!(catalog, |change: &SessionCatalogChange| {
        if reading(change, &spare_id, "spare") {
            seen.insert(&spare_id);
        }
        if reading(change, &main_id, "main") {
            seen.insert(&main_id);
        }
        seen.len() == 2
    });
    // A client joining now takes both readings whole, though no Session names
    // either Worktree.
    let mut rejoined = request(descriptor, reqwest::Method::GET, "/v1/session-events")
        .send()
        .await
        .unwrap()
        .bytes_stream()
        .eventsource();
    let snapshot = tokio::time::timeout(PROGRESS_DEADLINE, rejoined.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.event, SESSION_CATALOG_SNAPSHOT_EVENT);
    let snapshot: SessionCatalogSnapshot = serde_json::from_str(&snapshot.data).unwrap();
    assert!(snapshot.session_ids.is_empty());
    let mut observed = snapshot
        .checkout_states
        .iter()
        .map(|state| state.association.root.clone())
        .collect::<Vec<_>>();
    observed.sort();
    let mut expected = vec![main.clone(), spare.clone()];
    expected.sort();
    assert_eq!(observed, expected);
    drop(rejoined);
    // Switching the Session-less Worktree's branch outside Suru is announced.
    git(&spare, &["checkout", "-b", "moved"]);
    await_catalog_change!(catalog, |change| reading(change, &spare_id, "moved"));
    drop(catalog);
    server.shutdown().await.unwrap();
}

/// A Repository whose listing fails for a moment keeps the Worktrees it was
/// already observed to have: none of them is retired, so no Client is told a
/// Checkout State has gone only to be told it is back on the next tick.
#[tokio::test]
async fn a_failed_worktree_listing_retires_nothing_it_could_not_speak_for() {
    /// Git in every respect but the one listing this fails, which answers as a
    /// briefly unrunnable Git does.
    struct FailingListing {
        git: GitSourceControl,
        listings: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl suru::source_control::SourceControl for FailingListing {
        async fn discover(&self, directory: &Path) -> ResolvedWorkspace {
            self.git.discover(directory).await
        }
        async fn observe(&self, checkout: &CheckoutAssociation) -> CheckoutSummary {
            self.git.observe(checkout).await
        }
        async fn list_checkouts(
            &self,
            repository: &Repository,
        ) -> Result<Vec<CheckoutAssociation>, String> {
            // The first listing establishes the observed set; the second fails,
            // and every listing after it answers again.
            if self
                .listings
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                == 1
            {
                return Err("Git could not be run".to_owned());
            }
            self.git.list_checkouts(repository).await
        }
    }

    let temporary = tempfile::tempdir().unwrap();
    let root = canonical(temporary.path());
    let main = root.join("main");
    init(&main);
    let spare = root.join("spare");
    git(
        &main,
        &["worktree", "add", "-b", "spare", spare.to_str().unwrap()],
    );
    let source_control = Arc::new(FailingListing {
        git: GitSourceControl::default(),
        listings: std::sync::atomic::AtomicUsize::new(0),
    });
    let timings = ServerTimings {
        shutdown_grace: std::time::Duration::from_millis(5),
        ..Default::default()
    }
    .with_checkout_observation_interval(std::time::Duration::from_millis(15));
    let server = server::spawn_with_source_control(
        ServerConfig::new(root.join("state"), "failing-listing").unwrap(),
        vec![Arc::new(FailingProviderRuntime)],
        timings,
        source_control.clone(),
    )
    .await
    .unwrap();
    let descriptor = server.descriptor();
    let resolved = resolve(descriptor, &main, None).await;
    let spare_id = resolved
        .checkouts
        .iter()
        .find(|checkout| checkout.association.root == spare)
        .expect("the spare Worktree is one of this Repository's")
        .association
        .id
        .clone();
    let mut catalog = request(descriptor, reqwest::Method::GET, "/v1/session-events")
        .send()
        .await
        .unwrap()
        .bytes_stream()
        .eventsource();
    let opening = tokio::time::timeout(PROGRESS_DEADLINE, catalog.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(opening.event, SESSION_CATALOG_SNAPSHOT_EVENT);
    // Wait out the failing listing, then give the observation something to
    // announce that can only be announced after it: every change up to that
    // point is then in hand, and none of them may be a retirement.
    tokio::time::timeout(PROGRESS_DEADLINE, async {
        while source_control
            .listings
            .load(std::sync::atomic::Ordering::SeqCst)
            < 3
        {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the observation keeps listing after a failure");
    git(&spare, &["checkout", "-b", "moved"]);
    let mut retired = Vec::new();
    await_catalog_change!(catalog, |change: &SessionCatalogChange| {
        if let SessionCatalogChange::CheckoutStateChanged {
            checkout_id,
            checkout_state: None,
        } = change
        {
            retired.push(checkout_id.clone());
        }
        matches!(
            change,
            SessionCatalogChange::CheckoutStateChanged { checkout_id, checkout_state: Some(state) }
                if checkout_id == &spare_id
                    && matches!(&state.revision, Some(CheckoutRevision::Branch { name, .. }) if name == "moved")
        )
    });
    assert!(
        retired.is_empty(),
        "a failed listing retires nothing: {retired:?}"
    );
    drop(catalog);
    server.shutdown().await.unwrap();
}
