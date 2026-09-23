//! Real repository preparation shared by native Provider boundary checks.
use crate::server_support::PROGRESS_DEADLINE;
use std::path::Path;
use suru::{managed_client::ManagedClient, protocol::*};

pub async fn prepare(client: &ManagedClient, source: &Path, provider: &str) -> PreparedCheckout {
    for args in [
        vec!["init", "-b", "main"],
        vec![
            "-c",
            "user.name=Suru Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "source",
        ],
    ] {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(source)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let result = client
        .prepare_checkout(PrepareCheckoutRequest {
            id: Default::default(),
            source: ExecutionDirectory {
                path: source.to_owned(),
            },
            prompt: PreparationPrompt {
                text: "Native prepared startup".to_owned(),
                skill_invocations: vec![],
            },
            provider: ProviderId::new(provider),
        })
        .await
        .unwrap();
    assert_eq!(result.error, None);
    result.preparation
}
pub fn creation(plan: &PreparedCheckout) -> CreateSessionRequest {
    CreateSessionRequest {
        preparation_id: Some(plan.id),
        agent_selection: None,
        execution_directory: plan.destination.clone(),
        prompt: InitialPrompt {
            id: PromptId::new(),
            text: "Work from this prepared root".to_owned(),
            skill_invocations: vec![],
        },
    }
}

pub async fn settled(client: &ManagedClient, session: SessionId, index: usize) -> SessionSnapshot {
    let mut feed = client.subscribe_session(session).await.unwrap();
    tokio::time::timeout(PROGRESS_DEADLINE, async {
        loop {
            let snapshot = client.read_session(session).await.unwrap();
            if let Some(turn) = snapshot.turns.get(index)
                && turn.status != TurnStatus::Active
            {
                assert_eq!(
                    turn.status,
                    TurnStatus::Completed,
                    "{:?}",
                    snapshot.activities
                );
                return snapshot;
            }
            feed.next().await.unwrap().unwrap();
        }
    })
    .await
    .expect("native recovered Turn settles")
}
pub async fn recover(client: &ManagedClient, session: SessionId, checkout: &Path) {
    settled(client, session, 0).await;
    std::fs::remove_dir_all(checkout).unwrap();
    client
        .admit_prompt(
            session,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Resume after Worktree recovery".to_owned(),
                    skill_invocations: vec![],
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .unwrap();
    settled(client, session, 1).await;
    assert!(checkout.is_dir());
}
