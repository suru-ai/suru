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
async fn managed_preparation_captures_local_commit_once_and_reuses_checkout_and_creation() {
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
    let creation = creation(&one.preparation, "corrected work");
    let created = support::create_session(server.descriptor(), &creation).await;
    let startup = provider.next_start().await;
    assert_eq!(startup.execution_directory(), destination);
    drop(startup);
    let duplicate = support::create_session(server.descriptor(), &creation).await;
    assert_eq!(created.session.id, duplicate.session.id);
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
async fn admission_deletes_the_intent_and_restart_withdraws_its_undelivered_prompt() {
    let temp = tempfile::tempdir().unwrap();
    let root = suru::paths::canonical(temp.path()).unwrap();
    let main = root.join("repo");
    committed(&main);
    let config = ServerConfig::new(root.join("state"), "retired-admission").unwrap();
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(config.clone(), runtime)
        .await
        .unwrap();
    let preparation_request = request(&main, "Retire after admission");
    let prepared = prepare(server.descriptor(), &preparation_request).await;
    let intent = config
        .data_dir()
        .join("checkout-preparations")
        .join(format!("{}.json", prepared.preparation.id.0));
    assert!(intent.is_file());

    let creation = creation(&prepared.preparation, "Leave this Prompt undelivered");
    let admitted = support::create_session(server.descriptor(), &creation).await;
    let startup = provider.next_start().await;
    assert!(
        !intent.exists(),
        "an admitted preparation no longer remains resumable"
    );
    server.shutdown().await.unwrap();
    drop(startup);

    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(config, runtime).await.unwrap();
    let restored = support::read_session(server.descriptor(), admitted.session.id).await;
    assert_eq!(restored.prompts[0].status, PromptStatus::Cancelled);
    assert_eq!(restored.session.status, SessionStatus::Idle);
    assert!(provider.try_next_start().is_none());
    let repeated_preparation = reqwest::Client::new()
        .post(format!(
            "{}/v1/checkouts/prepare",
            server.descriptor().base_url
        ))
        .bearer_auth(&server.descriptor().token)
        .json(&preparation_request)
        .send()
        .await
        .unwrap();
    assert!(!repeated_preparation.status().is_success());
    assert!(
        !intent.exists(),
        "repeating a completed preparation cannot recreate its intent"
    );
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
async fn every_provider_automatically_rebinds_selected_skill_names_to_destination_identities() {
    for provider_name in ["codex", "copilot", "claude"] {
        let temp = tempfile::tempdir().unwrap();
        let root = suru::paths::canonical(temp.path()).unwrap();
        let main = root.join("main");
        committed(&main);
        let model = format!("{provider_name}-model");
        let provider_id = ProviderId::new(provider_name);
        let (runtime, mut provider) = ControlledProvider::with_provider(
            provider_id.clone(),
            vec![support::hosted_model(provider_name, &model)],
        );
        let server = server::spawn_with_provider(
            ServerConfig::new(
                root.join("state"),
                format!("{provider_name}-destination-skill"),
            )
            .unwrap(),
            runtime,
        )
        .await
        .unwrap();
        let mut intent = request(&main, "Carry selected Skill");
        intent.provider = provider_id.clone();
        let ready = prepare(server.descriptor(), &intent).await;
        assert_eq!(ready.error, None);
        let mut create = creation(&ready.preparation, "Please $ExPlAiN this checkout");
        create.agent_selection = Some(support::hosted_selection(provider_name, &model));
        create.prompt.skill_invocations = vec![SkillInvocation {
            skill_id: SkillId::new(format!("{provider_name}-source-explain")),
            name: "explain".to_owned(),
            scope: Some("Source checkout".to_owned()),
            marker: SkillMarkerSpan { start: 7, end: 15 },
        }];

        let snapshot = support::create_session(server.descriptor(), &create).await;
        let rebound = &snapshot.prompts[0].skill_invocations[0];
        assert_eq!(rebound.skill_id, SkillId::new("safe-explain-id"));
        assert_eq!(rebound.name, "explain");
        assert_eq!(rebound.scope.as_deref(), Some("Workspace"));
        let start = provider.next_start().await;
        let mut connection = start.succeed(AgentIdentity {
            agent: AgentId::new(provider_name),
            selection: support::hosted_selection(provider_name, &model),
        });
        let turn = connection.next_turn().await;
        assert_eq!(turn.skill_invocations().len(), 1);
        assert_eq!(
            turn.skill_invocations()[0].skill_id,
            SkillId::new("safe-explain-id")
        );
        turn.fail("fixture settled");
        drop(connection);
        server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn missing_or_ambiguous_destination_names_block_atomically_and_explicit_choice_can_retry() {
    let temp = tempfile::tempdir().unwrap();
    let root = suru::paths::canonical(temp.path()).unwrap();
    let main = root.join("main");
    committed(&main);
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(
        ServerConfig::new(root.join("state"), "destination-skill-errors").unwrap(),
        runtime,
    )
    .await
    .unwrap();
    let ready = prepare(server.descriptor(), &request(&main, "Match Skills")).await;

    let mut mixed = creation(&ready.preparation, "$explain $review");
    mixed.prompt.skill_invocations = vec![
        SkillInvocation {
            skill_id: SkillId::new("source-explain"),
            name: "explain".to_owned(),
            scope: Some("Source".to_owned()),
            marker: SkillMarkerSpan { start: 0, end: 8 },
        },
        SkillInvocation {
            skill_id: SkillId::new("source-review"),
            name: "review".to_owned(),
            scope: Some("Source".to_owned()),
            marker: SkillMarkerSpan { start: 9, end: 16 },
        },
    ];
    let rejected = create_response(server.descriptor(), &mixed).await;
    assert_eq!(rejected.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    let body = rejected.text().await.unwrap();
    assert!(body.contains("ambiguous"), "{body}");
    assert!(body.contains("review"), "{body}");
    assert!(provider.try_next_start().is_none());

    let mut missing = creation(&ready.preparation, "$gone");
    missing.prompt.skill_invocations = vec![SkillInvocation {
        skill_id: SkillId::new("source-gone"),
        name: "gone".to_owned(),
        scope: Some("Source".to_owned()),
        marker: SkillMarkerSpan { start: 0, end: 5 },
    }];
    let rejected = create_response(server.descriptor(), &missing).await;
    assert_eq!(rejected.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    let body = rejected.text().await.unwrap();
    assert!(body.contains("missing"), "{body}");
    assert!(body.contains("gone"), "{body}");
    assert!(provider.try_next_start().is_none());

    // `review` has two destination matches. Selecting one from destination
    // autocomplete supplies its exact identity and metadata, resolving that
    // ambiguity without retargeting the already-current binding.
    let mut corrected = creation(&ready.preparation, "$explain $review");
    corrected.prompt.skill_invocations = vec![
        SkillInvocation {
            skill_id: SkillId::new("source-explain"),
            name: "explain".to_owned(),
            scope: Some("Source".to_owned()),
            marker: SkillMarkerSpan { start: 0, end: 8 },
        },
        SkillInvocation {
            skill_id: SkillId::new("safe-review-id"),
            name: "review".to_owned(),
            scope: Some("Workspace".to_owned()),
            marker: SkillMarkerSpan { start: 9, end: 16 },
        },
    ];
    let snapshot = create_response(server.descriptor(), &corrected)
        .await
        .error_for_status()
        .unwrap()
        .json::<SessionSnapshot>()
        .await
        .unwrap();
    assert_eq!(snapshot.prompts.len(), 1);
    assert_eq!(
        snapshot.prompts[0].skill_invocations[0].skill_id,
        SkillId::new("safe-explain-id")
    );
    assert_eq!(
        snapshot.prompts[0].skill_invocations[1].skill_id,
        SkillId::new("safe-review-id")
    );
    drop(provider.next_start().await);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn creation_leaves_recursive_submodules_uninitialized_and_still_discovers_destination_skills()
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
    // Make both submodule sources unavailable. Preparation must not consult
    // either source or run recursive initialization.
    let hidden = root.join("middle-hidden");
    std::fs::rename(&middle, &hidden).unwrap();
    let intent = request(&main, "Leave recursive modules alone");
    runtime.fail_skill_discovery("fixture destination Skills unavailable");
    let skills_failed = prepare(server.descriptor(), &intent).await;
    assert!(skills_failed.preparation.checkout_created);
    assert!(skills_failed.error.as_ref().unwrap().contains("Skills"));
    let destination = &skills_failed.preparation.destination.path;
    assert!(!destination.join("module/.git").exists());
    assert!(!destination.join("module/nested/tracked").exists());
    assert!(provider.try_next_start().is_none());
    runtime.clear_skill_discovery_failure();
    let ready = prepare(server.descriptor(), &intent).await;
    assert_eq!(ready.error, None);
    assert_eq!(
        ready.preparation.destination,
        skills_failed.preparation.destination
    );
    let snapshot = support::create_session(
        server.descriptor(),
        &creation(&ready.preparation, "Work without initialized modules"),
    )
    .await;
    let start = provider.next_start().await;
    assert_eq!(start.execution_directory(), destination);
    drop(start);
    assert_eq!(snapshot.session.execution_directory.path, *destination);
    assert!(!destination.join("module/.git").exists());
    assert!(!destination.join("module/nested/tracked").exists());
    std::fs::rename(hidden, &middle).unwrap();
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
