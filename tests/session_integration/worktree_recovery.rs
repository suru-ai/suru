//! Recovery preserves retained conversation and working-copy identities.
use crate::{
    provider_support::{ControlledProvider, ControlledProviderSession},
    repositories::git,
    support,
};
use std::path::{Path, PathBuf};
use suru::{
    protocol::*,
    provider::{ProviderEvent, ProviderResumeState},
    server::{self, ServerConfig},
};
use tokio::time::{Duration, timeout};

struct Layout {
    _temp: tempfile::TempDir,
    root: PathBuf,
    main: PathBuf,
    linked: PathBuf,
}
impl Layout {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = suru::paths::canonical(temp.path()).unwrap();
        let main = root.join("main");
        let linked = root.join("external worktree");
        std::fs::create_dir(&main).unwrap();
        git(&main, &["init", "-b", "main"]);
        std::fs::create_dir(main.join("nested")).unwrap();
        std::fs::write(main.join("nested/tracked"), "original").unwrap();
        git(&main, &["add", "."]);
        commit(&main, "initial");
        git(
            &main,
            &["worktree", "add", "-b", "topic", linked.to_str().unwrap()],
        );
        Self {
            _temp: temp,
            root,
            main,
            linked,
        }
    }
    fn config(&self, channel: &str) -> ServerConfig {
        ServerConfig::new(self.root.join("state"), channel).unwrap()
    }
}
fn commit(root: &Path, message: &str) {
    git(
        root,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-am",
            message,
        ],
    );
}
fn read_git(root: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}
fn prompt(text: &str) -> InitialPrompt {
    InitialPrompt {
        id: PromptId::new(),
        text: text.to_owned(),
        skill_invocations: vec![],
    }
}
fn identity() -> AgentIdentity {
    AgentIdentity {
        agent: AgentId::new("recovery-agent"),
        selection: support::hosted_selection("controlled", "test"),
    }
}
fn resume() -> ProviderResumeState {
    ProviderResumeState::new(serde_json::json!({"opaque":"keep-native-context", "version":7}))
}
async fn open(
    server: &server::RunningServer,
    provider: &mut ControlledProvider,
    path: &Path,
) -> (SessionSnapshot, ControlledProviderSession) {
    let snapshot = support::create_session(
        server.descriptor(),
        &CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: ExecutionDirectory {
                path: path.to_owned(),
            },
            prompt: prompt("Original history"),
        },
    )
    .await;
    let start = timeout(Duration::from_secs(2), provider.next_start())
        .await
        .unwrap();
    assert_eq!(start.execution_directory(), path);
    let mut connected = start.succeed_with_resume(identity(), Some(resume()));
    timeout(Duration::from_secs(2), connected.next_turn())
        .await
        .unwrap()
        .succeed();
    connected.emit(ProviderEvent::TurnCompleted);
    support::read_session_until(
        &reqwest::Client::new(),
        server.descriptor(),
        snapshot.session.id,
        "initial Turn completed",
        |s| {
            s.turns
                .first()
                .is_some_and(|t| t.status == TurnStatus::Completed)
        },
    )
    .await;
    (snapshot, connected)
}
async fn admit(
    descriptor: &RuntimeDescriptor,
    id: SessionId,
    prompt: InitialPrompt,
) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/sessions/{id}/prompts", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&AdmitPromptRequest {
            prompt,
            delivery: PromptDelivery::Steer,
        })
        .send()
        .await
        .unwrap()
}
async fn assert_success(response: reqwest::Response) {
    let status = response.status();
    let body = response.text().await.unwrap();
    assert!(status.is_success(), "{status}: {body}");
}
async fn restarted(provider: &mut ControlledProvider, path: &Path) -> ControlledProviderSession {
    let start = timeout(Duration::from_secs(2), provider.next_start())
        .await
        .expect("native Session restarts after recovery");
    assert_eq!(start.execution_directory(), path);
    assert_eq!(start.resume_state(), Some(&resume()));
    let mut connected = start.succeed_with_resume(identity(), Some(resume()));
    timeout(Duration::from_secs(2), connected.next_turn())
        .await
        .unwrap()
        .succeed();
    connected.emit(ProviderEvent::TurnCompleted);
    connected
}

