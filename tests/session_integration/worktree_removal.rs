use super::*;
use crate::server_support::PROGRESS_DEADLINE;
use suru::source_control::{GitSourceControl, SourceControl};
async fn target(path: &Path) -> CheckoutRemovalTarget {
    let resolved = GitSourceControl::default().discover(path).await;
    CheckoutRemovalTarget {
        repository: resolved.workspace.repository.unwrap(),
        checkout: resolved.checkout.unwrap(),
    }
}
async fn preview(
    server: &server::RunningServer,
    target: &CheckoutRemovalTarget,
) -> CheckoutRemovalPreview {
    let response = reqwest::Client::new()
        .post(format!(
            "{}/v1/checkouts/removal-preview",
            server.descriptor().base_url
        ))
        .bearer_auth(&server.descriptor().token)
        .json(target)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert!(status.is_success(), "{status}: {body}");
    serde_json::from_str(&body).unwrap()
}
async fn remove(
    server: &server::RunningServer,
    preview: CheckoutRemovalPreview,
    force: bool,
) -> RemoveCheckoutResult {
    let response = reqwest::Client::new()
        .post(format!(
            "{}/v1/checkouts/remove",
            server.descriptor().base_url
        ))
        .bearer_auth(&server.descriptor().token)
        .json(&RemoveCheckoutRequest { preview, force })
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert!(status.is_success(), "{status}: {body}");
    serde_json::from_str(&body).unwrap()
}
#[tokio::test]
async fn removal_previews_all_content_conditions_force_retains_branch_and_main_is_excluded() {
    for condition in ["clean", "tracked", "untracked", "ignored", "locked"] {
        let layout = Layout::new();
        let (runtime, _) = ControlledProvider::new();
        let server = server::spawn_with_provider(layout.config(condition), runtime)
            .await
            .unwrap();
        match condition {
            "tracked" => {
                std::fs::write(layout.linked.join("nested/tracked"), "local changes").unwrap()
            }
            "untracked" => {
                std::fs::write(layout.linked.join("personal"), "keep until force").unwrap()
            }
            "ignored" => {
                std::fs::write(layout.main.join(".git/info/exclude"), "ignored\n").unwrap();
                std::fs::write(layout.linked.join("ignored"), "ignored contents").unwrap();
            }
            "locked" => git(
                &layout.main,
                &[
                    "worktree",
                    "lock",
                    "--reason",
                    "external owner's lock",
                    layout.linked.to_str().unwrap(),
                ],
            ),
            _ => {}
        }
        let target = target(&layout.linked).await;
        let facts = preview(&server, &target).await;
        assert_eq!(facts.affected_sessions, 0);
        assert_eq!(!facts.inspection.tracked.is_empty(), condition == "tracked");
        assert_eq!(
            !facts.inspection.untracked.is_empty(),
            condition == "untracked"
        );
        assert_eq!(!facts.inspection.ignored.is_empty(), condition == "ignored");
        assert_eq!(facts.inspection.lock.is_some(), condition == "locked");
        assert!(layout.linked.exists(), "preview/cancel never mutates");
        let result = remove(&server, facts.clone(), false).await;
        if facts.inspection.requires_force() {
            assert!(!result.removed);
            assert!(layout.linked.exists());
            assert!(remove(&server, facts, true).await.removed);
        } else {
            assert!(result.removed, "{:?}", result.error);
        }
        assert!(!layout.linked.exists());
        assert!(!read_git(&layout.main, &["rev-parse", "refs/heads/topic"]).is_empty());
        let main_target = self::target(&layout.main).await;
        let response = reqwest::Client::new()
            .post(format!(
                "{}/v1/checkouts/removal-preview",
                server.descriptor().base_url
            ))
            .bearer_auth(&server.descriptor().token)
            .json(&main_target)
            .send()
            .await
            .unwrap();
        assert!(!response.status().is_success());
        assert!(layout.main.exists());
        server.shutdown().await.unwrap();
    }
}
#[tokio::test]
async fn removal_reconfirms_new_ignored_contents_and_retains_fresh_revision_history_for_recovery() {
    let layout = Layout::new();
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(layout.config("retained"), runtime)
        .await
        .unwrap();
    let (original, _connected) = open(&server, &mut provider, &layout.linked).await;
    let target = target(&layout.linked).await;
    let old = preview(&server, &target).await;
    assert_eq!(old.affected_sessions, 1);
    std::fs::write(layout.main.join(".git/info/exclude"), "ignored\n").unwrap();
    std::fs::write(layout.linked.join("ignored"), "new unconfirmed contents").unwrap();
    let changed = remove(&server, old, false).await;
    assert!(!changed.removed);
    assert!(changed.error.unwrap().contains("changed"));
    assert!(layout.linked.join("ignored").exists());
    git(&layout.linked, &["checkout", "-b", "new-recovery"]);
    commit(&layout.linked, "latest before removal");
    let commit_before = read_git(&layout.linked, &["rev-parse", "HEAD"]);
    let facts = preview(&server, &target).await;
    assert!(remove(&server, facts, false).await.removed);
    assert!(!layout.linked.exists());
    assert_eq!(
        read_git(&layout.main, &["rev-parse", "refs/heads/new-recovery"]),
        commit_before
    );
    commit(&layout.main, "advance retained branch after removal");
    let advanced = read_git(&layout.main, &["rev-parse", "HEAD"]);
    git(
        &layout.main,
        &["update-ref", "refs/heads/new-recovery", &advanced],
    );
    assert_success(
        admit(
            server.descriptor(),
            original.session.id,
            prompt("Recover retained history"),
        )
        .await,
    )
    .await;
    let _connection = restarted(&mut provider, &layout.linked).await;
    assert_eq!(read_git(&layout.linked, &["rev-parse", "HEAD"]), advanced);
    assert_eq!(
        read_git(&layout.linked, &["branch", "--show-current"]),
        "new-recovery"
    );
    let snapshot = support::read_session_until(
        &reqwest::Client::new(),
        server.descriptor(),
        original.session.id,
        "recovered history",
        |s| s.turns.len() == 2,
    )
    .await;
    assert_eq!(snapshot.prompts[0].text, "Original history");
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn removal_waits_for_native_startup_and_force_cannot_remove_surviving_subagent_checkout() {
    use suru::provider::{ProviderSubagentId, ProviderSubagentStatus};
    let layout = Layout::new();
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(layout.config("working"), runtime)
        .await
        .unwrap();
    let target = target(&layout.linked).await;
    let old = preview(&server, &target).await;
    let snapshot = support::create_session(
        server.descriptor(),
        &CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: ExecutionDirectory {
                path: layout.linked.clone(),
            },
            prompt: prompt("Working parent"),
        },
    )
    .await;
    let start = timeout(PROGRESS_DEADLINE, provider.next_start())
        .await
        .unwrap();
    let connection = {
        let pending = remove(&server, old, true);
        tokio::pin!(pending);
        assert!(
            timeout(Duration::from_millis(25), &mut pending)
                .await
                .is_err(),
            "native startup retains the mutation guard"
        );
        assert!(layout.linked.exists());
        let mut connection = start.succeed(identity());
        timeout(PROGRESS_DEADLINE, connection.next_turn())
            .await
            .unwrap()
            .succeed();
        let result = timeout(PROGRESS_DEADLINE, pending).await.unwrap();
        assert!(!result.removed);
        assert!(result.error.unwrap().contains("Working"));
        connection
    };
    let child = ProviderSubagentId::new("surviving-child");
    connection
        .emit_and_wait_until_observed(ProviderEvent::SubagentStarted {
            subagent_id: child.clone(),
            name: "Explore".into(),
            description: "Keep working".into(),
        })
        .await;
    connection
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    support::read_session_until(
        &reqwest::Client::new(),
        server.descriptor(),
        snapshot.session.id,
        "parent settled with surviving child",
        |s| s.turns[0].status == TurnStatus::Completed && s.session.working_since.is_some(),
    )
    .await;
    let facts = preview(&server, &target).await;
    assert_eq!(facts.affected_sessions, 2);
    assert!(facts.working_sessions >= 1);
    assert!(!remove(&server, facts, true).await.removed);
    assert!(layout.linked.exists());
    connection
        .emit_and_wait_until_observed(ProviderEvent::SubagentCompleted {
            subagent_id: child,
            status: ProviderSubagentStatus::Completed,
        })
        .await;
    support::read_session_until(
        &reqwest::Client::new(),
        server.descriptor(),
        snapshot.session.id,
        "all work completed",
        |s| s.session.working_since.is_none(),
    )
    .await;
    let facts = preview(&server, &target).await;
    assert_eq!(facts.working_sessions, 0);
    assert!(remove(&server, facts, false).await.removed);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn removal_initialized_submodules_require_force_and_session_settlement_deletion_never_cleans()
{
    let layout = Layout::new();
    let module = layout.root.join("module-source");
    std::fs::create_dir(&module).unwrap();
    git(&module, &["init", "-b", "main"]);
    std::fs::write(module.join("tracked"), "module contents").unwrap();
    git(&module, &["add", "."]);
    commit(&module, "initial module");
    git(
        &layout.linked,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            module.to_str().unwrap(),
            "module",
        ],
    );
    commit(&layout.linked, "add module");
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(layout.config("modules"), runtime)
        .await
        .unwrap();
    let (session, _connection) = open(&server, &mut provider, &layout.linked).await;
    let response = reqwest::Client::new()
        .post(format!(
            "{}/v1/sessions/{}/settlement",
            server.descriptor().base_url,
            session.session.id
        ))
        .bearer_auth(&server.descriptor().token)
        .json(&SettleSessionRequest { settled: true })
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    assert!(layout.linked.join("module/tracked").exists());
    let response = reqwest::Client::new()
        .delete(format!(
            "{}/v1/sessions/{}",
            server.descriptor().base_url,
            session.session.id
        ))
        .bearer_auth(&server.descriptor().token)
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    assert!(layout.linked.join("module/tracked").exists());
    let target = target(&layout.linked).await;
    let facts = preview(&server, &target).await;
    assert_eq!(facts.affected_sessions, 0);
    assert_eq!(facts.inspection.initialized_submodules.len(), 1);
    assert!(!remove(&server, facts.clone(), false).await.removed);
    assert!(layout.linked.join("module/tracked").exists());
    assert!(remove(&server, facts, true).await.removed);
    assert!(module.join("tracked").exists());
    assert!(!read_git(&layout.main, &["rev-parse", "refs/heads/topic"]).is_empty());
    server.shutdown().await.unwrap();
}

