//! Real external Git mutations enter via the owning Server's catalog stream.
use super::*;
use std::path::Path;
use suru::{
    managed_client::{OutlookClient, SessionCatalogSubscription},
    protocol::{
        CheckoutRevision, CheckoutSummary, SessionId, SessionListItem, SessionSummary,
        SourceControlAvailability,
    },
};

fn git(root: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Suru Test")
        .env("GIT_AUTHOR_EMAIL", "suru@example.invalid")
        .env("GIT_COMMITTER_NAME", "Suru Test")
        .env("GIT_COMMITTER_EMAIL", "suru@example.invalid")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
async fn create(client: &OutlookClient, path: &Path) -> SessionId {
    client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: path.to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Checkout observation".into(),
                skill_invocations: vec![],
            },
        })
        .await
        .unwrap()
        .session
        .id
}
async fn summaries(client: &OutlookClient) -> Vec<SessionSummary> {
    client
        .list_sessions(None)
        .await
        .unwrap()
        .into_iter()
        .map(|item| {
            let SessionListItem::Readable(summary) = item else {
                panic!("readable")
            };
            *summary
        })
        .collect()
}
async fn observed(
    client: &OutlookClient,
    subscription: &mut SessionCatalogSubscription,
    count: usize,
    expected: impl Fn(&CheckoutSummary) -> bool,
) -> Vec<SessionSummary> {
    timeout(Duration::from_secs(3), async {
        loop {
            let listed = summaries(client).await;
            if listed.len() == count
                && listed
                    .iter()
                    .all(|summary| summary.checkout_state.as_ref().is_some_and(&expected))
            {
                return listed;
            }
            subscription.next().await.expect("catalog stays open");
        }
    })
    .await
    .expect("external Git change is observed through the catalog")
}
fn branch(reading: &CheckoutSummary, expected: &str) -> bool {
    reading.availability == SourceControlAvailability::Available
        && matches!(&reading.revision, Some(CheckoutRevision::Branch { name, .. }) if name == expected)
}

