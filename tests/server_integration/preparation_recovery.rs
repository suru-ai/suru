use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use suru::protocol::*;
use suru::source_control::{GitSourceControl, PreparationCheckpoint, PreparationObserver};
struct InterruptedCreation(AtomicBool);
#[async_trait::async_trait]
impl PreparationObserver for InterruptedCreation {
    async fn checkpoint(
        &self,
        at: PreparationCheckpoint,
        _: &PreparedCheckout,
    ) -> Result<(), String> {
        if at == PreparationCheckpoint::CheckoutCreated && !self.0.swap(true, Ordering::SeqCst) {
            return Err("Serving checkout created before interrupted metadata update".into());
        }
        Ok(())
    }
}
#[tokio::test]
async fn remote_preparation_retries_reuse_owning_servers_checkout_and_admission() {
    let (runtime, mut provider) = provider_support::ControlledProvider::new();
    let pair = paired_servers_with_source_control(
        "remote-preparation-retry",
        false,
        Some(runtime),
        Some(Arc::new(
            GitSourceControl::default()
                .with_preparation_observer(Arc::new(InterruptedCreation(AtomicBool::new(false)))),
        )),
    )
    .await;
    let temp = tempfile::tempdir().unwrap();
    let root = suru::paths::canonical(temp.path()).unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
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
    };
    git(&["init", "-b", "main"]);
    git(&[
        "-c",
        "commit.gpgsign=false",
        "commit",
        "--allow-empty",
        "-m",
        "initial",
    ]);
    let remote = pair
        .connecting_client
        .outlook(Outlook::Remote("workstation".into()));
    let mut request = PrepareCheckoutRequest {
        id: Default::default(),
        source: ExecutionDirectory { path: root.clone() },
        description: "Remote preparation".into(),
        provider: ProviderId::new("controlled"),
    };
    let failed = remote.prepare_checkout(request.clone()).await.unwrap();
    assert!(failed.error.is_some());
    assert!(failed.preparation.destination.path.exists());
    assert!(provider.try_next_start().is_none());
    git(&[
        "-c",
        "commit.gpgsign=false",
        "commit",
        "--allow-empty",
        "-m",
        "source advanced",
    ]);
    request.description = "Edited Remote draft".into();
    let (a, b) = tokio::join!(
        remote.prepare_checkout(request.clone()),
        remote.prepare_checkout(request)
    );
    let a = a.unwrap();
    let b = b.unwrap();
    assert_eq!(a.error, None);
    assert_eq!(b.error, None);
    assert_eq!(a.preparation.destination, failed.preparation.destination);
    assert_eq!(a.preparation.plan, failed.preparation.plan);
    let create = |text: &str| CreateSessionRequest {
        preparation_id: Some(a.preparation.id),
        agent_selection: None,
        execution_directory: a.preparation.destination.clone(),
        prompt: InitialPrompt {
            id: PromptId::new(),
            text: text.into(),
            skill_invocations: vec![],
        },
    };
    let (one, two) = tokio::join!(
        remote.create_session(create("First request")),
        remote.create_session(create("Duplicate request"))
    );
    assert_eq!(one.unwrap().session.id, two.unwrap().session.id);
    let start = timeout(Duration::from_secs(2), provider.next_start())
        .await
        .unwrap();
    assert_eq!(start.execution_directory(), a.preparation.destination.path);
    drop(start);
    assert_eq!(remote.list_sessions(None).await.unwrap().len(), 1);
    assert!(
        pair.connecting_client
            .list_sessions(None)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(provider.try_next_start().is_none());
    drop(remote);
    pair.shutdown().await;
}