#[tokio::test]
async fn two_warm_sessions_recover_one_external_worktree_from_current_branch_tip_and_both_resume() {
    let layout = Layout::new();
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(layout.config("shared-recovery"), runtime)
        .await
        .unwrap();
    let (a, _old_a) = open(&server, &mut provider, &layout.linked.join("nested")).await;
    let (b, _old_b) = open(&server, &mut provider, &layout.linked).await;
    std::fs::remove_dir_all(&layout.linked).unwrap();
    std::fs::write(layout.main.join("nested/tracked"), "new tip").unwrap();
    commit(&layout.main, "advance retained branch");
    let tip = read_git(&layout.main, &["rev-parse", "HEAD"]);
    git(&layout.main, &["update-ref", "refs/heads/topic", &tip]);
    let descriptor = server.descriptor().clone();
    let task_a =
        tokio::spawn(async move { admit(&descriptor, a.session.id, prompt("Continue A")).await });
    assert_success(
        timeout(Duration::from_secs(2), task_a)
            .await
            .unwrap()
            .unwrap(),
    )
    .await;
    let _new_a = restarted(&mut provider, &layout.linked.join("nested")).await;
    let response = admit(server.descriptor(), b.session.id, prompt("Continue B")).await;
    assert_success(response).await;
    let _new_b = restarted(&mut provider, &layout.linked).await;
    assert_eq!(read_git(&layout.linked, &["rev-parse", "HEAD"]), tip);
    assert_eq!(
        std::fs::read_to_string(layout.linked.join("nested/tracked")).unwrap(),
        "new tip"
    );
    let retained = support::read_session(server.descriptor(), b.session.id).await;
    assert_eq!(
        retained.session.checkout.as_ref().unwrap().id,
        b.session.checkout.as_ref().unwrap().id
    );
    assert_eq!(retained.prompts[0].text, "Original history");
    assert_eq!(retained.session.execution_directory.path, layout.linked);
    assert_eq!(
        read_git(&layout.main, &["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        2
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn missing_subdirectory_failure_still_invalidates_other_warm_sessions_and_preserves_retry() {
    let layout = Layout::new();
    let untracked = layout.linked.join("untracked-directory");
    std::fs::create_dir(&untracked).unwrap();
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(layout.config("missing-subdir"), runtime)
        .await
        .unwrap();
    let (a, _old_a) = open(&server, &mut provider, &untracked).await;
    let (b, _old_b) = open(&server, &mut provider, &layout.linked).await;
    std::fs::remove_dir_all(&layout.linked).unwrap();
    let retry = prompt("Restore exact context");
    let response = admit(server.descriptor(), a.session.id, retry.clone()).await;
    assert_eq!(response.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    let body = response.text().await.unwrap();
    assert!(body.contains("exact Execution Directory"), "{body}");
    assert!(layout.linked.is_dir());
    assert!(provider.try_next_start().is_none());
    assert_success(
        admit(
            server.descriptor(),
            b.session.id,
            prompt("Other warm Session"),
        )
        .await,
    )
    .await;
    let _new_b = restarted(&mut provider, &layout.linked).await;
    std::fs::create_dir(&untracked).unwrap();
    assert_success(admit(server.descriptor(), a.session.id, retry).await).await;
    let _new_a = restarted(&mut provider, &untracked).await;
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn detached_recovery_after_restart_preserves_history_exact_subdir_and_opaque_resume() {
    let layout = Layout::new();
    git(&layout.linked, &["checkout", "--detach"]);
    let commit = read_git(&layout.linked, &["rev-parse", "HEAD"]);
    let (runtime, mut provider) = ControlledProvider::new();
    let config = layout.config("detached-restart");
    let server = server::spawn_with_provider(config.clone(), runtime)
        .await
        .unwrap();
    let (original, _connection) = open(&server, &mut provider, &layout.linked.join("nested")).await;
    server.shutdown().await.unwrap();
    std::fs::remove_dir_all(&layout.linked).unwrap();
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(config, runtime).await.unwrap();
    assert_success(
        admit(
            server.descriptor(),
            original.session.id,
            prompt("Resume detached context"),
        )
        .await,
    )
    .await;
    let _connection = restarted(&mut provider, &layout.linked.join("nested")).await;
    assert_eq!(read_git(&layout.linked, &["rev-parse", "HEAD"]), commit);
    assert_eq!(
        read_git(&layout.linked, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "HEAD"
    );
    let retained = support::read_session(server.descriptor(), original.session.id).await;
    assert_eq!(retained.session.workspace.id, original.session.workspace.id);
    assert_eq!(retained.session.id, original.session.id);
    assert_eq!(retained.prompts[0].id, original.prompts[0].id);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn recovery_conflicts_preserve_retained_identity_and_never_start_native_work() {
    for conflict in [
        "deleted branch",
        "occupied branch",
        "external lock",
        "replacement repository",
        "replacement directory",
        "nested repository",
        "missing metadata",
        "missing commit",
    ] {
        let layout = Layout::new();
        if conflict == "missing commit" {
            git(&layout.linked, &["checkout", "--detach"]);
        }
        let retained_commit = read_git(&layout.linked, &["rev-parse", "HEAD"]);
        let (runtime, mut provider) = ControlledProvider::new();
        let config = layout.config("recovery-conflict");
        let server = server::spawn_with_provider(config.clone(), runtime)
            .await
            .unwrap();
        let exact = layout.linked.join("nested");
        let (original, _old) = open(&server, &mut provider, &exact).await;
        server.shutdown().await.unwrap();
        if conflict != "nested repository" {
            std::fs::remove_dir_all(&layout.linked).unwrap();
        }
        match conflict {
            "deleted branch" => {
                git(&layout.main, &["update-ref", "-d", "refs/heads/topic"]);
            }
            "occupied branch" => {
                git(
                    &layout.main,
                    &["worktree", "remove", layout.linked.to_str().unwrap()],
                );
                git(
                    &layout.main,
                    &[
                        "worktree",
                        "add",
                        layout.root.join("other").to_str().unwrap(),
                        "topic",
                    ],
                );
            }
            "external lock" => {
                git(
                    &layout.main,
                    &[
                        "worktree",
                        "lock",
                        "--reason",
                        "user owns this lock",
                        layout.linked.to_str().unwrap(),
                    ],
                );
            }
            "replacement repository" => {
                std::fs::create_dir_all(&exact).unwrap();
                git(&layout.linked, &["init", "-b", "replacement"]);
                std::fs::write(layout.linked.join("unrelated"), "preserve me").unwrap();
                git(&layout.linked, &["add", "."]);
                commit(&layout.linked, "replacement");
            }
            "replacement directory" => {
                std::fs::create_dir_all(&exact).unwrap();
                std::fs::write(layout.linked.join("unrelated"), "preserve me").unwrap();
            }
            "nested repository" => {
                git(&exact, &["init", "-b", "nested"]);
            }
            "missing metadata" => {
                std::fs::rename(
                    layout.main.join(".git"),
                    layout.root.join("hidden-metadata"),
                )
                .unwrap();
            }
            "missing commit" => {
                std::fs::remove_file(
                    layout
                        .main
                        .join(".git/objects")
                        .join(&retained_commit[..2])
                        .join(&retained_commit[2..]),
                )
                .unwrap();
            }
            _ => unreachable!(),
        }
        let (runtime, mut provider) = ControlledProvider::new();
        let server = server::spawn_with_provider(config, runtime).await.unwrap();
        let restored = support::read_session(server.descriptor(), original.session.id).await;
        assert_eq!(
            restored.session.workspace.id, original.session.workspace.id,
            "{conflict}"
        );
        assert_eq!(
            restored.session.checkout, original.session.checkout,
            "{conflict}"
        );
        let draft = prompt("Retry this same conversation");
        let response = admit(server.descriptor(), original.session.id, draft.clone()).await;
        let status = response.status();
        let body = response.text().await.unwrap();
        assert_eq!(
            status,
            reqwest::StatusCode::UNPROCESSABLE_ENTITY,
            "{conflict}: {body}"
        );
        assert!(body.contains("Worktree unavailable"), "{conflict}: {body}");
        assert!(provider.try_next_start().is_none(), "{conflict}");
        let retained = support::read_session(server.descriptor(), original.session.id).await;
        assert!(!retained.prompts.iter().any(|p| p.id == draft.id));
        assert_eq!(retained.prompts[0].text, "Original history");
        if conflict.starts_with("replacement") {
            assert_eq!(
                std::fs::read_to_string(layout.linked.join("unrelated")).unwrap(),
                "preserve me"
            );
        }
        if conflict == "external lock" {
            git(
                &layout.main,
                &["worktree", "unlock", layout.linked.to_str().unwrap()],
            );
            assert_success(admit(server.descriptor(), original.session.id, draft).await).await;
            let _new = restarted(&mut provider, &exact).await;
        }
        server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn recursive_submodule_and_skill_failures_retain_recovery_progress_across_restart() {
    let layout = Layout::new();
    let leaf = layout.root.join("leaf");
    let middle = layout.root.join("middle");
    for path in [&leaf, &middle] {
        std::fs::create_dir(path).unwrap();
        git(path, &["init", "-b", "main"]);
        std::fs::write(path.join("tracked"), "module").unwrap();
        git(path, &["add", "."]);
        commit(path, "initial module");
    }
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
    commit(&middle, "nested module");
    git(
        &layout.main,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            middle.to_str().unwrap(),
            "module",
        ],
    );
    commit(&layout.main, "recursive module");
    git(&layout.linked, &["merge", "--ff-only", "main"]);
    let git_config = layout.root.join("git-config");
    std::fs::write(&git_config, "[protocol \"file\"]\nallow = always\n").unwrap();
    let adapter = std::sync::Arc::new(
        suru::source_control::GitSourceControl::default().with_configuration_file(&git_config),
    );
    let (runtime, mut provider) = ControlledProvider::new();
    let config = layout.config("recursive-recovery");
    let server = server::spawn_with_source_control(
        config.clone(),
        vec![runtime],
        server::ServerTimings::default().with_checkout_skill_timeout(Duration::from_millis(300)),
        adapter.clone(),
    )
    .await
    .unwrap();
    let (original, _old) = open(&server, &mut provider, &layout.linked).await;
    std::fs::remove_dir_all(&layout.linked).unwrap();
    let hidden = layout.root.join("middle-hidden");
    std::fs::rename(&middle, &hidden).unwrap();
    let retry = prompt("Continue with recursive modules");
    let failed = admit(server.descriptor(), original.session.id, retry.clone()).await;
    assert_eq!(failed.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    let body = failed.text().await.unwrap();
    assert!(body.contains("submodule initialization failed"), "{body}");
    assert!(layout.linked.join("nested/tracked").is_file());
    assert!(provider.try_next_start().is_none());
    server.shutdown().await.unwrap();
    std::fs::rename(hidden, &middle).unwrap();
    let (runtime, mut provider) = ControlledProvider::new();
    runtime.fail_skill_discovery("destination catalog offline");
    let server = server::spawn_with_source_control(
        config,
        vec![runtime.clone()],
        server::ServerTimings::default().with_checkout_skill_timeout(Duration::from_millis(300)),
        adapter,
    )
    .await
    .unwrap();
    let failed = admit(server.descriptor(), original.session.id, retry.clone()).await;
    assert_eq!(failed.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    let body = failed.text().await.unwrap();
    assert!(body.contains("Skills"), "{body}");
    assert!(layout.linked.join("module/nested/tracked").is_file());
    assert!(provider.try_next_start().is_none());
    runtime.clear_skill_discovery_failure();
    assert_success(admit(server.descriptor(), original.session.id, retry).await).await;
    let _new = restarted(&mut provider, &layout.linked).await;
    assert_eq!(
        read_git(&layout.main, &["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        2
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn prompt_gate_persists_external_branch_switch_without_catalog_interest_before_recovery() {
    let layout = Layout::new();
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(layout.config("no-interest-recovery"), runtime)
        .await
        .unwrap();
    let (original, mut connection) = open(&server, &mut provider, &layout.linked).await;
    git(&layout.linked, &["checkout", "-b", "latest-observed"]);
    assert_success(
        admit(
            server.descriptor(),
            original.session.id,
            prompt("Use new local branch"),
        )
        .await,
    )
    .await;
    timeout(Duration::from_secs(2), connection.next_turn())
        .await
        .unwrap()
        .succeed();
    connection.emit(ProviderEvent::TurnCompleted);
    support::read_session_until(
        &reqwest::Client::new(),
        server.descriptor(),
        original.session.id,
        "new branch Turn settles",
        |s| {
            s.turns
                .get(1)
                .is_some_and(|t| t.status == TurnStatus::Completed)
        },
    )
    .await;
    let saved = support::read_session(server.descriptor(), original.session.id).await;
    assert!(
        matches!(saved.session.checkout.unwrap().recovery_revision, Some(CheckoutRevision::Branch { name, .. }) if name == "latest-observed")
    );
    std::fs::remove_dir_all(&layout.linked).unwrap();
    assert_success(
        admit(
            server.descriptor(),
            original.session.id,
            prompt("Recover latest observed branch"),
        )
        .await,
    )
    .await;
    let _reconnected = restarted(&mut provider, &layout.linked).await;
    assert_eq!(
        read_git(&layout.linked, &["symbolic-ref", "--short", "HEAD"]),
        "latest-observed"
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn interrupted_registration_retries_without_overwriting_staged_or_switched_work() {
    for partial in ["registration", "staged contents", "switched revision"] {
        let layout = Layout::new();
        let (runtime, mut provider) = ControlledProvider::new();
        let config = layout.config("partial-recovery");
        let server = server::spawn_with_provider(config.clone(), runtime)
            .await
            .unwrap();
        let (original, _old) = open(&server, &mut provider, &layout.linked).await;
        let association = original.session.checkout.as_ref().unwrap();
        server.shutdown().await.unwrap();
        std::fs::remove_dir_all(&layout.linked).unwrap();
        git(
            &layout.main,
            &["worktree", "remove", layout.linked.to_str().unwrap()],
        );
        let token = format!("suru-recovery:{}", association.id.0);
        git(
            &layout.main,
            &[
                "worktree",
                "add",
                "--no-checkout",
                "--lock",
                "--reason",
                &token,
                layout.linked.to_str().unwrap(),
                "topic",
            ],
        );
        if partial == "staged contents" {
            std::fs::write(layout.linked.join("new-file"), "staged user work").unwrap();
            git(&layout.linked, &["add", "new-file"]);
        } else if partial == "switched revision" {
            git(
                &layout.linked,
                &["symbolic-ref", "HEAD", "refs/heads/other"],
            );
            git(&layout.main, &["branch", "other", "main"]);
        }
        let unrelated = layout.root.join("unrelated-missing");
        git(
            &layout.main,
            &["worktree", "add", "--detach", unrelated.to_str().unwrap()],
        );
        std::fs::remove_dir_all(&unrelated).unwrap();
        let (runtime, mut provider) = ControlledProvider::new();
        let server = server::spawn_with_provider(config, runtime).await.unwrap();
        let retry = prompt("Finish interrupted recovery");
        let response = admit(server.descriptor(), original.session.id, retry).await;
        if partial == "registration" {
            assert_success(response).await;
            let _new = restarted(&mut provider, &layout.linked).await;
            assert!(layout.linked.join("nested/tracked").exists());
        } else {
            let status = response.status();
            let body = response.text().await.unwrap();
            assert_eq!(
                status,
                reqwest::StatusCode::UNPROCESSABLE_ENTITY,
                "{partial}: {body}"
            );
            assert!(provider.try_next_start().is_none());
            if partial == "staged contents" {
                assert!(body.contains("staged changes"), "{body}");
                assert_eq!(
                    read_git(&layout.linked, &["show", ":new-file"]),
                    "staged user work"
                );
            } else {
                assert!(body.contains("retained revision"), "{body}");
            }
        }
        assert!(
            read_git(&layout.main, &["worktree", "list", "--porcelain"])
                .contains(unrelated.to_str().unwrap())
        );
        server.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn shared_recovery_does_not_interrupt_working_session_and_refuses_stale_native_steering() {
    let layout = Layout::new();
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(layout.config("active-recovery"), runtime)
        .await
        .unwrap();
    let (a, mut old_a) = open(&server, &mut provider, &layout.linked).await;
    let (b, _old_b) = open(&server, &mut provider, &layout.linked).await;
    assert_success(admit(server.descriptor(), a.session.id, prompt("Keep working")).await).await;
    timeout(Duration::from_secs(2), old_a.next_turn())
        .await
        .unwrap()
        .succeed();
    std::fs::remove_dir_all(&layout.linked).unwrap();
    assert_success(
        admit(
            server.descriptor(),
            b.session.id,
            prompt("Recover shared checkout"),
        )
        .await,
    )
    .await;
    let _new_b = restarted(&mut provider, &layout.linked).await;
    let retry = prompt("Steer previous native context");
    let response = admit(server.descriptor(), a.session.id, retry.clone()).await;
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(status, reqwest::StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body.contains("still Working"), "{body}");
    assert_eq!(
        support::read_session(server.descriptor(), a.session.id)
            .await
            .turns[1]
            .status,
        TurnStatus::Active
    );
    old_a.emit(ProviderEvent::TurnCompleted);
    support::read_session_until(
        &reqwest::Client::new(),
        server.descriptor(),
        a.session.id,
        "original native work settles normally",
        |s| s.turns[1].status == TurnStatus::Completed,
    )
    .await;
    assert_success(admit(server.descriptor(), a.session.id, retry).await).await;
    let _new_a = restarted(&mut provider, &layout.linked).await;
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn already_admitted_queued_prompt_recovers_at_native_start_and_revalidates_skills() {
    let layout = Layout::new();
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(layout.config("queued-recovery"), runtime.clone())
        .await
        .unwrap();
    let (original, mut connection) = open(&server, &mut provider, &layout.linked).await;
    assert_success(
        admit(
            server.descriptor(),
            original.session.id,
            prompt("First active work"),
        )
        .await,
    )
    .await;
    timeout(Duration::from_secs(2), connection.next_turn())
        .await
        .unwrap()
        .succeed();
    let queued = prompt("Queued work must not use stale process");
    let response = reqwest::Client::new()
        .post(format!(
            "{}/v1/sessions/{}/prompts",
            server.descriptor().base_url,
            original.session.id
        ))
        .bearer_auth(&server.descriptor().token)
        .json(&AdmitPromptRequest {
            prompt: queued.clone(),
            delivery: PromptDelivery::Queue,
        })
        .send()
        .await
        .unwrap();
    assert_success(response).await;
    std::fs::remove_dir_all(&layout.linked).unwrap();
    runtime.fail_skill_discovery("catalog unavailable after checkout recreation");
    connection.emit(ProviderEvent::TurnCompleted);
    let failed = support::read_session_until(
        &reqwest::Client::new(),
        server.descriptor(),
        original.session.id,
        "queued recovery skill failure",
        |s| {
            s.turns
                .get(2)
                .is_some_and(|t| t.status == TurnStatus::Failed)
        },
    )
    .await;
    assert!(layout.linked.is_dir());
    assert!(provider.try_next_start().is_none());
    assert!(failed.prompts.iter().any(|p| p.id == queued.id));
    runtime.clear_skill_discovery_failure();
    assert_success(
        admit(
            server.descriptor(),
            original.session.id,
            prompt("Retry recovered queued work"),
        )
        .await,
    )
    .await;
    let _new = restarted(&mut provider, &layout.linked).await;
    server.shutdown().await.unwrap();
}

#[path = "worktree_removal.rs"]
mod removal;
