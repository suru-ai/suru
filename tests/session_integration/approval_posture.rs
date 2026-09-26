//! Public Session Approval Posture mutation, runtime delivery, and durability.

use crate::{
    provider_support::ControlledProvider,
    support::{hosted_model, hosted_selection, receive_managed_client_initial_state},
};
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig},
    protocol::{
        Activity, AdmitPromptRequest, AgentId, AgentIdentity, Approval, ApprovalId,
        ApprovalOutcome, ApprovalPosture, ApprovalPostureApplication, ApprovalSubject,
        CodexApprovalPolicy, CodexSandboxMode, CreateSessionRequest, Decision, InitialPrompt,
        PromptDelivery, PromptId, ProviderId, SessionApprovalPosture, SettingMutation,
        UpdateApprovalPostureRequest,
    },
    provider::{ProviderEvent, ProviderSubagentId},
    server::{self, ServerConfig},
};

#[tokio::test]
async fn active_posture_update_reports_next_turn_and_leaves_existing_approval_answerable() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (runtime, mut provider) = ControlledProvider::with_provider(
        ProviderId::new("codex"),
        vec![hosted_model("codex", "gpt-test")],
    );
    let server = server::spawn_with_provider(
        ServerConfig::new(state.path(), "approval-posture-live").unwrap(),
        runtime,
    )
    .await
    .unwrap();
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state.path(), "approval-posture-live").unwrap(),
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
    let mut native = provider.next_start().await.succeed(AgentIdentity {
        agent: AgentId::new("codex"),
        selection: hosted_selection("codex", "gpt-test"),
    });
    native.next_turn().await.succeed();
    let approval = Approval {
        id: ApprovalId::new(),
        subject: ApprovalSubject::Command {
            command: "cargo nextest run".into(),
            cwd: None,
            actions: Vec::new(),
        },
        reason: Some("run checks".into()),
    };
    native
        .emit_and_wait_until_observed(ProviderEvent::ApprovalRequested {
            approval: approval.clone(),
            tool_activity_id: None,
        })
        .await;
    native.gate_posture_updates();
    let automatic = ApprovalPosture::Codex {
        approval_policy: CodexApprovalPolicy::Never,
        sandbox_mode: CodexSandboxMode::DangerFullAccess,
    };
    let update = client.update_approval_posture(
        created.session.id,
        UpdateApprovalPostureRequest {
            posture: Some(automatic),
        },
    );
    let native_update = async {
        let update = native.next_posture_update().await;
        assert_eq!(update.posture, automatic);
        assert!(update.has_active_work);
        assert_eq!(
            crate::support::read_session(server.descriptor(), created.session.id)
                .await
                .session
                .approval_posture
                .unwrap()
                .application,
            ApprovalPostureApplication::Applying,
            "the requested posture is not advertised as applied before native acknowledgement"
        );
        update.next_turn();
    };
    let (updated, ()) = tokio::join!(update, native_update);
    assert_eq!(
        updated.unwrap().application,
        ApprovalPostureApplication::NextTurn
    );
    let snapshot = client.read_session(created.session.id).await.unwrap();
    assert!(snapshot.pending_approvals.contains(&approval.id));
    assert!(snapshot.activities.iter().any(|activity| matches!(
        activity,
        Activity::Approval { approval: stored, outcome: ApprovalOutcome::Pending, .. }
            if stored.id == approval.id
    )));

    let decision = client.submit_decision(created.session.id, approval.id, Decision::Accept);
    let delivered = async {
        let (id, value) = native.next_decision().await;
        assert_eq!((id, value), (approval.id, Decision::Accept));
    };
    let (decided, ()) = tokio::join!(decision, delivered);
    decided.unwrap();

    native
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    client
        .admit_prompt(
            created.session.id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "next".into(),
                    skill_invocations: Vec::new(),
                },
                delivery: PromptDelivery::Queue,
            },
        )
        .await
        .unwrap();
    let next = native.next_turn().await;
    assert_eq!(next.approval_posture(), Some(&automatic));
    next.succeed();
    assert_eq!(
        client
            .read_session(created.session.id)
            .await
            .unwrap()
            .session
            .approval_posture
            .unwrap()
            .application,
        ApprovalPostureApplication::Applied
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_unpinned_active_session_adopts_server_posture_without_losing_its_delay_on_reads() {
    let state = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (runtime, mut provider) = ControlledProvider::with_provider(
        ProviderId::new("codex"),
        vec![hosted_model("codex", "gpt-test")],
    );
    let server = server::spawn_with_provider(
        ServerConfig::new(state.path(), "approval-posture-setting-live")
            .unwrap()
            .with_config_dir(config_dir.path()),
        runtime,
    )
    .await
    .unwrap();
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state.path(), "approval-posture-setting-live").unwrap(),
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
    let mut native = provider.next_start().await.succeed(AgentIdentity {
        agent: AgentId::new("codex"),
        selection: hosted_selection("codex", "gpt-test"),
    });
    native.next_turn().await.succeed();
    native.gate_posture_updates();

    let setting = client.mutate_setting(SettingMutation::ProviderCodexSandboxMode {
        value: Some(CodexSandboxMode::ReadOnly),
    });
    let apply = async {
        let update = native.next_posture_update().await;
        assert_eq!(
            update.posture,
            ApprovalPosture::Codex {
                approval_policy: CodexApprovalPolicy::OnRequest,
                sandbox_mode: CodexSandboxMode::ReadOnly,
            }
        );
        assert!(update.has_active_work);
        update.next_turn();
    };
    let (setting, ()) = tokio::join!(setting, apply);
    setting.unwrap();
    let delayed = client.read_session(created.session.id).await.unwrap();
    assert_eq!(
        delayed.session.approval_posture.unwrap().application,
        ApprovalPostureApplication::NextTurn
    );

    client
        .mutate_setting(SettingMutation::AppearanceShowIcons { value: Some(false) })
        .await
        .unwrap();
    assert_eq!(
        client
            .read_session(created.session.id)
            .await
            .unwrap()
            .session
            .approval_posture
            .unwrap()
            .application,
        ApprovalPostureApplication::NextTurn,
        "unrelated Settings adoption and hydration preserve the delayed native state"
    );
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_late_settings_batch_ack_cannot_restore_a_delay_after_the_next_turn_applies() {
    let state = tempfile::tempdir().unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let channel = "approval-posture-ordered-ack";
    let (runtime, mut provider) = ControlledProvider::with_provider(
        ProviderId::new("codex"),
        vec![hosted_model("codex", "gpt-test")],
    );
    let server = server::spawn_with_provider(
        ServerConfig::new(state.path(), channel)
            .unwrap()
            .with_config_dir(config_dir.path()),
        runtime,
    )
    .await
    .unwrap();
    let config = ManagedClientConfig::new(state.path(), channel).unwrap();
    let mut settings_client = ManagedClient::connect(config.clone()).await.unwrap();
    receive_managed_client_initial_state(&mut settings_client).await;
    let mut turn_client = ManagedClient::connect(config).await.unwrap();
    receive_managed_client_initial_state(&mut turn_client).await;

    let create = |text: &str| CreateSessionRequest {
        preparation_id: None,
        agent_selection: Some(hosted_selection("codex", "gpt-test")),
        execution_directory: suru::protocol::ExecutionDirectory {
            path: workspace.path().into(),
        },
        prompt: InitialPrompt {
            id: PromptId::new(),
            text: text.into(),
            skill_invocations: Vec::new(),
        },
    };
    let first = turn_client.create_session(create("first")).await.unwrap();
    let mut first_native = provider.next_start().await.succeed(AgentIdentity {
        agent: AgentId::new("codex"),
        selection: hosted_selection("codex", "gpt-test"),
    });
    first_native.next_turn().await.succeed();
    let second = turn_client.create_session(create("second")).await.unwrap();
    let mut second_native = provider.next_start().await.succeed(AgentIdentity {
        agent: AgentId::new("codex"),
        selection: hosted_selection("codex", "gpt-test"),
    });
    second_native.next_turn().await.succeed();
    first_native.drain_posture_updates();
    second_native.drain_posture_updates();
    first_native.gate_posture_updates();
    second_native.gate_posture_updates();

    let setting = settings_client.mutate_setting(SettingMutation::ProviderCodexSandboxMode {
        value: Some(CodexSandboxMode::ReadOnly),
    });
    let actors = async {
        let first_update = tokio::time::timeout(
            crate::server_support::PROGRESS_DEADLINE,
            first_native.next_posture_update(),
        )
        .await
        .expect("first Session receives its Settings posture update");
        let second_update = tokio::time::timeout(
            crate::server_support::PROGRESS_DEADLINE,
            second_native.next_posture_update(),
        )
        .await
        .expect("second Session receives its Settings posture update");
        first_update.next_turn();
        first_native
            .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
            .await;
        turn_client
            .admit_prompt(
                first.session.id,
                AdmitPromptRequest {
                    prompt: InitialPrompt {
                        id: PromptId::new(),
                        text: "apply now".into(),
                        skill_invocations: Vec::new(),
                    },
                    delivery: PromptDelivery::Queue,
                },
            )
            .await
            .unwrap();
        first_native.next_turn().await.succeed();
        tokio::time::timeout(crate::server_support::PROGRESS_DEADLINE, async {
            loop {
                let posture = crate::support::read_session(server.descriptor(), first.session.id)
                    .await
                    .session
                    .approval_posture
                    .unwrap();
                if posture.application == ApprovalPostureApplication::Applied {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the first Session records its successfully applied next Turn");
        second_update.next_turn();
    };
    let (setting, ()) = tokio::join!(setting, actors);
    setting.unwrap();
    assert_eq!(
        turn_client
            .read_session(first.session.id)
            .await
            .unwrap()
            .session
            .approval_posture
            .unwrap()
            .application,
        ApprovalPostureApplication::Applied,
        "the other Session's late Settings acknowledgement cannot restore NextTurn"
    );
    assert_eq!(
        turn_client
            .read_session(second.session.id)
            .await
            .unwrap()
            .session
            .approval_posture
            .unwrap()
            .application,
        ApprovalPostureApplication::NextTurn
    );
    server.shutdown().await.unwrap();
}

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
            application: suru::protocol::ApprovalPostureApplication::Applied,
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
                    posture: Some(pinned),
                },
            )
            .await
            .unwrap(),
        SessionApprovalPosture {
            value: pinned,
            pinned: true,
            application: suru::protocol::ApprovalPostureApplication::Applied,
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
            value: pinned,
            pinned: true,
            application: suru::protocol::ApprovalPostureApplication::Applied,
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
            value: followed,
            pinned: false,
            application: suru::protocol::ApprovalPostureApplication::Applied,
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
            pinned: false,
            application: suru::protocol::ApprovalPostureApplication::Applied,
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
            pinned: true,
            application: suru::protocol::ApprovalPostureApplication::Applied,
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
            delegation: None,
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
            application: suru::protocol::ApprovalPostureApplication::Applied,
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
            application: suru::protocol::ApprovalPostureApplication::Applied,
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
            application: suru::protocol::ApprovalPostureApplication::Applied,
        }),
        "resetting the owning Session refreshes every child's inherited reading"
    );
    server.shutdown().await.unwrap();
}
