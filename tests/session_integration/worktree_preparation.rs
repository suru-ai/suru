//! Managed preparation through authenticated owning-Server operations and real Git.
use crate::{provider_support::ControlledProvider, repositories::git, support};
use std::path::Path;
use suru::{
    protocol::*,
    server::{self, ServerConfig},
};

fn committed(root: &Path) {
    std::fs::create_dir_all(root).unwrap();
    git(root, &["init", "-b", "main"]);
    std::fs::write(root.join("tracked"), "committed\n").unwrap();
    git(root, &["add", "."]);
    git(
        root,
        &["-c", "commit.gpgsign=false", "commit", "-m", "initial"],
    );
}
fn read_git(root: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}
async fn prepare(
    descriptor: &RuntimeDescriptor,
    request: &PrepareCheckoutRequest,
) -> PrepareCheckoutResult {
    let response = reqwest::Client::new()
        .post(format!("{}/v1/checkouts/prepare", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(request)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert!(status.is_success(), "{status}: {body}");
    serde_json::from_str(&body).unwrap()
}
fn request(root: &Path, description: &str) -> PrepareCheckoutRequest {
    PrepareCheckoutRequest {
        id: Default::default(),
        source: ExecutionDirectory {
            path: root.to_owned(),
        },
        description: description.to_owned(),
        provider: ProviderId::new("controlled"),
    }
}
fn creation(preparation: &PreparedCheckout, text: &str) -> CreateSessionRequest {
    CreateSessionRequest {
        preparation_id: Some(preparation.id),
        agent_selection: None,
        execution_directory: preparation.destination.clone(),
        prompt: InitialPrompt {
            id: PromptId::new(),
            text: text.to_owned(),
            skill_invocations: vec![],
        },
    }
}

#[tokio::test]
async fn managed_preparation_captures_local_commit_once_and_reuses_checkout_and_admitted_session() {
    let temp = tempfile::tempdir().unwrap();
    let root = suru::paths::canonical(temp.path()).unwrap();
    let main = root.join("repo");
    committed(&main);
    let linked = root.join("linked");
    git(
        &main,
        &["worktree", "add", "-b", "source", linked.to_str().unwrap()],
    );
    let subdir = linked.join("nested");
    std::fs::create_dir(&subdir).unwrap();
    std::fs::write(main.join(".git/info/exclude"), "existing-no-newline").unwrap();
    std::fs::write(linked.join("tracked"), "uncommitted").unwrap();
    std::fs::write(linked.join("local-only"), "not copied").unwrap();
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(
        ServerConfig::new(root.join("state"), "preview.1").unwrap(),
        runtime,
    )
    .await
    .unwrap();
    let mut request = request(&subdir, "Fix a locally described change");
    // Intent is just a request value; no branch or container exists yet.
    assert!(!main.join(".suru-worktrees").exists());
    git(
        &linked,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "new source tip",
        ],
    );
    let source = read_git(&linked, &["rev-parse", "HEAD"]);
    let result = prepare(server.descriptor(), &request).await;
    assert_eq!(result.error, None);
    let destination = result.preparation.destination.path.clone();
    assert!(destination.starts_with(main.join(".suru-worktrees")));
    assert_eq!(read_git(&destination, &["rev-parse", "HEAD"]), source);
    assert_eq!(
        std::fs::read_to_string(destination.join("tracked")).unwrap(),
        "committed\n"
    );
    assert!(!destination.join("local-only").exists());
    assert_eq!(
        std::fs::read_to_string(main.join(".git/info/exclude")).unwrap(),
        "existing-no-newline\n/.suru-worktrees/\n"
    );
    assert_eq!(read_git(&main, &["status", "--porcelain"]), "");
    std::fs::write(destination.join("new-file"), "visible").unwrap();
    assert!(read_git(&destination, &["status", "--porcelain"]).contains("new-file"));
    git(&main, &["clean", "-fd"]);
    assert!(destination.join("new-file").exists());
    git(
        &linked,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "later tip",
        ],
    );
    request.description = "Corrected draft is not another checkout".to_owned();
    let (one, two) = tokio::join!(
        prepare(server.descriptor(), &request),
        prepare(server.descriptor(), &request)
    );
    assert_eq!(one.preparation.destination, result.preparation.destination);
    assert_eq!(two.preparation.plan, result.preparation.plan);
    assert_eq!(read_git(&destination, &["rev-parse", "HEAD"]), source);
    let created = support::create_session(
        server.descriptor(),
        &creation(&one.preparation, "corrected work"),
    )
    .await;
    let startup = provider.next_start().await;
    assert_eq!(startup.execution_directory(), destination);
    drop(startup);
    let duplicate = support::create_session(
        server.descriptor(),
        &creation(&two.preparation, "different Prompt ID"),
    )
    .await;
    assert_eq!(created.session.id, duplicate.session.id);
    let acknowledged = prepare(server.descriptor(), &request).await;
    assert_eq!(acknowledged.error, None);
    assert_eq!(
        acknowledged.preparation.admitted_session,
        Some(created.session.id)
    );
    assert!(provider.try_next_start().is_none());
    let failed = support::read_session_until(
        &reqwest::Client::new(),
        server.descriptor(),
        created.session.id,
        "post-admission Provider failure",
        |snapshot| {
            snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.status == TurnStatus::Failed)
        },
    )
    .await;
    assert_eq!(failed.session.id, created.session.id);
    assert!(destination.is_dir());
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn naming_bare_storage_and_destination_validation_preserve_existing_resources() {
    let temp = tempfile::tempdir().unwrap();
    let root = suru::paths::canonical(temp.path()).unwrap();
    let main = root.join("repo");
    committed(&main);
    let bare = root.join("bare");
    git(
        &root,
        &[
            "clone",
            "--bare",
            main.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(
        ServerConfig::new(root.join("state"), "naming").unwrap(),
        runtime,
    )
    .await
    .unwrap();
    let mut destinations = std::collections::HashSet::new();
    for description in [
        "👩🏽‍💻 / ... ~ @{}",
        &"A_very.Long~name / ".repeat(100),
        "same",
        "same",
    ] {
        let result = prepare(server.descriptor(), &request(&bare, description)).await;
        assert_eq!(result.error, None);
        let CheckoutPreparationPlan::Git { branch, .. } = &result.preparation.plan;
        assert!(branch.starts_with("suru/"));
        assert!(branch.len() < 64);
        git(&bare, &["check-ref-format", branch]);
        assert!(
            result
                .preparation
                .destination
                .path
                .starts_with(bare.join(".suru-worktrees"))
        );
        assert!(destinations.insert(result.preparation.destination.path.clone()));
    }
    let intent = request(&main, "validation");
    let result = prepare(server.descriptor(), &intent).await;
    let destination = result.preparation.destination.path.clone();
    git(
        &main,
        &["worktree", "remove", destination.to_str().unwrap()],
    );
    std::fs::create_dir(&destination).unwrap();
    std::fs::write(destination.join("keep"), "unrelated").unwrap();
    let retry = prepare(server.descriptor(), &intent).await;
    assert!(retry.error.unwrap().contains("unrelated"));
    assert_eq!(
        std::fs::read_to_string(destination.join("keep")).unwrap(),
        "unrelated"
    );
    assert!(provider.try_next_start().is_none());
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn recursive_local_submodules_and_destination_skills_finish_before_startup_and_retry_reuses_progress()
 {
    let temp = tempfile::tempdir().unwrap();
    let root = suru::paths::canonical(temp.path()).unwrap();
    let leaf = root.join("leaf");
    committed(&leaf);
    let middle = root.join("middle");
    committed(&middle);
    git(
        &middle,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            leaf.to_str().unwrap(),
            "nested",
        ],
    );
    git(
        &middle,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-am",
            "nested module",
        ],
    );
    let main = root.join("main");
    committed(&main);
    git(
        &main,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            middle.to_str().unwrap(),
            "module",
        ],
    );
    git(
        &main,
        &["-c", "commit.gpgsign=false", "commit", "-am", "module"],
    );
    // This isolated Git user configuration permits only this fixture's local transport.
    let config = root.join("git-config");
    std::fs::write(&config, "[protocol \"file\"]\nallow = always\n").unwrap();
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_source_control(
        ServerConfig::new(root.join("state"), "submodules").unwrap(),
        vec![runtime.clone()],
        Default::default(),
        std::sync::Arc::new(
            suru::source_control::GitSourceControl::default().with_configuration_file(&config),
        ),
    )
    .await
    .unwrap();
    // A missing local source makes initialization fail after Worktree creation.
    let hidden = root.join("middle-hidden");
    std::fs::rename(&middle, &hidden).unwrap();
    let mut intent = request(&main, "Initialize recursive modules");
    let failed = prepare(server.descriptor(), &intent).await;
    assert!(
        failed
            .error
            .as_ref()
            .unwrap()
            .contains("Submodule initialization failed")
    );
    assert!(failed.preparation.checkout_created);
    assert!(!failed.preparation.ready);
    assert!(failed.location.is_some());
    assert!(provider.try_next_start().is_none());
    std::fs::rename(hidden, &middle).unwrap();
    runtime.fail_skill_discovery("fixture destination Skills unavailable");
    intent.description = "Edited draft after creation".to_owned();
    let skills_failed = prepare(server.descriptor(), &intent).await;
    assert_eq!(
        skills_failed.preparation.destination,
        failed.preparation.destination
    );
    assert!(skills_failed.error.as_ref().unwrap().contains("Skills"));
    let destination = &skills_failed.preparation.destination.path;
    assert!(destination.join("module/nested/tracked").is_file());
    assert!(provider.try_next_start().is_none());
    runtime.clear_skill_discovery_failure();
    let ready = prepare(server.descriptor(), &intent).await;
    assert_eq!(ready.error, None);
    assert_eq!(
        ready.preparation.destination,
        failed.preparation.destination
    );
    let snapshot = support::create_session(
        server.descriptor(),
        &creation(&ready.preparation, "Use initialized modules"),
    )
    .await;
    let start = provider.next_start().await;
    assert_eq!(start.execution_directory(), destination);
    drop(start);
    assert_eq!(snapshot.session.execution_directory.path, *destination);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn preparation_is_durable_and_unborn_creation_returns_reason_without_mutation() {
    let temp = tempfile::tempdir().unwrap();
    let root = suru::paths::canonical(temp.path()).unwrap();
    let main = root.join("main");
    committed(&main);
    let state = root.join("state");
    let (runtime, _) = ControlledProvider::new();
    let config = ServerConfig::new(&state, "durable").unwrap();
    let server = server::spawn_with_provider(config.clone(), runtime)
        .await
        .unwrap();
    let intent = request(&main, "Durable intent");
    let first = prepare(server.descriptor(), &intent).await;
    assert_eq!(first.error, None);
    server.shutdown().await.unwrap();
    git(
        &main,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "advanced",
        ],
    );
    let (runtime, _) = ControlledProvider::new();
    let server = server::spawn_with_provider(config, runtime).await.unwrap();
    let second = prepare(server.descriptor(), &intent).await;
    assert_eq!(second.error, None);
    assert_eq!(second.preparation, first.preparation);
    let unborn = root.join("unborn");
    std::fs::create_dir(&unborn).unwrap();
    git(&unborn, &["init", "-b", "main"]);
    let response = reqwest::Client::new()
        .post(format!(
            "{}/v1/checkouts/prepare",
            server.descriptor().base_url
        ))
        .bearer_auth(&server.descriptor().token)
        .json(&request(&unborn, "No usable revision"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    assert!(response.text().await.unwrap().contains("unborn"));
    assert!(!unborn.join(".suru-worktrees").exists());
    server.shutdown().await.unwrap();
}

struct CollisionAdapter {
    git: suru::source_control::GitSourceControl,
}
#[async_trait::async_trait]
impl suru::source_control::SourceControl for CollisionAdapter {
    async fn discover(&self, path: &Path) -> ResolvedWorkspace {
        self.git.discover(path).await
    }
    async fn plan_checkout(
        &self,
        id: PreparationId,
        source: &ResolvedWorkspace,
        description: &str,
    ) -> Result<PreparedCheckout, String> {
        let plan = self.git.plan_checkout(id, source, description).await?;
        let CheckoutPreparationPlan::Git { branch, .. } = &plan.plan;
        // Another Git actor wins this name after planning, before Suru mutation.
        git(&plan.source.path, &["branch", branch]);
        Ok(plan)
    }
    async fn prepare_checkout(&self, plan: &PreparedCheckout) -> Result<ResolvedWorkspace, String> {
        self.git.prepare_checkout(plan).await
    }
    async fn initialize_checkout(&self, plan: &PreparedCheckout) -> Result<(), String> {
        self.git.initialize_checkout(plan).await
    }
}
#[tokio::test]
async fn branch_collision_does_not_reset_or_duplicate_intention_and_unknown_main_disables_creation()
{
    let temp = tempfile::tempdir().unwrap();
    let root = suru::paths::canonical(temp.path()).unwrap();
    let main = root.join("main");
    committed(&main);
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_source_control(
        ServerConfig::new(root.join("state"), "collisions").unwrap(),
        vec![runtime],
        Default::default(),
        std::sync::Arc::new(CollisionAdapter {
            git: Default::default(),
        }),
    )
    .await
    .unwrap();
    let intent = request(&main, "Collision");
    let failed = prepare(server.descriptor(), &intent).await;
    assert!(failed.error.as_ref().unwrap().contains("already exists"));
    assert!(!failed.preparation.destination.path.exists());
    let CheckoutPreparationPlan::Git {
        branch,
        source_commit,
    } = &failed.preparation.plan;
    assert_eq!(read_git(&main, &["rev-parse", branch]), *source_commit);
    assert!(provider.try_next_start().is_none());
    git(&main, &["branch", "-D", branch]); // Explicit external resolution permits the exact original intention to retry.
    let retried = prepare(server.descriptor(), &intent).await;
    assert_eq!(retried.error, None);
    assert_eq!(retried.preparation.plan, failed.preparation.plan);
    let separate = root.join("separate");
    let metadata = root.join("metadata");
    git(
        &root,
        &[
            "init",
            "--separate-git-dir",
            metadata.to_str().unwrap(),
            separate.to_str().unwrap(),
        ],
    );
    git(
        &separate,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "source",
        ],
    );
    let linked = root.join("unknown-main-linked");
    git(
        &separate,
        &["worktree", "add", "--detach", linked.to_str().unwrap()],
    );
    let response = reqwest::Client::new()
        .post(format!(
            "{}/v1/checkouts/prepare",
            server.descriptor().base_url
        ))
        .bearer_auth(&server.descriptor().token)
        .json(&request(&linked, "Unknown main"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("main checkout location")
    );
    assert!(!metadata.join(".suru-worktrees").exists());
    server.shutdown().await.unwrap();
}

#[path = "preparation_recovery.rs"]
mod recovery;