#[tokio::test]
async fn shared_checkout_streams_external_changes_to_two_clients_and_recovers_facts_without_history()
 {
    let state = tempfile::tempdir().unwrap();
    let config_root = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let main = root.path().join("main");
    let linked = root.path().join("linked");
    std::fs::create_dir(&main).unwrap();
    git(&main, &["init", "-b", "main"]);
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
    git(
        &main,
        &["worktree", "add", "-b", "feature", linked.to_str().unwrap()],
    );
    let config = ServerConfig::new(state.path(), "checkout-stream")
        .unwrap()
        .with_config_dir(config_root.path());
    let database = config.data_dir().join("suru.db");
    let timings = ServerTimings {
        shutdown_grace: Duration::from_millis(5),
        ..Default::default()
    }
    .with_checkout_observation_interval(Duration::from_millis(15));
    let server = server::spawn_with_timings(config.clone(), timings)
        .await
        .unwrap();
    let first =
        ManagedClient::connect(ManagedClientConfig::new(state.path(), "checkout-stream").unwrap())
            .await
            .unwrap();
    let second =
        ManagedClient::connect(ManagedClientConfig::new(state.path(), "checkout-stream").unwrap())
            .await
            .unwrap();
    let local = first.outlook(Outlook::Local);
    let other = second.outlook(Outlook::Local);
    let mut one = local.subscribe_catalog();
    let mut two = other.subscribe_catalog();
    let a = create(&local, &linked).await;
    let nested = linked.join("nested");
    std::fs::create_dir(&nested).unwrap();
    let b = create(&other, &nested).await;
    let before = observed(&local, &mut one, 2, |r| branch(r, "feature")).await;
    observed(&other, &mut two, 2, |r| branch(r, "feature")).await;
    git(&linked, &["checkout", "-b", "external"]);
    let after = observed(&local, &mut one, 2, |r| branch(r, "external")).await;
    assert_eq!(after[0].checkout_state, after[1].checkout_state);
    observed(&other, &mut two, 2, |r| branch(r, "external")).await;
    for summary in &after {
        let previous = before
            .iter()
            .find(|old| old.session.id == summary.session.id)
            .unwrap();
        assert_eq!(
            summary.updated_at, previous.updated_at,
            "Git does not count as Session activity"
        );
        assert_eq!(
            summary.session.checkout.as_ref().unwrap().recovery_revision,
            summary.checkout_state.as_ref().unwrap().revision
        );
    }
    git(&linked, &["checkout", "--detach"]);
    let detached = observed(&local, &mut one, 2, |r| {
        matches!(r.revision, Some(CheckoutRevision::Detached { .. }))
    })
    .await;
    let recovery = detached[0].session.checkout.clone().unwrap();
    observed(&other, &mut two, 2, |r| {
        matches!(r.revision, Some(CheckoutRevision::Detached { .. }))
    })
    .await;
    std::fs::remove_dir_all(&linked).unwrap();
    let unavailable = observed(&local, &mut one, 2, |r| {
        matches!(
            r.availability,
            SourceControlAvailability::Unavailable { .. }
        )
    })
    .await;
    assert!(
        unavailable.iter().all(|summary| summary
            .checkout_state
            .as_ref()
            .unwrap()
            .revision
            .is_none())
    );
    assert_eq!(
        unavailable[0]
            .session
            .checkout
            .as_ref()
            .unwrap()
            .recovery_revision,
        recovery.recovery_revision
    );
    drop(one);
    drop(two);
    drop(local);
    drop(other);
    drop(first);
    drop(second);
    server.shutdown().await.unwrap();
    // If observation hydrates history this malformed content makes the rows
    // unreadable. Discovery, recovery metadata and live updates must not read it.
    seed_database(
        &database,
        "UPDATE prompts SET payload = '{invalid history';",
    );
    let server = server::spawn_with_timings(config, timings).await.unwrap();
    let client =
        ManagedClient::connect(ManagedClientConfig::new(state.path(), "checkout-stream").unwrap())
            .await
            .unwrap();
    let local = client.outlook(Outlook::Local);
    let initial = summaries(&local).await;
    assert_eq!(initial.len(), 2);
    assert!(initial.iter().all(|s| [a, b].contains(&s.session.id)
        && s.session.checkout.as_ref().unwrap().recovery_revision == recovery.recovery_revision));
    assert!(
        initial.iter().all(|s| s.checkout_state.is_none()),
        "recovery facts never masquerade as live state"
    );
    let mut subscription = local.subscribe_catalog();
    observed(&local, &mut subscription, 2, |r| {
        matches!(
            r.availability,
            SourceControlAvailability::Unavailable { .. }
        )
    })
    .await;
    drop(subscription);
    drop(local);
    drop(client);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn remote_checkout_observation_is_owned_and_streamed_by_the_origin() {
    let pair = paired_servers("remote-checkout-observation").await;
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "-b", "unborn"]);
    let remote = pair
        .connecting_client
        .outlook(Outlook::Remote("workstation".into()));
    let owner = pair.serving_client.outlook(Outlook::Local);
    let mut subscription = remote.subscribe_catalog();
    create(&remote, root.path()).await;
    let initial = observed(&remote, &mut subscription, 1, |r| branch(r, "unborn")).await;
    assert!(matches!(
        initial[0].checkout_state.as_ref().unwrap().revision,
        Some(CheckoutRevision::Branch { commit: None, .. })
    ));
    git(
        root.path(),
        &["symbolic-ref", "HEAD", "refs/heads/external"],
    );
    let changed = observed(&remote, &mut subscription, 1, |r| branch(r, "external")).await;
    assert_eq!(changed, summaries(&owner).await);
    assert!(
        pair.connecting_client
            .list_sessions(None)
            .await
            .unwrap()
            .is_empty()
    );
    drop(subscription);
    drop(remote);
    drop(owner);
    pair.shutdown().await;
}

