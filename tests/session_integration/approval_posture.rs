//! Public Session Approval Posture mutation, runtime delivery, and durability.

use crate::{
    provider_support::ControlledProvider,
    support::{hosted_model, hosted_selection, receive_managed_client_initial_state},
};
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig},
    protocol::{
        Activity, AdmitPromptRequest, AgentId, AgentIdentity, ApprovalPosture, CodexApprovalPolicy,
        CodexSandboxMode, CreateSessionRequest, InitialPrompt, PromptDelivery, PromptId,
        ProviderId, SessionApprovalPosture, SettingMutation, UpdateApprovalPostureRequest,
    },
    provider::{ProviderEvent, ProviderSubagentId},
    server::{self, ServerConfig},
};

#[tokio::test]
async fn approval_posture_pins_resets_follows_settings_and_reaches_session_and_turn_starts() {
    let state = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let channel = "approval-posture-public";
    let config = ServerConfig::new(state.path(), channel)
        .unwrap()
        .with_data_dir(data.path())
        .with_config_dir(config_dir.path());
    let (runtime, mut provider) = ControlledProvider::with_provider(
        ProviderId::new("codex"),
        vec![hosted_model("codex", "gpt-test")],
    );
    let server = server::spawn_with_provider(config.clone(), runtime)
        .await
        .unwrap();
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state.path(), channel)
            .unwrap()
            .with_data_dir(data.path()),
    )
    .await
    .unwrap();
    receive_managed_client_initial_state(&mut client).await;

    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "first".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .unwrap();
    let default = ApprovalPosture::Codex {
        approval_policy: CodexApprovalPolicy::OnRequest,
        sandbox_mode: CodexSandboxMode::WorkspaceWrite,
    };
    assert_eq!(created.session.approval_posture, None);
    let start = provider.next_start().await;
    assert_eq!(start.approval_posture(), Some(&default));
    let mut native = start.succeed(AgentIdentity {
        agent: AgentId::new("codex"),
        selection: hosted_selection("codex", "gpt-test"),
    });
    let turn = native.next_turn().await;
    assert_eq!(
        client
            .read_session(created.session.id)
            .await
            .unwrap()
            .session
            .approval_posture,
        Some(SessionApprovalPosture {
            value: default,
            pinned: false,
        }),
        "native Provider selection publishes its effective posture before the Turn"
    );
    assert_eq!(turn.approval_posture(), Some(&default));
    turn.succeed();
    native.emit(ProviderEvent::TurnCompleted);

    let pinned = ApprovalPosture::Codex {
        approval_policy: CodexApprovalPolicy::Untrusted,
        sandbox_mode: CodexSandboxMode::ReadOnly,
    };
    assert_eq!(
        client
            .update_approval_posture(
                created.session.id,
                UpdateApprovalPostureRequest {
                    posture: Some(pinned.clone()),
                },
            )
            .await
            .unwrap(),
        SessionApprovalPosture {
            value: pinned.clone(),
            pinned: true,
        }
    );
    client
        .mutate_setting(SettingMutation::ProviderCodexApprovalPolicy {
            value: Some(CodexApprovalPolicy::Never),
        })
        .await
        .unwrap();
    client
        .mutate_setting(SettingMutation::ProviderCodexSandboxMode {
            value: Some(CodexSandboxMode::DangerFullAccess),
        })
        .await
        .unwrap();
    assert_eq!(
        client
            .read_session(created.session.id)
            .await
            .unwrap()
            .session
            .approval_posture,
        Some(SessionApprovalPosture {
            value: pinned.clone(),
            pinned: true
        })
    );

    client
        .admit_prompt(
            created.session.id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "second".to_owned(),
                    skill_invocations: Vec::new(),
                },
                delivery: PromptDelivery::Queue,
            },
        )
        .await
        .unwrap();
    let turn = native.next_turn().await;
    assert_eq!(turn.approval_posture(), Some(&pinned));
    turn.succeed();
    native.emit(ProviderEvent::TurnCompleted);

    let followed = ApprovalPosture::Codex {
        approval_policy: CodexApprovalPolicy::Never,
        sandbox_mode: CodexSandboxMode::DangerFullAccess,
    };
    assert_eq!(
        client
            .update_approval_posture(
                created.session.id,
                UpdateApprovalPostureRequest { posture: None },
            )
            .await
            .unwrap(),
        SessionApprovalPosture {
            value: followed.clone(),
            pinned: false
        }
    );
    drop(native);
    drop(client);
    server.shutdown().await.unwrap();

    let (runtime, _provider) = ControlledProvider::with_provider(
        ProviderId::new("codex"),
        vec![hosted_model("codex", "gpt-test")],
    );
    let restarted = server::spawn_with_provider(config, runtime).await.unwrap();
    let restored = crate::support::read_session(restarted.descriptor(), created.session.id).await;
    assert_eq!(
        restored.session.approval_posture,
        Some(SessionApprovalPosture {
            value: followed,
            pinned: false
        })
    );
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_override_for_another_provider_is_rejected() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (runtime, _provider) = ControlledProvider::with_provider(
        ProviderId::new("codex"),
        vec![hosted_model("codex", "gpt-test")],
    );
    let server = server::spawn_with_provider(
        ServerConfig::new(state.path(), "approval-posture-conflict").unwrap(),
        runtime,
    )
    .await
    .unwrap();
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state.path(), "approval-posture-conflict").unwrap(),
    )
    .await
    .unwrap();
    receive_managed_client_initial_state(&mut client).await;
    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: Some(hosted_selection("codex", "gpt-test")),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().into(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "work".into(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .unwrap();
    assert!(
        client
            .update_approval_posture(
                created.session.id,
                UpdateApprovalPostureRequest {
                    posture: Some(ApprovalPosture::Copilot {
                        permissions: suru::protocol::CopilotPermissions::AllowAll
                    })
                },
            )
            .await
            .is_err()
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_pinned_approval_posture_survives_server_restart() {
    let state = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let channel = "approval-posture-pinned-restart";
    let config = ServerConfig::new(state.path(), channel)
        .unwrap()
        .with_data_dir(data.path());
    let model = hosted_model("codex", "gpt-test");
    let (runtime, mut provider) =
        ControlledProvider::with_provider(ProviderId::new("codex"), vec![model.clone()]);
    let server = server::spawn_with_provider(config.clone(), runtime)
        .await
        .unwrap();
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state.path(), channel)
            .unwrap()
            .with_data_dir(data.path()),
    )
    .await
    .unwrap();
    receive_managed_client_initial_state(&mut client).await;
    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: Some(hosted_selection("codex", "gpt-test")),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().into(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "persist".into(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .unwrap();
    let mut native = provider.next_start().await.succeed(AgentIdentity {
        agent: AgentId::new("codex"),
        selection: hosted_selection("codex", "gpt-test"),
    });
    native.next_turn().await.succeed();
    native.emit(ProviderEvent::TurnCompleted);
    let pinned = ApprovalPosture::Codex {
        approval_policy: CodexApprovalPolicy::Untrusted,
        sandbox_mode: CodexSandboxMode::ReadOnly,
    };
    client
        .update_approval_posture(
            created.session.id,
            UpdateApprovalPostureRequest {
                posture: Some(pinned),
            },
        )
        .await
        .unwrap();
    drop(native);
    drop(client);
    server.shutdown().await.unwrap();

    let (runtime, _provider) =
        ControlledProvider::with_provider(ProviderId::new("codex"), vec![model]);
    let restarted = server::spawn_with_provider(config, runtime).await.unwrap();
    let restored = crate::support::read_session(restarted.descriptor(), created.session.id).await;
    assert_eq!(
        restored.session.approval_posture,
        Some(SessionApprovalPosture {
            value: pinned,
            pinned: true
        })
    );
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn subagent_posture_is_inherited_and_cannot_be_independently_mutated() {
    let state = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let channel = "approval-posture-subagent-inheritance";
    let config = ServerConfig::new(state.path(), channel)
        .unwrap()
        .with_data_dir(data.path())
        .with_config_dir(config_dir.path());
    let (runtime, mut provider) = ControlledProvider::with_provider(
        ProviderId::new("codex"),
        vec![hosted_model("codex", "gpt-test")],
    );
    let server = server::spawn_with_provider(config, runtime).await.unwrap();
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state.path(), channel)
            .unwrap()
            .with_data_dir(data.path()),
    )
    .await
    .unwrap();
    receive_managed_client_initial_state(&mut client).await;
    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: Some(hosted_selection("codex", "gpt-test")),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().into(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "delegate".into(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .unwrap();
    let mut native = provider.next_start().await.succeed(AgentIdentity {
        agent: AgentId::new("codex"),
        selection: hosted_selection("codex", "gpt-test"),
    });
    native.next_turn().await.succeed();
    let pinned = ApprovalPosture::Codex {
        approval_policy: CodexApprovalPolicy::Untrusted,
        sandbox_mode: CodexSandboxMode::ReadOnly,
    };
    client
        .update_approval_posture(
            created.session.id,
            UpdateApprovalPostureRequest {
                posture: Some(pinned),
            },
        )
        .await
        .unwrap();
    native
        .emit_and_wait_until_observed(ProviderEvent::SubagentStarted {
            subagent_id: ProviderSubagentId::new("child"),
            name: "Child".into(),
            description: "Inherited posture".into(),
        })
        .await;
    let parent = client.read_session(created.session.id).await.unwrap();
    let child_id = parent
        .activities
        .iter()
        .find_map(|activity| match activity {
            Activity::Subagent { session_id, .. } => Some(*session_id),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        client
            .read_session(child_id)
            .await
            .unwrap()
            .session
            .approval_posture,
        Some(SessionApprovalPosture {
            value: pinned,
            pinned: true,
        })
    );
    let error = client
        .update_approval_posture(
            child_id,
            UpdateApprovalPostureRequest {
                posture: Some(ApprovalPosture::Codex {
                    approval_policy: CodexApprovalPolicy::Never,
                    sandbox_mode: CodexSandboxMode::DangerFullAccess,
                }),
            },
        )
        .await
        .expect_err("a child cannot pin a posture its shared Provider actor will not receive");
    assert!(
        error.to_string().contains("inherited from its parent"),
        "{error}"
    );
    assert!(
        client
            .update_approval_posture(child_id, UpdateApprovalPostureRequest { posture: None },)
            .await
            .is_err(),
        "a child cannot reset away from its parent's pin"
    );

    client
        .mutate_setting(SettingMutation::ProviderCodexApprovalPolicy {
            value: Some(CodexApprovalPolicy::Never),
        })
        .await
        .unwrap();
    assert_eq!(
        client
            .read_session(child_id)
            .await
            .unwrap()
            .session
            .approval_posture,
        Some(SessionApprovalPosture {
            value: pinned,
            pinned: true,
        }),
        "global Settings do not overwrite a native posture inherited from a pinned parent"
    );
    let followed = ApprovalPosture::Codex {
        approval_policy: CodexApprovalPolicy::Never,
        sandbox_mode: CodexSandboxMode::WorkspaceWrite,
    };
    client
        .update_approval_posture(
            created.session.id,
            UpdateApprovalPostureRequest { posture: None },
        )
        .await
        .unwrap();
    assert_eq!(
        client
            .read_session(child_id)
            .await
            .unwrap()
            .session
            .approval_posture,
        Some(SessionApprovalPosture {
            value: followed,
            pinned: false,
        }),
        "resetting the owning Session refreshes every child's inherited reading"
    );
    server.shutdown().await.unwrap();
}
