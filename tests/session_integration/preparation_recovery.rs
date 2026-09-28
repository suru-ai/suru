//! Reconciliation is exercised through API outcomes and real Git resources.
use super::*;
use crate::server_support::PROGRESS_DEADLINE;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use suru::source_control::{GitSourceControl, PreparationCheckpoint as Point, PreparationObserver};
use tokio::time::{Duration, timeout};

struct Fault {
    at: Point,
    fired: AtomicBool,
    observed: Mutex<Option<PreparedCheckout>>,
    reached: tokio::sync::Notify,
    release: Option<tokio::sync::Semaphore>,
}
impl Fault {
    fn once(at: Point) -> Arc<Self> {
        Arc::new(Self {
            at,
            fired: AtomicBool::new(false),
            observed: Mutex::new(None),
            reached: Default::default(),
            release: None,
        })
    }
}
#[async_trait::async_trait]
impl PreparationObserver for Fault {
    async fn checkpoint(&self, at: Point, plan: &PreparedCheckout) -> Result<(), String> {
        if self.at == at && !self.fired.swap(true, Ordering::SeqCst) {
            *self.observed.lock().unwrap() = Some(plan.clone());
            self.reached.notify_one();
            if let Some(release) = &self.release {
                release.acquire().await.unwrap().forget();
            }
            return Err(format!("Interrupted at {at:?}"));
        }
        Ok(())
    }
}
fn timings() -> server::ServerTimings {
    server::ServerTimings {
        shutdown_grace: Duration::from_millis(5),
        checkout_skill_timeout: Duration::from_secs(2),
        ..Default::default()
    }
}
async fn create_response(
    descriptor: &RuntimeDescriptor,
    request: &CreateSessionRequest,
) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(request)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn restart_reconciles_each_git_preparation_window_and_duplicate_edited_retries() {
    for point in [
        Point::IntentPersisted,
        Point::BranchCreated,
        Point::RegistrationCreated,
        Point::CheckoutCreated,
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = suru::paths::canonical(temp.path()).unwrap();
        let main = root.join("main");
        committed(&main);
        let config = ServerConfig::new(root.join("state"), "restart-preparation").unwrap();
        let (runtime, mut provider) = ControlledProvider::new();
        let fault = Fault::once(point);
        let server = server::spawn_with_source_control(
            config.clone(),
            vec![runtime.clone()],
            timings(),
            Arc::new(GitSourceControl::default().with_preparation_observer(fault)),
        )
        .await
        .unwrap();
        let mut request = request(&main, "First description");
        let failed = prepare(server.descriptor(), &request).await;
        assert!(
            failed
                .error
                .as_ref()
                .is_some_and(|error| error.contains("Interrupted")),
            "{point:?}: {:?}",
            failed.error
        );
        let captured = failed.preparation.clone();
        assert!(provider.try_next_start().is_none());
        if matches!(point, Point::CheckoutCreated | Point::RegistrationCreated) {
            let listed = reqwest::Client::new()
                .post(format!(
                    "{}/v1/workspaces/resolve",
                    server.descriptor().base_url
                ))
                .bearer_auth(&server.descriptor().token)
                .json(&ResolveWorkspaceRequest {
                    workspace_id: None,
                    checkout_id: None,
                    remembered_execution_directory: None,
                    base: None,
                    path: main.clone(),
                })
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json::<ResolvedWorkspace>()
                .await
                .unwrap();
            assert!(
                listed
                    .checkouts
                    .iter()
                    .any(|checkout| checkout.association.root == captured.destination.path),
                "created checkout remains discoverable"
            );
        }
        server.shutdown().await.unwrap();
        git(
            &main,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-m",
                "source moved",
            ],
        );
        if point == Point::BranchCreated {
            let CheckoutPreparationPlan::Git { branch, .. } = &captured.plan;
            git(&main, &["branch", "-D", branch]); // Missing own branch is recreated from its atomic claim.
        }
        if point == Point::CheckoutCreated {
            std::fs::remove_dir_all(&captured.destination.path).unwrap(); // Own stale registration, not global prune.
        }
        request.prompt.text = "Edited draft after replacement".into();
        let server = server::spawn_with_source_control(
            config,
            vec![runtime],
            timings(),
            Arc::new(GitSourceControl::default()),
        )
        .await
        .unwrap();
        let (one, two) = tokio::join!(
            prepare(server.descriptor(), &request),
            prepare(server.descriptor(), &request)
        );
        assert_eq!(one.error, None, "{point:?}");
        assert_eq!(two.error, None, "{point:?}");
        assert_eq!(one.preparation.id, captured.id);
        assert_eq!(two.preparation.destination, captured.destination);
        assert_eq!(one.preparation.plan, captured.plan);
        assert_eq!(one.preparation.intended_session, captured.intended_session);
        let CheckoutPreparationPlan::Git { source_commit, .. } = &captured.plan;
        assert_eq!(
            &read_git(&captured.destination.path, &["rev-parse", "HEAD"]),
            source_commit
        );
        assert_eq!(
            std::fs::read_to_string(captured.destination.path.join("tracked")).unwrap(),
            "committed\n"
        );
        let a = creation(&one.preparation, "Admitted once");
        let b = a.clone();
        let (a, b) = tokio::join!(
            create_response(server.descriptor(), &a),
            create_response(server.descriptor(), &b)
        );
        let a = a
            .error_for_status()
            .unwrap()
            .json::<SessionSnapshot>()
            .await
            .unwrap();
        let b = b
            .error_for_status()
            .unwrap()
            .json::<SessionSnapshot>()
            .await
            .unwrap();
        assert_eq!(a.session.id, captured.intended_session);
        assert_eq!(a.session.id, b.session.id);
        let start = timeout(PROGRESS_DEADLINE, provider.next_start())
            .await
            .expect("one Provider startup");
        assert_eq!(start.execution_directory(), captured.destination.path);
        drop(start);
        assert!(provider.try_next_start().is_none());
        server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn an_unfinished_intention_keeps_its_name_from_a_later_preparation() {
    let temp = tempfile::tempdir().unwrap();
    let root = suru::paths::canonical(temp.path()).unwrap();
    let main = root.join("main");
    committed(&main);
    let (runtime, _provider) = ControlledProvider::new();
    let server = server::spawn_with_source_control(
        ServerConfig::new(root.join("state"), "held-name").unwrap(),
        vec![runtime],
        timings(),
        Arc::new(
            GitSourceControl::default()
                .with_preparation_observer(Fault::once(Point::IntentPersisted)),
        ),
    )
    .await
    .unwrap();
    // Interrupted before its branch or location exist, the first intention
    // holds its name only through its stored plan.
    let first = request(&main, "Fix the parser");
    let interrupted = prepare(server.descriptor(), &first).await;
    assert!(interrupted.error.is_some());
    let container = main.join(".suru-worktrees");
    assert_eq!(
        interrupted.preparation.destination.path,
        container.join("fix-parser")
    );
    assert!(!interrupted.preparation.destination.path.exists());

    let later = prepare(server.descriptor(), &request(&main, "Fix the parser")).await;
    assert_eq!(later.error, None);
    assert_eq!(branch(&later.preparation), "suru/fix-parser-2");
    assert_eq!(
        later.preparation.destination.path,
        container.join("fix-parser-2")
    );

    let retried = prepare(server.descriptor(), &first).await;
    assert_eq!(retried.error, None);
    assert_eq!(retried.preparation.plan, interrupted.preparation.plan);
    assert_eq!(branch(&retried.preparation), "suru/fix-parser");
    assert_eq!(
        read_git(
            &retried.preparation.destination.path,
            &["symbolic-ref", "--short", "HEAD"]
        ),
        "suru/fix-parser"
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn interrupted_request_and_server_replacement_keep_the_same_preparation_identity() {
    let temp = tempfile::tempdir().unwrap();
    let root = suru::paths::canonical(temp.path()).unwrap();
    let main = root.join("main");
    committed(&main);
    let config = ServerConfig::new(root.join("state"), "interrupted-client").unwrap();
    let (runtime, mut provider) = ControlledProvider::new();
    let fault = Arc::new(Fault {
        at: Point::BranchCreated,
        fired: AtomicBool::new(false),
        observed: Mutex::new(None),
        reached: Default::default(),
        release: Some(tokio::sync::Semaphore::new(0)),
    });
    let server = server::spawn_with_source_control(
        config.clone(),
        vec![runtime.clone()],
        timings(),
        Arc::new(GitSourceControl::default().with_preparation_observer(fault.clone())),
    )
    .await
    .unwrap();
    let request = request(&main, "Interrupted client");
    let descriptor = server.descriptor().clone();
    let sent = request.clone();
    let task = tokio::spawn(async move { prepare(&descriptor, &sent).await });
    timeout(PROGRESS_DEADLINE, fault.reached.notified())
        .await
        .unwrap();
    let captured = fault.observed.lock().unwrap().clone().unwrap();
    task.abort();
    fault.release.as_ref().unwrap().add_permits(1);
    server.shutdown().await.unwrap();
    assert!(provider.try_next_start().is_none());
    let server = server::spawn_with_source_control(
        config,
        vec![runtime],
        timings(),
        Arc::new(GitSourceControl::default()),
    )
    .await
    .unwrap();
    let retry = prepare(server.descriptor(), &request).await;
    assert_eq!(retry.error, None);
    assert_eq!(retry.preparation.destination, captured.destination);
    assert_eq!(retry.preparation.plan, captured.plan);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn persisted_initial_session_shell_resumes_original_prompt_once_after_restart() {
    let temp = tempfile::tempdir().unwrap();
    let root = suru::paths::canonical(temp.path()).unwrap();
    let main = root.join("main");
    committed(&main);
    let config = ServerConfig::new(root.join("state"), "pending-admission").unwrap();
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_source_control(
        config.clone(),
        vec![runtime.clone()],
        timings(),
        Arc::new(
            GitSourceControl::default()
                .with_preparation_observer(Fault::once(Point::SessionPersisted)),
        ),
    )
    .await
    .unwrap();
    let unrelated = support::create_session(
        server.descriptor(),
        &CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: ExecutionDirectory { path: root.clone() },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Unrelated history".into(),
                skill_invocations: vec![],
                attachments: Vec::new(),
            },
        },
    )
    .await;
    drop(
        timeout(PROGRESS_DEADLINE, provider.next_start())
            .await
            .unwrap(),
    );
    support::read_session_until(
        &reqwest::Client::new(),
        server.descriptor(),
        unrelated.session.id,
        "unrelated Turn settles",
        |s| {
            s.turns
                .first()
                .is_some_and(|t| t.status == TurnStatus::Failed)
        },
    )
    .await;
    let request = request(&main, "Original draft");
    let prepared = prepare(server.descriptor(), &request).await;
    assert_eq!(prepared.error, None);
    let original_catalog = crate::provider_support::fixture_skill_catalog(
        ProviderId::new("controlled"),
        prepared.preparation.destination.path.clone(),
    );
    runtime.offer_skills(original_catalog.clone());
    let mut original = creation(&prepared.preparation, "$review Original admitted text");
    let skill = &original_catalog.skills[0];
    original.prompt.skill_invocations = vec![SkillInvocation {
        skill_id: skill.id.clone(),
        name: skill.name.clone(),
        scope: skill.scope.clone(),
        span: TextSpan { start: 0, end: 7 },
    }];
    let response = create_response(server.descriptor(), &original).await;
    assert!(!response.status().is_success());
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("Interrupted at SessionPersisted")
    );
    assert!(provider.try_next_start().is_none());
    server.shutdown().await.unwrap();
    {
        use diesel::{Connection, connection::SimpleConnection};
        let mut database = diesel::SqliteConnection::establish(
            config.data_dir().join("suru.db").to_str().unwrap(),
        )
        .unwrap();
        database
            .batch_execute(&format!(
                "UPDATE prompts SET payload = '{{invalid history' WHERE session_id = '{}';",
                unrelated.session.id
            ))
            .unwrap();
    }

    let server = server::spawn_with_source_control(
        config.clone(),
        vec![runtime.clone()],
        timings(),
        Arc::new(GitSourceControl::default()),
    )
    .await
    .unwrap();
    runtime.fail_skill_discovery("temporarily unavailable destination Skills");
    let failed = prepare(server.descriptor(), &request).await;
    assert!(failed.error.is_some());
    assert!(provider.try_next_start().is_none());
    runtime.clear_skill_discovery_failure();
    let mut missing_catalog = original_catalog.clone();
    missing_catalog.skills.clear();
    runtime.offer_skills(missing_catalog);
    let missing = prepare(server.descriptor(), &request).await;
    assert!(
        missing
            .error
            .as_ref()
            .is_some_and(|error| error.contains("destination Skills"))
    );
    assert!(
        provider.try_next_start().is_none(),
        "a fresh catalog missing the admitted Skill cannot start the Provider"
    );
    runtime.offer_skills(original_catalog);
    let one = prepare(server.descriptor(), &request).await;
    assert_eq!(one.error, None);
    assert_eq!(
        one.preparation.admitted_session,
        Some(prepared.preparation.intended_session)
    );
    let rejoined = support::create_session(server.descriptor(), &original).await;
    let listed = reqwest::Client::new()
        .get(format!("{}/v1/sessions", server.descriptor().base_url))
        .bearer_auth(&server.descriptor().token)
        .send()
        .await
        .unwrap()
        .json::<Vec<SessionListItem>>()
        .await
        .unwrap();
    assert_eq!(listed.len(), 2);
    assert!(
        listed.iter().all(|item| item.readable().is_some()),
        "retry does not hydrate unrelated malformed history"
    );
    assert_eq!(rejoined.prompts.len(), 1);
    assert_eq!(rejoined.prompts[0].id, original.prompt.id);
    assert_eq!(rejoined.prompts[0].text, original.prompt.text);
    let start = timeout(PROGRESS_DEADLINE, provider.next_start())
        .await
        .expect("one Provider startup");
    assert_eq!(
        start.execution_directory(),
        one.preparation.destination.path
    );
    drop(start);
    let settled = support::read_session_until(
        &reqwest::Client::new(),
        server.descriptor(),
        rejoined.session.id,
        "original delivery settles",
        |s| !s.turns.is_empty() && s.turns[0].status == TurnStatus::Failed,
    )
    .await;
    assert_eq!(settled.prompts.len(), 1);
    assert!(provider.try_next_start().is_none());
    server.shutdown().await.unwrap();
    let server = server::spawn_with_source_control(
        config,
        vec![runtime],
        timings(),
        Arc::new(GitSourceControl::default()),
    )
    .await
    .unwrap();
    let rejoined = support::create_session(server.descriptor(), &original).await;
    assert_eq!(rejoined.session.id, settled.session.id);
    assert!(provider.try_next_start().is_none());
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn ambiguous_checkout_proof_external_locks_and_deleted_ready_branches_are_not_overwritten() {
    let temp = tempfile::tempdir().unwrap();
    let root = suru::paths::canonical(temp.path()).unwrap();
    let main = root.join("main");
    committed(&main);
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_source_control(
        ServerConfig::new(root.join("state"), "ownership").unwrap(),
        vec![runtime],
        timings(),
        Arc::new(
            GitSourceControl::default()
                .with_preparation_observer(Fault::once(Point::RegistrationCreated)),
        ),
    )
    .await
    .unwrap();
    let request = request(&main, "Owned partial");
    let partial = prepare(server.descriptor(), &request).await;
    assert!(partial.error.is_some());
    let destination = &partial.preparation.destination.path;
    let metadata =
        std::path::PathBuf::from(read_git(destination, &["rev-parse", "--absolute-git-dir"]));
    let own_lock = std::fs::read(metadata.join("locked")).unwrap();
    std::fs::write(destination.join("keep"), "user file").unwrap();
    std::fs::write(metadata.join("suru-preparation"), "another intention").unwrap();
    let conflict = prepare(server.descriptor(), &request).await;
    assert!(conflict.error.unwrap().contains("marker conflicts"));
    std::fs::remove_file(metadata.join("suru-preparation")).unwrap();
    std::fs::write(metadata.join("locked"), "external maintenance").unwrap();
    let conflict = prepare(server.descriptor(), &request).await;
    assert!(conflict.error.unwrap().contains("external lock"));
    assert_eq!(
        std::fs::read_to_string(destination.join("keep")).unwrap(),
        "user file"
    );
    std::fs::write(metadata.join("locked"), own_lock).unwrap();
    let ready = prepare(server.descriptor(), &request).await;
    assert_eq!(ready.error, None);
    assert_eq!(
        std::fs::read_to_string(destination.join("keep")).unwrap(),
        "user file"
    );
    let CheckoutPreparationPlan::Git { branch, .. } = &ready.preparation.plan;
    git(
        &main,
        &["update-ref", "-d", &format!("refs/heads/{branch}")],
    );
    let failed = prepare(server.descriptor(), &request).await;
    assert!(failed.error.unwrap().contains("HEAD is unavailable"));
    assert!(provider.try_next_start().is_none());
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn matching_branch_commit_and_destination_do_not_prove_external_checkout_ownership() {
    let temp = tempfile::tempdir().unwrap();
    let root = suru::paths::canonical(temp.path()).unwrap();
    let main = root.join("main");
    committed(&main);
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_source_control(
        ServerConfig::new(root.join("state"), "replaced-checkout").unwrap(),
        vec![runtime],
        timings(),
        Arc::new(GitSourceControl::default()),
    )
    .await
    .unwrap();
    let request = request(&main, "Known checkout");
    let ready = prepare(server.descriptor(), &request).await;
    assert_eq!(ready.error, None);
    let destination = &ready.preparation.destination.path;
    let CheckoutPreparationPlan::Git {
        branch,
        source_commit,
        ..
    } = &ready.preparation.plan;
    git(
        &main,
        &["worktree", "remove", destination.to_str().unwrap()],
    );
    git(
        &main,
        &["worktree", "add", destination.to_str().unwrap(), branch],
    );
    std::fs::write(destination.join("keep"), "external work").unwrap();
    assert_eq!(
        &read_git(destination, &["rev-parse", "HEAD"]),
        source_commit
    );
    let failed = prepare(server.descriptor(), &request).await;
    assert!(failed.error.unwrap().contains("proven preparation"));
    assert_eq!(
        std::fs::read_to_string(destination.join("keep")).unwrap(),
        "external work"
    );
    let another = root.join("another");
    committed(&another);
    let mut conflict = request.clone();
    conflict.source.path = another;
    let response = reqwest::Client::new()
        .post(format!(
            "{}/v1/checkouts/prepare",
            server.descriptor().base_url
        ))
        .bearer_auth(&server.descriptor().token)
        .json(&conflict)
        .send()
        .await
        .unwrap();
    assert!(!response.status().is_success());
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("another execution location")
    );
    assert!(provider.try_next_start().is_none());
    server.shutdown().await.unwrap();
}

struct FailMetadataSave {
    data: std::path::PathBuf,
    fired: AtomicBool,
}
#[async_trait::async_trait]
impl PreparationObserver for FailMetadataSave {
    async fn checkpoint(&self, at: Point, _: &PreparedCheckout) -> Result<(), String> {
        if at == Point::CheckoutCreated && !self.fired.swap(true, Ordering::SeqCst) {
            // Make the actual atomic store write fail, preserving its previous
            // durable intent. Assertions below concern only public API/Git facts.
            std::fs::rename(
                self.data.join("checkout-preparations"),
                self.data.join("retained-intents"),
            )
            .unwrap();
            std::fs::write(
                self.data.join("checkout-preparations"),
                "temporarily unavailable storage",
            )
            .unwrap();
        }
        Ok(())
    }
}
#[tokio::test]
async fn real_metadata_write_failure_after_git_success_recovers_same_checkout_on_restart() {
    let temp = tempfile::tempdir().unwrap();
    let root = suru::paths::canonical(temp.path()).unwrap();
    let main = root.join("main");
    committed(&main);
    let config = ServerConfig::new(root.join("state"), "metadata-failure").unwrap();
    let data = config.data_dir().to_owned();
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_source_control(
        config.clone(),
        vec![runtime.clone()],
        timings(),
        Arc::new(
            GitSourceControl::default().with_preparation_observer(Arc::new(FailMetadataSave {
                data: data.clone(),
                fired: AtomicBool::new(false),
            })),
        ),
    )
    .await
    .unwrap();
    let request = request(&main, "Persist Git progress");
    let failed = prepare(server.descriptor(), &request).await;
    assert!(failed.error.as_ref().unwrap().contains("persist"));
    assert!(
        failed
            .preparation
            .destination
            .path
            .join("tracked")
            .is_file()
    );
    assert!(provider.try_next_start().is_none());
    std::fs::remove_file(data.join("checkout-preparations")).unwrap();
    std::fs::rename(
        data.join("retained-intents"),
        data.join("checkout-preparations"),
    )
    .unwrap();
    server.shutdown().await.unwrap();
    let server = server::spawn_with_source_control(
        config,
        vec![runtime],
        timings(),
        Arc::new(GitSourceControl::default()),
    )
    .await
    .unwrap();
    let retry = prepare(server.descriptor(), &request).await;
    assert_eq!(retry.error, None);
    assert_eq!(
        retry.preparation.destination,
        failed.preparation.destination
    );
    assert_eq!(retry.preparation.plan, failed.preparation.plan);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn lost_admission_response_rejoins_delivered_work_without_another_provider_turn() {
    let temp = tempfile::tempdir().unwrap();
    let root = suru::paths::canonical(temp.path()).unwrap();
    let main = root.join("main");
    committed(&main);
    let config = ServerConfig::new(root.join("state"), "lost-admission").unwrap();
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_source_control(
        config.clone(),
        vec![runtime.clone()],
        timings(),
        Arc::new(
            GitSourceControl::default().with_preparation_observer(Fault::once(Point::Admitted)),
        ),
    )
    .await
    .unwrap();
    let request = request(&main, "Once");
    let ready = prepare(server.descriptor(), &request).await;
    let initial = creation(&ready.preparation, "Deliver this exactly once");
    let response = create_response(server.descriptor(), &initial).await;
    assert!(!response.status().is_success());
    let mut running = timeout(PROGRESS_DEADLINE, provider.next_start())
        .await
        .expect("one Provider startup")
        .succeed(AgentIdentity {
            agent: AgentId::new("controlled"),
            selection: AgentSelection {
                provider: ProviderId::new("controlled"),
                model: ModelId::new("test"),
                options: vec![],
            },
        });
    let turn = timeout(PROGRESS_DEADLINE, running.next_turn())
        .await
        .expect("one Provider Turn");
    assert_eq!(turn.prompt(), initial.prompt.text);
    turn.fail("fixture settled");
    let settled = support::read_session_until(
        &reqwest::Client::new(),
        server.descriptor(),
        ready.preparation.intended_session,
        "settled initial Turn",
        |s| {
            s.turns
                .first()
                .is_some_and(|t| t.status == TurnStatus::Failed)
        },
    )
    .await;
    assert_eq!(settled.prompts.len(), 1);
    let duplicate = support::create_session(server.descriptor(), &initial).await;
    assert_eq!(duplicate.session.id, settled.session.id);
    assert_eq!(duplicate.prompts.len(), 1);
    assert!(
        timeout(Duration::from_millis(30), running.next_turn())
            .await
            .is_err()
    );
    drop(running);
    server.shutdown().await.unwrap();
    let server = server::spawn_with_source_control(
        config,
        vec![runtime],
        timings(),
        Arc::new(GitSourceControl::default()),
    )
    .await
    .unwrap();
    let retry = support::create_session(server.descriptor(), &initial).await;
    assert_eq!(retry.session.id, settled.session.id);
    assert!(provider.try_next_start().is_none());
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn restart_reuses_checkout_without_initializing_an_unavailable_submodule() {
    let temp = tempfile::tempdir().unwrap();
    let root = suru::paths::canonical(temp.path()).unwrap();
    let main = root.join("main");
    let module = root.join("module-source");
    committed(&main);
    committed(&module);
    git(
        &main,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            module.to_str().unwrap(),
            "module",
        ],
    );
    git(
        &main,
        &["-c", "commit.gpgsign=false", "commit", "-am", "submodule"],
    );
    let git_config = root.join("git-config");
    std::fs::write(&git_config, "[protocol \"file\"]\nallow = always\n").unwrap();
    let saved_module = root.join("temporarily-unavailable-module");
    std::fs::rename(&module, &saved_module).unwrap();
    let config = ServerConfig::new(root.join("state"), "module-restart").unwrap();
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_source_control(
        config.clone(),
        vec![runtime.clone()],
        timings(),
        Arc::new(GitSourceControl::default().with_configuration_file(&git_config)),
    )
    .await
    .unwrap();
    let request = request(&main, "Leave modules uninitialized");
    let first = prepare(server.descriptor(), &request).await;
    assert_eq!(first.error, None);
    assert!(first.preparation.checkout_created);
    assert!(first.preparation.destination.path.join("tracked").exists());
    assert!(
        !first
            .preparation
            .destination
            .path
            .join("module/.git")
            .exists()
    );
    assert!(provider.try_next_start().is_none());
    server.shutdown().await.unwrap();
    let server = server::spawn_with_source_control(
        config,
        vec![runtime],
        timings(),
        Arc::new(GitSourceControl::default().with_configuration_file(&git_config)),
    )
    .await
    .unwrap();
    let retry = prepare(server.descriptor(), &request).await;
    assert_eq!(retry.error, None);
    assert_eq!(retry.preparation.destination, first.preparation.destination);
    assert_eq!(retry.preparation.plan, first.preparation.plan);
    assert!(
        !retry
            .preparation
            .destination
            .path
            .join("module/.git")
            .exists()
    );
    support::create_session(
        server.descriptor(),
        &creation(&retry.preparation, "Now ready"),
    )
    .await;
    drop(
        timeout(PROGRESS_DEADLINE, provider.next_start())
            .await
            .unwrap(),
    );
    assert!(provider.try_next_start().is_none());
    std::fs::rename(saved_module, &module).unwrap();
    server.shutdown().await.unwrap();
}