struct GatedObservation {
    git: suru::source_control::GitSourceControl,
    calls: tokio::sync::mpsc::UnboundedSender<()>,
    gate: tokio::sync::Semaphore,
}
#[async_trait::async_trait]
impl suru::source_control::SourceControl for GatedObservation {
    async fn discover(&self, path: &Path) -> suru::protocol::ResolvedWorkspace {
        self.git.discover(path).await
    }
    async fn observe(&self, checkout: &suru::protocol::CheckoutAssociation) -> CheckoutSummary {
        self.calls.send(()).unwrap();
        self.gate.acquire().await.unwrap().forget();
        self.git.observe(checkout).await
    }
}

#[tokio::test]
async fn checkout_observation_is_shared_and_stops_when_catalog_interest_ends() {
    let state = tempfile::tempdir().unwrap();
    let config_root = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "-b", "main"]);
    let (calls, mut calls_rx) = tokio::sync::mpsc::unbounded_channel();
    let adapter = std::sync::Arc::new(GatedObservation {
        git: Default::default(),
        calls,
        gate: tokio::sync::Semaphore::new(0),
    });
    let config = ServerConfig::new(state.path(), "checkout-interest")
        .unwrap()
        .with_config_dir(config_root.path());
    let (runtime, mut provider) = provider_support::ControlledProvider::new();
    let server = server::spawn_with_source_control(
        config,
        vec![runtime],
        ServerTimings {
            shutdown_grace: Duration::from_millis(5),
            ..Default::default()
        }
        .with_checkout_observation_interval(Duration::from_millis(10)),
        adapter.clone(),
    )
    .await
    .unwrap();
    for _ in 0..2 {
        let snapshot = reqwest::Client::new()
            .post(format!("{}/v1/sessions", server.descriptor().base_url))
            .bearer_auth(&server.descriptor().token)
            .json(&CreateSessionRequest {
                preparation_id: None,
                agent_selection: None,
                execution_directory: suru::protocol::ExecutionDirectory {
                    path: root.path().to_owned(),
                },
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Observe shared checkout".into(),
                    skill_invocations: vec![],
                },
            })
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json::<SessionSnapshot>()
            .await
            .unwrap();
        // Admission now retains the Repository barrier through native startup.
        // Finish that unrelated startup before admitting the next Session;
        // observation interest remains absent and must still cause no polls.
        timeout(Duration::from_secs(1), provider.next_start())
            .await
            .unwrap()
            .fail("Observation fixture has no native work");
        timeout(Duration::from_secs(1), async {
            loop {
                let current = reqwest::Client::new()
                    .get(format!(
                        "{}/v1/sessions/{}",
                        server.descriptor().base_url,
                        snapshot.session.id
                    ))
                    .bearer_auth(&server.descriptor().token)
                    .send()
                    .await
                    .unwrap()
                    .error_for_status()
                    .unwrap()
                    .json::<SessionSnapshot>()
                    .await
                    .unwrap();
                if current.session.working_since.is_none()
                    && current
                        .turns
                        .last()
                        .is_some_and(|t| t.status == suru::protocol::TurnStatus::Failed)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("controlled startup failed before observing");
    }
    assert!(
        calls_rx.try_recv().is_err(),
        "unobserved catalogs do no Git polling"
    );
    let first = ManagedClient::connect(
        ManagedClientConfig::new(state.path(), "checkout-interest").unwrap(),
    )
    .await
    .unwrap();
    let second = ManagedClient::connect(
        ManagedClientConfig::new(state.path(), "checkout-interest").unwrap(),
    )
    .await
    .unwrap();
    let local = first.outlook(Outlook::Local);
    let other = second.outlook(Outlook::Local);
    let mut one = local.subscribe_catalog();
    let mut two = other.subscribe_catalog();
    timeout(Duration::from_secs(1), one.next())
        .await
        .unwrap()
        .unwrap();
    timeout(Duration::from_secs(1), two.next())
        .await
        .unwrap()
        .unwrap();
    timeout(Duration::from_secs(1), calls_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        calls_rx.try_recv().is_err(),
        "one checkout reading for two Sessions and Clients"
    );
    adapter.gate.add_permits(1);
    observed(&local, &mut one, 2, |r| branch(r, "main")).await;
    observed(&other, &mut two, 2, |r| branch(r, "main")).await;
    // Block the next reading so dropping interest has a deterministic boundary.
    timeout(Duration::from_secs(1), calls_rx.recv())
        .await
        .unwrap()
        .unwrap();
    drop(one);
    drop(two);
    drop(local);
    drop(other);
    drop(first);
    drop(second);
    tokio::time::sleep(Duration::from_millis(20)).await;
    adapter.gate.add_permits(1);
    assert!(
        timeout(Duration::from_millis(100), calls_rx.recv())
            .await
            .is_err(),
        "no polling remains after releasing interest and its bounded in-flight read"
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn discovery_persists_branch_and_detached_recovery_before_any_catalog_interest() {
    let state = tempfile::tempdir().unwrap();
    let config_root = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let main = root.path().join("main");
    let branched = root.path().join("branched");
    let detached = root.path().join("detached");
    std::fs::create_dir(&main).unwrap();
    git(&main, &["init", "-b", "main"]);
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
    git(
        &main,
        &[
            "worktree",
            "add",
            "-b",
            "retained",
            branched.to_str().unwrap(),
        ],
    );
    git(
        &main,
        &["worktree", "add", "--detach", detached.to_str().unwrap()],
    );
    let config = ServerConfig::new(state.path(), "discovery-recovery")
        .unwrap()
        .with_config_dir(config_root.path());
    let timings = ServerTimings {
        shutdown_grace: Duration::from_millis(5),
        ..Default::default()
    }
    .with_checkout_observation_interval(Duration::from_millis(15));
    let server = server::spawn_with_timings(config.clone(), timings)
        .await
        .unwrap();
    let http = reqwest::Client::new();
    let mut remembered = Vec::new();
    for path in [&branched, &detached] {
        // No ManagedClient exists: this request opens no catalog interest, so
        // recovery must be captured by discovery itself, not a polling tick.
        let created = http
            .post(format!("{}/v1/sessions", server.descriptor().base_url))
            .bearer_auth(&server.descriptor().token)
            .json(&CreateSessionRequest {
                preparation_id: None,
                agent_selection: None,
                execution_directory: suru::protocol::ExecutionDirectory { path: path.clone() },
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Retain discovered checkout".into(),
                    skill_invocations: vec![],
                },
            })
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json::<SessionSnapshot>()
            .await
            .unwrap();
        remembered.push((created.session.id, created.session.checkout.unwrap()));
    }
    assert!(
        matches!(&remembered[0].1.recovery_revision, Some(CheckoutRevision::Branch { name, commit: Some(_) }) if name == "retained")
    );
    assert!(matches!(
        remembered[1].1.recovery_revision,
        Some(CheckoutRevision::Detached { .. })
    ));
    server.shutdown().await.unwrap();
    std::fs::remove_dir_all(&branched).unwrap();
    std::fs::remove_dir_all(&detached).unwrap();

    let server = server::spawn_with_timings(config, timings).await.unwrap();
    let initial = http
        .get(format!("{}/v1/sessions", server.descriptor().base_url))
        .bearer_auth(&server.descriptor().token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Vec<SessionListItem>>()
        .await
        .unwrap();
    assert_eq!(initial.len(), 2);
    for summary in initial.iter().map(|item| item.readable().unwrap()) {
        let (_, checkout) = remembered
            .iter()
            .find(|(id, _)| *id == summary.session.id)
            .unwrap();
        assert_eq!(summary.session.checkout.as_ref(), Some(checkout));
        assert_eq!(
            summary.checkout_state, None,
            "remembered discovery is not a fresh reading"
        );
    }
    let client = ManagedClient::connect(
        ManagedClientConfig::new(state.path(), "discovery-recovery").unwrap(),
    )
    .await
    .unwrap();
    let local = client.outlook(Outlook::Local);
    let mut subscription = local.subscribe_catalog();
    let unavailable = observed(&local, &mut subscription, 2, |reading| {
        matches!(
            reading.availability,
            SourceControlAvailability::Unavailable { .. }
        )
    })
    .await;
    for summary in unavailable {
        let (_, checkout) = remembered
            .iter()
            .find(|(id, _)| *id == summary.session.id)
            .unwrap();
        assert_eq!(summary.session.checkout.as_ref(), Some(checkout));
        assert_eq!(summary.checkout_state.unwrap().revision, None);
    }
    drop(subscription);
    drop(local);
    drop(client);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn remote_concurrent_prompts_recover_one_checkout_on_the_owning_server() {
    use suru::{
        protocol::*,
        provider::{ProviderEvent, ProviderResumeState},
    };
    let (runtime, mut provider) = provider_support::ControlledProvider::new();
    let pair = paired_servers_with_runtime("remote-shared-recovery", false, Some(runtime)).await;
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let main = root.join("main");
    let linked = root.join("external");
    std::fs::create_dir(&main).unwrap();
    git(&main, &["init", "-b", "main"]);
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
    git(
        &main,
        &["worktree", "add", "-b", "topic", linked.to_str().unwrap()],
    );
    let remote = pair
        .connecting_client
        .outlook(Outlook::Remote("workstation".into()));
    let mut catalog = remote.subscribe_catalog();
    let mut originals = Vec::new();
    let mut old_connections = Vec::new();
    let identity = || AgentIdentity {
        agent: AgentId::new("remote-recovery"),
        selection: AgentSelection {
            provider: ProviderId::new("controlled"),
            model: ModelId::new("test"),
            options: vec![],
        },
    };
    let resume = ProviderResumeState::new(serde_json::json!({"remote-context":"preserve"}));
    for _ in 0..2 {
        let id = create(&remote, &linked).await;
        let start = timeout(Duration::from_secs(2), provider.next_start())
            .await
            .unwrap();
        let mut connection = start.succeed_with_resume(identity(), Some(resume.clone()));
        timeout(Duration::from_secs(2), connection.next_turn())
            .await
            .unwrap()
            .succeed();
        connection.emit(ProviderEvent::TurnCompleted);
        let mut feed = remote.subscribe_session(id).await.unwrap();
        timeout(Duration::from_secs(2), async {
            loop {
                if remote
                    .read_session(id)
                    .await
                    .unwrap()
                    .turns
                    .iter()
                    .any(|t| t.status == TurnStatus::Completed)
                {
                    break;
                }
                feed.next().await.unwrap().unwrap();
            }
        })
        .await
        .unwrap();
        originals.push(id);
        old_connections.push(connection);
    }
    observed(&remote, &mut catalog, 2, |r| branch(r, "topic")).await;
    std::fs::remove_dir_all(&linked).unwrap();
    observed(&remote, &mut catalog, 2, |r| {
        matches!(
            r.availability,
            SourceControlAvailability::Unavailable { .. }
        )
    })
    .await;
    let request = || AdmitPromptRequest {
        prompt: InitialPrompt {
            id: PromptId::new(),
            text: "Recover remotely".into(),
            skill_invocations: vec![],
        },
        delivery: PromptDelivery::Steer,
    };
    let client_a = remote.clone();
    let id_a = originals[0];
    let a = tokio::spawn(async move { client_a.admit_prompt(id_a, request()).await });
    let client_b = remote.clone();
    let id_b = originals[1];
    let b = tokio::spawn(async move { client_b.admit_prompt(id_b, request()).await });
    let mut new_connections = Vec::new();
    for _ in 0..2 {
        let start = timeout(Duration::from_secs(3), provider.next_start())
            .await
            .unwrap();
        assert_eq!(start.execution_directory(), linked);
        assert_eq!(start.resume_state(), Some(&resume));
        let mut connection = start.succeed_with_resume(identity(), Some(resume.clone()));
        timeout(Duration::from_secs(2), connection.next_turn())
            .await
            .unwrap()
            .succeed();
        connection.emit(ProviderEvent::TurnCompleted);
        new_connections.push(connection);
    }
    a.await.unwrap().unwrap();
    b.await.unwrap().unwrap();
    observed(&remote, &mut catalog, 2, |r| branch(r, "topic")).await;
    assert!(
        pair.connecting_client
            .list_sessions(None)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(remote.list_sessions(None).await.unwrap().len(), 2);
    drop(catalog);
    drop(remote);
    pair.shutdown().await;
}

#[tokio::test]
async fn remote_removal_routes_to_owner_counts_its_catalog_and_streams_missing() {
    use suru::{
        protocol::*,
        source_control::{GitSourceControl, SourceControl},
    };
    let (runtime, mut provider) = provider_support::ControlledProvider::new();
    let pair = paired_servers_with_runtime("remote-removal", false, Some(runtime)).await;
    let temp = tempfile::tempdir().unwrap();
    let main = std::fs::canonicalize(temp.path()).unwrap();
    git(&main, &["init", "-b", "main"]);
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
    let linked = main.join("linked");
    git(
        &main,
        &["worktree", "add", "-b", "topic", linked.to_str().unwrap()],
    );
    let remote = pair
        .connecting_client
        .outlook(Outlook::Remote("workstation".into()));
    let local = pair.connecting_client.outlook(Outlook::Local);
    let mut subscription = remote.subscribe_catalog();
    let id = create(&remote, &linked).await;
    let start = timeout(Duration::from_secs(2), provider.next_start())
        .await
        .unwrap();
    let mut connection = start.succeed(AgentIdentity {
        agent: AgentId::new("removal"),
        selection: AgentSelection {
            provider: ProviderId::new("controlled"),
            model: ModelId::new("test"),
            options: vec![],
        },
    });
    timeout(Duration::from_secs(2), connection.next_turn())
        .await
        .unwrap()
        .succeed();
    connection
        .emit_and_wait_until_observed(suru::provider::ProviderEvent::TurnCompleted)
        .await;
    timeout(Duration::from_secs(2), async { loop { if remote.list_sessions(None).await.unwrap().iter().any(|s| matches!(s, SessionListItem::Readable(s) if s.session.id == id && s.session.working_since.is_none())) { break; } tokio::time::sleep(Duration::from_millis(5)).await; } }).await.unwrap();
    create(&local, &linked).await;
    observed(&remote, &mut subscription, 1, |r| branch(r, "topic")).await;
    let location = GitSourceControl::default().discover(&linked).await;
    let target = CheckoutRemovalTarget {
        repository: location.workspace.repository.unwrap(),
        checkout: location.checkout.unwrap(),
    };
    let facts = remote.preview_checkout_removal(target).await.unwrap();
    assert_eq!(
        facts.affected_sessions, 1,
        "does not count connecting Server Session"
    );
    let result = remote
        .remove_checkout(RemoveCheckoutRequest {
            preview: facts,
            force: false,
        })
        .await
        .unwrap();
    assert!(result.removed, "{:?}", result.error);
    observed(&remote, &mut subscription, 1, |r| {
        matches!(
            r.availability,
            SourceControlAvailability::Unavailable { .. }
        )
    })
    .await;
    assert_eq!(summaries(&local).await.len(), 1);
    assert!(!linked.exists());
    drop(subscription);
    drop(remote);
    drop(local);
    pair.shutdown().await;
}
