//! Real repository preparation shared by native Provider boundary checks.
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
            description: "Native prepared startup".to_owned(),
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