struct FailAfterCreation;
#[async_trait::async_trait]
impl suru::source_control::PreparationObserver for FailAfterCreation {
    async fn checkpoint(
        &self,
        at: suru::source_control::PreparationCheckpoint,
        _: &PreparedCheckout,
    ) -> Result<(), String> {
        if at == suru::source_control::PreparationCheckpoint::CheckoutCreated {
            Err("Destination preparation interrupted after checkout creation".into())
        } else {
            Ok(())
        }
    }
}
#[tokio::test]
async fn removal_of_failed_preparation_needs_no_session_and_force_refuses_replacement_contents() {
    let layout = Layout::new();
    let (runtime, _) = ControlledProvider::new();
    let server = server::spawn_with_source_control(
        layout.config("failed-preparation"),
        vec![runtime],
        Default::default(),
        std::sync::Arc::new(
            GitSourceControl::default()
                .with_preparation_observer(std::sync::Arc::new(FailAfterCreation)),
        ),
    )
    .await
    .unwrap();
    let prepared = reqwest::Client::new()
        .post(format!(
            "{}/v1/checkouts/prepare",
            server.descriptor().base_url
        ))
        .bearer_auth(&server.descriptor().token)
        .json(&PrepareCheckoutRequest {
            id: Default::default(),
            source: ExecutionDirectory {
                path: layout.main.clone(),
            },
            description: "Remove interrupted preparation".into(),
            provider: ProviderId::new("controlled"),
        })
        .send()
        .await
        .unwrap()
        .json::<PrepareCheckoutResult>()
        .await
        .unwrap();
    assert!(prepared.error.as_ref().unwrap().contains("interrupted"));
    let root = &prepared.preparation.destination.path;
    assert!(root.exists());
    let prepared_target = target(root).await;
    let facts = preview(&server, &prepared_target).await;
    assert_eq!(facts.affected_sessions, 0);
    assert!(remove(&server, facts, false).await.removed);
    assert!(!root.exists());
    let CheckoutPreparationPlan::Git { branch, .. } = &prepared.preparation.plan;
    assert!(
        !read_git(
            &layout.main,
            &["rev-parse", &format!("refs/heads/{branch}")]
        )
        .is_empty()
    );

    let original_target = target(&layout.linked).await;
    let facts = preview(&server, &original_target).await;
    let relocated = layout.root.join("keep-original");
    std::fs::rename(&layout.linked, &relocated).unwrap();
    std::fs::create_dir(&layout.linked).unwrap();
    std::fs::write(
        layout.linked.join("unrelated"),
        "never recursively delete me",
    )
    .unwrap();
    let response = reqwest::Client::new()
        .post(format!(
            "{}/v1/checkouts/remove",
            server.descriptor().base_url
        ))
        .bearer_auth(&server.descriptor().token)
        .json(&RemoveCheckoutRequest {
            preview: facts,
            force: true,
        })
        .send()
        .await
        .unwrap();
    assert!(!response.status().is_success());
    assert_eq!(
        std::fs::read_to_string(layout.linked.join("unrelated")).unwrap(),
        "never recursively delete me"
    );
    assert!(relocated.join("nested/tracked").exists());
    assert!(!read_git(&layout.main, &["rev-parse", "refs/heads/topic"]).is_empty());
    server.shutdown().await.unwrap();
}
