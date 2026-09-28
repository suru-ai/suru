//! Agent selection: authority, convergence across clients, and turn-boundary capture.

use crate::server_support::PROGRESS_DEADLINE;
use crate::{
    provider_support::ControlledProvider,
    support::{controlled_selection, next_session_update, receive_managed_client_initial_state},
};
use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, SessionEvent},
    protocol::{
        Activity, AdmitPromptRequest, AgentId, AgentIdentity, AgentSelection,
        AgentSelectionOperationId, CreateSessionRequest, Health, InitialPrompt, ModelAvailability,
        ModelCatalog, ModelDescriptor, ModelId, ModelOptionChoice, ModelOptionChoiceId,
        ModelOptionDescriptor, ModelOptionId, ModelOptionKind, ModelOptionRole,
        ModelOptionSelection, ModelOptionValue, PromptDelivery, PromptId, PromptStatus,
        ProviderCatalogStatus, ProviderId, ProviderModelCatalog, SessionChange, SessionRevision,
        SessionSnapshot, SkillId, SkillInvocation, TextSpan, TurnStatus,
        UpdateAgentSelectionRequest,
    },
    provider::{ProviderEvent, ProviderSkillInvocation},
    server::{self, ServerConfig},
    tui::{Application, ApplicationEvent, ApplicationTransition, CommandId, SemanticCommandId},
};
use tokio::time::{Duration, timeout};

#[tokio::test]
async fn agent_selection_changes_do_not_rewrite_an_active_turn_identity() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "mutable-agent-selection-test")
            .expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "mutable-agent-selection-test")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
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
                text: "Keep this Turn on its effective Agent".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create Session");
    let initial_identity = AgentIdentity {
        agent: AgentId::new("codex"),
        selection: AgentSelection {
            provider: ProviderId::new("codex"),
            model: ModelId::new("gpt-initial"),
            options: Vec::new(),
        },
    };
    let mut provider_session = provider
        .next_start()
        .await
        .succeed(initial_identity.clone());
    provider_session.next_turn().await.succeed();

    let mut feed = client
        .subscribe_session(created.session.id)
        .await
        .expect("subscribe to active Session");
    let SessionEvent::Snapshot(active) = feed
        .next()
        .await
        .expect("Session snapshot arrives")
        .expect("Session stream remains valid")
    else {
        panic!("Session stream starts with a snapshot");
    };
    assert_eq!(
        active.session.agent_selection,
        Some(initial_identity.selection.clone())
    );
    assert_eq!(active.turns[0].agent, Some(initial_identity.clone()));

    let next_selection = AgentSelection {
        provider: ProviderId::new("codex"),
        model: ModelId::new("gpt-next"),
        options: Vec::new(),
    };
    let published = server
        .session_event_sink()
        .publish(
            created.session.id,
            vec![SessionChange::AgentSelectionChanged {
                selection: next_selection.clone(),
            }],
        )
        .await
        .expect("publish a later Agent Selection");
    assert_eq!(next_session_update(&mut feed).await, published);

    let changed = client
        .read_session(created.session.id)
        .await
        .expect("read changed Session");
    assert_eq!(changed.session.agent_selection, Some(next_selection));
    assert_eq!(
        changed.turns[0].agent,
        Some(initial_identity),
        "an active Turn retains the effective Agent it began with"
    );

    provider_session.emit(ProviderEvent::TurnCompleted);
    next_session_update(&mut feed).await;
    drop(provider_session);
    drop(feed);
    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn session_creation_makes_the_landing_agent_selection_authoritative() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, _provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "selected-session-create-test")
            .expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let selection = AgentSelection {
        provider: ProviderId::new("controlled"),
        model: ModelId::new("gpt-selected"),
        options: Vec::new(),
    };

    let response = reqwest::Client::new()
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&serde_json::json!({
            "execution_directory": { "path": workspace.path() },
            "prompt": {
                "id": PromptId::new(),
                "text": "Begin with my landing selection",
                "skill_invocations": []
            },
            "agent_selection": selection,
        }))
        .send()
        .await
        .expect("create selected Session");

    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let created = response
        .json::<SessionSnapshot>()
        .await
        .expect("decode selected Session");
    assert_eq!(created.session.agent_selection, Some(selection));

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn confirmed_landing_agent_selection_defaults_new_sessions_after_a_restart() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let first_workspace = tempfile::tempdir().expect("create first valid Workspace");
    let second_workspace = tempfile::tempdir().expect("create second valid Workspace");
    let config = ServerConfig::new(state_dir.path(), "landing-selection-restart-test")
        .expect("configure original server")
        .with_data_dir(data_dir.path());
    let (original_runtime, _original_provider) = ControlledProvider::new();
    let original = server::spawn_with_provider(config.clone(), original_runtime)
        .await
        .expect("spawn original server");
    let original_descriptor = original.descriptor().clone();
    let client = reqwest::Client::new();

    let initial = client
        .post(format!("{}/v1/sessions", original_descriptor.base_url))
        .bearer_auth(&original_descriptor.token)
        .json(&CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: first_workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Use the existing default".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .send()
        .await
        .expect("create Session without a stored preference")
        .error_for_status()
        .expect("Session creation without a stored preference succeeds")
        .json::<SessionSnapshot>()
        .await
        .expect("decode Session without a stored preference");
    assert_eq!(
        initial.session.agent_selection, None,
        "an absent preference must preserve the Provider's current default behavior"
    );

    let selected = controlled_selection("gpt-remembered", "high", "fast");
    let confirmed = client
        .put(format!(
            "{}/v1/landing-agent-selection",
            original_descriptor.base_url
        ))
        .bearer_auth(&original_descriptor.token)
        .json(&selected)
        .send()
        .await
        .expect("confirm landing Agent Selection")
        .error_for_status()
        .expect("landing Agent Selection confirmation succeeds")
        .json::<AgentSelection>()
        .await
        .expect("decode confirmed landing Agent Selection");
    assert_eq!(confirmed, selected);
    original.shutdown().await.expect("stop original server");

    let (replacement_runtime, _replacement_provider) = ControlledProvider::new();
    let replacement = server::spawn_with_provider(config, replacement_runtime)
        .await
        .expect("spawn replacement server");
    let replacement_descriptor = replacement.descriptor().clone();
    let replacement_health = client
        .get(format!("{}/health", replacement_descriptor.base_url))
        .bearer_auth(&replacement_descriptor.token)
        .send()
        .await
        .expect("read replacement server health")
        .error_for_status()
        .expect("replacement server health read succeeds")
        .json::<Health>()
        .await
        .expect("decode replacement server health");
    assert_eq!(
        replacement_health.landing_agent_selection,
        Some(selected.clone())
    );

    let created = client
        .post(format!("{}/v1/sessions", replacement_descriptor.base_url))
        .bearer_auth(&replacement_descriptor.token)
        .json(&CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: second_workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Reuse my remembered selection".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .send()
        .await
        .expect("create Session with the persisted default")
        .error_for_status()
        .expect("Session creation with the persisted default succeeds")
        .json::<SessionSnapshot>()
        .await
        .expect("decode Session with the persisted default");
    assert_eq!(created.session.agent_selection, Some(selected));

    replacement
        .shutdown()
        .await
        .expect("shut down replacement server");
}

#[tokio::test]
async fn agent_selection_commands_are_idempotent_and_converge_across_clients() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, _provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "selection-command-test").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let mut first = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "selection-command-test")
            .expect("configure first client"),
    )
    .await
    .expect("connect first client");
    let mut second = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "selection-command-test")
            .expect("configure second client"),
    )
    .await
    .expect("connect second client");
    receive_managed_client_initial_state(&mut first).await;
    receive_managed_client_initial_state(&mut second).await;
    let initial = AgentSelection {
        provider: ProviderId::new("controlled"),
        model: ModelId::new("gpt-initial"),
        options: Vec::new(),
    };
    let created = first
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: Some(initial),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Wait for a selected Turn".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create selected Session");
    let mut first_feed = first
        .subscribe_session(created.session.id)
        .await
        .expect("subscribe first client");
    let mut second_feed = second
        .subscribe_session(created.session.id)
        .await
        .expect("subscribe second client");
    for feed in [&mut first_feed, &mut second_feed] {
        assert!(matches!(
            feed.next()
                .await
                .expect("snapshot arrives")
                .expect("valid feed"),
            SessionEvent::Snapshot(_)
        ));
    }

    let operation_id = AgentSelectionOperationId::new();
    let selected = controlled_selection("gpt-selected", "high-native", "fast-native");
    let request = UpdateAgentSelectionRequest {
        operation_id,
        selection: selected.clone(),
    };
    assert_eq!(
        first
            .update_agent_selection(created.session.id, request.clone())
            .await
            .expect("accept Agent Selection"),
        selected
    );
    let first_update = next_session_update(&mut first_feed).await;
    let second_update = next_session_update(&mut second_feed).await;
    assert_eq!(first_update, second_update);
    assert_eq!(first_update.revision, SessionRevision(2));
    assert_eq!(
        first_update.changes,
        vec![SessionChange::AgentSelectionChanged {
            selection: selected.clone(),
        }]
    );

    assert_eq!(
        first
            .update_agent_selection(created.session.id, request)
            .await
            .expect("retry exact Agent Selection operation"),
        selected
    );
    assert!(
        timeout(Duration::from_millis(50), first_feed.next())
            .await
            .is_err(),
        "an exact operation retry must not publish another revision"
    );

    let conflict = first
        .update_agent_selection(
            created.session.id,
            UpdateAgentSelectionRequest {
                operation_id,
                selection: AgentSelection {
                    provider: ProviderId::new("controlled"),
                    model: ModelId::new("gpt-conflict"),
                    options: Vec::new(),
                },
            },
        )
        .await
        .expect_err("reject conflicting operation identity reuse");
    assert!(conflict.to_string().contains("operation identity"));

    let final_selection = controlled_selection("gpt-final", "low-native", "standard-native");
    second
        .update_agent_selection(
            created.session.id,
            UpdateAgentSelectionRequest {
                operation_id: AgentSelectionOperationId::new(),
                selection: final_selection.clone(),
            },
        )
        .await
        .expect("accept second client's selection");
    let first_final = next_session_update(&mut first_feed).await;
    let second_final = next_session_update(&mut second_feed).await;
    assert_eq!(first_final, second_final);
    assert_eq!(first_final.revision, SessionRevision(3));
    assert_eq!(
        first
            .read_session(created.session.id)
            .await
            .expect("read converged Session")
            .session
            .agent_selection,
        Some(final_selection)
    );

    drop(first_feed);
    drop(second_feed);
    drop(first);
    drop(second);
    server.shutdown().await.expect("shut down server");
}

fn opaque_cycling_selection(effort: &str) -> AgentSelection {
    AgentSelection {
        provider: ProviderId::new("controlled"),
        model: ModelId::new("gpt-cycle"),
        options: vec![
            ModelOptionSelection {
                id: ModelOptionId::new("reasoning-opaque"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new(effort),
                },
            },
            ModelOptionSelection {
                id: ModelOptionId::new("fast-opaque"),
                value: ModelOptionValue::Toggle { enabled: false },
            },
        ],
    }
}

fn opaque_cycling_catalog() -> ModelCatalog {
    let effort_choice = |id: &str, label: &str| ModelOptionChoice {
        id: ModelOptionChoiceId::new(id),
        label: label.to_owned(),
        description: None,
        availability: ModelAvailability::Available,
    };
    ModelCatalog {
        providers: vec![ProviderModelCatalog {
            provider: ProviderId::new("controlled"),
            display_name: "controlled".to_owned(),
            models: vec![ModelDescriptor {
                provider: ProviderId::new("controlled"),
                id: ModelId::new("gpt-cycle"),
                display_name: "Cycle Native".to_owned(),
                description: "Cycle Native description".to_owned(),
                is_default: true,
                availability: ModelAvailability::Available,
                options: vec![
                    ModelOptionDescriptor {
                        id: ModelOptionId::new("reasoning-opaque"),
                        label: "Effort".to_owned(),
                        description: None,
                        role: ModelOptionRole::ReasoningEffort,
                        kind: ModelOptionKind::Select {
                            choices: vec![
                                effort_choice("low", "Low"),
                                effort_choice("medium", "Medium"),
                                effort_choice("high", "High"),
                            ],
                            default: ModelOptionChoiceId::new("medium"),
                        },
                    },
                    ModelOptionDescriptor {
                        id: ModelOptionId::new("fast-opaque"),
                        label: "Fast".to_owned(),
                        description: None,
                        role: ModelOptionRole::Speed,
                        kind: ModelOptionKind::Toggle { default: false },
                    },
                ],
            }],
            status: ProviderCatalogStatus::Fresh,
        }],
    }
}

#[tokio::test]
async fn rapid_reasoning_cycles_serialize_coalesce_and_converge_across_clients() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, _provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "reasoning-cycle-test").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let mut first = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "reasoning-cycle-test")
            .expect("configure first client"),
    )
    .await
    .expect("connect first client");
    let mut second = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "reasoning-cycle-test")
            .expect("configure second client"),
    )
    .await
    .expect("connect second client");
    receive_managed_client_initial_state(&mut first).await;
    receive_managed_client_initial_state(&mut second).await;
    let created = first
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: Some(opaque_cycling_selection("low")),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Cycle Reasoning Effort rapidly".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create selected Session");
    let session_id = created.session.id;
    let mut first_feed = first
        .subscribe_session(session_id)
        .await
        .expect("subscribe first client");
    let mut second_feed = second
        .subscribe_session(session_id)
        .await
        .expect("subscribe second client");
    for feed in [&mut first_feed, &mut second_feed] {
        assert!(matches!(
            feed.next()
                .await
                .expect("snapshot arrives")
                .expect("valid feed"),
            SessionEvent::Snapshot(_)
        ));
    }

    // Drive the first client's TUI: warm the catalog, then cycle rapidly.
    let mut application = Application::new(workspace.path(), Default::default());
    application
        .handle_event(ApplicationEvent::SessionAttached(created))
        .expect("attach the driving TUI client");
    let ApplicationTransition::ListModels(catalog_request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::ModelList,
        )))
        .expect("open Model picker to warm the catalog")
    else {
        panic!("Model picker should request the catalog");
    };
    application
        .handle_event(ApplicationEvent::ModelsListed {
            request: catalog_request,
            catalog: opaque_cycling_catalog(),
        })
        .expect("cache the Model catalog");
    application
        .handle_terminal_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )))
        .expect("close the warmed Model picker");

    let press = |application: &mut Application| {
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Char('t'),
                KeyModifiers::CONTROL,
            )))
            .expect("press Ctrl+T")
    };
    let ApplicationTransition::UpdateAgentSelection {
        request: first_request,
        ..
    } = press(&mut application)
    else {
        panic!("the first cycle should dispatch one selection request");
    };
    assert_eq!(first_request.selection, opaque_cycling_selection("medium"));
    assert_eq!(press(&mut application), ApplicationTransition::Continue);
    assert_eq!(press(&mut application), ApplicationTransition::Continue);

    // Serialized transport: the first request settles against the real
    // server before the coalesced latest selection is dispatched.
    let accepted = first
        .update_agent_selection(session_id, first_request.clone())
        .await
        .expect("accept the first cycle");
    let ApplicationTransition::UpdateAgentSelection {
        request: coalesced_request,
        ..
    } = application
        .handle_event(ApplicationEvent::AgentSelectionUpdated {
            operation_id: first_request.operation_id,
            selection: accepted,
        })
        .expect("settle the first cycle")
    else {
        panic!("settling should flush the coalesced latest selection");
    };
    assert_eq!(
        coalesced_request.selection,
        opaque_cycling_selection("low"),
        "three rapid presses wrap back to Low and skip the intermediate High",
    );
    let accepted = first
        .update_agent_selection(session_id, coalesced_request.clone())
        .await
        .expect("accept the coalesced cycle");
    assert_eq!(
        application
            .handle_event(ApplicationEvent::AgentSelectionUpdated {
                operation_id: coalesced_request.operation_id,
                selection: accepted,
            })
            .expect("settle the coalesced cycle"),
        ApplicationTransition::Continue
    );

    // Both observers see exactly the accepted selections, in acceptance
    // order, and the skipped intermediate choice never reaches the wire.
    let first_updates = [
        next_session_update(&mut first_feed).await,
        next_session_update(&mut first_feed).await,
    ];
    let second_updates = [
        next_session_update(&mut second_feed).await,
        next_session_update(&mut second_feed).await,
    ];
    assert_eq!(first_updates, second_updates);
    assert_eq!(first_updates[0].revision, SessionRevision(2));
    assert_eq!(first_updates[1].revision, SessionRevision(3));
    assert_eq!(
        first_updates[0].changes,
        vec![SessionChange::AgentSelectionChanged {
            selection: opaque_cycling_selection("medium"),
        }]
    );
    assert_eq!(
        first_updates[1].changes,
        vec![SessionChange::AgentSelectionChanged {
            selection: opaque_cycling_selection("low"),
        }]
    );

    // A stale idempotent replay produces no new revision and cannot
    // overwrite the converged state in the driving client.
    let replayed = first
        .update_agent_selection(session_id, first_request.clone())
        .await
        .expect("replay the settled operation");
    assert_eq!(replayed, opaque_cycling_selection("medium"));
    assert_eq!(
        application
            .handle_event(ApplicationEvent::AgentSelectionUpdated {
                operation_id: first_request.operation_id,
                selection: replayed,
            })
            .expect("ignore the stale replay"),
        ApplicationTransition::Continue
    );
    assert!(
        timeout(Duration::from_millis(50), first_feed.next())
            .await
            .is_err(),
        "an idempotent replay must not publish another revision"
    );

    for update in first_updates {
        application
            .handle_event(ApplicationEvent::Session(SessionEvent::Updated(update)))
            .expect("apply the acceptance stream to the driving client");
    }
    assert_eq!(
        first
            .read_session(session_id)
            .await
            .expect("read converged Session")
            .session
            .agent_selection,
        Some(opaque_cycling_selection("low"))
    );

    drop(first_feed);
    drop(second_feed);
    drop(first);
    drop(second);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn concurrent_clients_converge_in_server_acceptance_order() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, _provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "concurrent-selection-test").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let mut first = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "concurrent-selection-test")
            .expect("configure first client"),
    )
    .await
    .expect("connect first client");
    let mut second = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "concurrent-selection-test")
            .expect("configure second client"),
    )
    .await
    .expect("connect second client");
    receive_managed_client_initial_state(&mut first).await;
    receive_managed_client_initial_state(&mut second).await;
    let created = first
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: Some(AgentSelection {
                provider: ProviderId::new("controlled"),
                model: ModelId::new("initial"),
                options: Vec::new(),
            }),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Wait while clients select concurrently".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create selected Session");
    let mut first_feed = first
        .subscribe_session(created.session.id)
        .await
        .expect("subscribe first client");
    let mut second_feed = second
        .subscribe_session(created.session.id)
        .await
        .expect("subscribe second client");
    for feed in [&mut first_feed, &mut second_feed] {
        feed.next()
            .await
            .expect("snapshot arrives")
            .expect("snapshot is valid");
    }
    let first_selection = AgentSelection {
        provider: ProviderId::new("controlled"),
        model: ModelId::new("first-client"),
        options: Vec::new(),
    };
    let second_selection = AgentSelection {
        provider: ProviderId::new("controlled"),
        model: ModelId::new("second-client"),
        options: Vec::new(),
    };

    let (first_result, second_result) = tokio::join!(
        first.update_agent_selection(
            created.session.id,
            UpdateAgentSelectionRequest {
                operation_id: AgentSelectionOperationId::new(),
                selection: first_selection,
            },
        ),
        second.update_agent_selection(
            created.session.id,
            UpdateAgentSelectionRequest {
                operation_id: AgentSelectionOperationId::new(),
                selection: second_selection,
            },
        ),
    );
    first_result.expect("first concurrent command is accepted");
    second_result.expect("second concurrent command is accepted");
    let first_updates = [
        next_session_update(&mut first_feed).await,
        next_session_update(&mut first_feed).await,
    ];
    let second_updates = [
        next_session_update(&mut second_feed).await,
        next_session_update(&mut second_feed).await,
    ];
    assert_eq!(first_updates, second_updates);
    assert_eq!(first_updates[0].revision, SessionRevision(2));
    assert_eq!(first_updates[1].revision, SessionRevision(3));
    let accepted_last = first_updates[1]
        .changes
        .iter()
        .find_map(|change| match change {
            SessionChange::AgentSelectionChanged { selection } => Some(selection.clone()),
            _ => None,
        })
        .expect("last accepted command publishes its Selection");
    assert_eq!(
        first
            .read_session(created.session.id)
            .await
            .expect("read converged Session")
            .session
            .agent_selection,
        Some(accepted_last)
    );

    drop(first_feed);
    drop(second_feed);
    drop(first);
    drop(second);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn turn_boundaries_capture_the_latest_selection_while_steers_keep_the_active_turn() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "selection-turn-order-test").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "selection-turn-order-test")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_managed_client_initial_state(&mut client).await;
    let first_selection = controlled_selection("model-a", "high-native", "standard-native");
    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: Some(first_selection.clone()),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Begin on A".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create selected Session");
    let default_identity = AgentIdentity {
        agent: AgentId::new("controlled"),
        selection: AgentSelection {
            provider: ProviderId::new("controlled"),
            model: ModelId::new("provider-default"),
            options: Vec::new(),
        },
    };
    let mut provider_session = provider.next_start().await.succeed(default_identity);
    let first_turn = provider_session.next_turn().await;
    assert_eq!(first_turn.selection(), &first_selection);
    first_turn.succeed();

    let second_selection = controlled_selection("model-b", "low-native", "fast-native");
    client
        .update_agent_selection(
            created.session.id,
            UpdateAgentSelectionRequest {
                operation_id: AgentSelectionOperationId::new(),
                selection: second_selection,
            },
        )
        .await
        .expect("select B while A is active");
    client
        .admit_prompt(
            created.session.id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Steer the A Turn".to_owned(),
                    skill_invocations: Vec::new(),
                    attachments: Vec::new(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("admit steer Prompt");
    let steer = provider_session.next_steer().await;
    assert_eq!(steer.prompt(), "Steer the A Turn");
    steer.succeed();
    client
        .admit_prompt(
            created.session.id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Queue a new Turn".to_owned(),
                    skill_invocations: Vec::new(),
                    attachments: Vec::new(),
                },
                delivery: PromptDelivery::Queue,
            },
        )
        .await
        .expect("queue next Prompt");
    let final_selection = controlled_selection("model-c", "high-native", "fast-native");
    client
        .update_agent_selection(
            created.session.id,
            UpdateAgentSelectionRequest {
                operation_id: AgentSelectionOperationId::new(),
                selection: final_selection.clone(),
            },
        )
        .await
        .expect("select C before queued Turn begins");

    provider_session.emit(ProviderEvent::TurnCompleted);
    let queued_turn = provider_session.next_turn().await;
    assert_eq!(queued_turn.prompt(), "Queue a new Turn");
    assert_eq!(queued_turn.selection(), &final_selection);
    queued_turn.succeed();
    let active = client
        .read_session(created.session.id)
        .await
        .expect("read both Turn identities");
    assert_eq!(
        active.turns[0].agent.as_ref().map(|agent| &agent.selection),
        Some(&first_selection)
    );
    assert_eq!(
        active.turns[1].agent.as_ref().map(|agent| &agent.selection),
        Some(&final_selection)
    );

    provider_session.emit(ProviderEvent::TurnCompleted);
    drop(provider_session);
    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn provider_effective_selection_reconciles_the_active_turn_with_visible_activity() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "effective-selection-test").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "effective-selection-test")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_managed_client_initial_state(&mut client).await;
    let requested = AgentSelection {
        provider: ProviderId::new("controlled"),
        model: ModelId::new("requested-model"),
        options: vec![
            ModelOptionSelection {
                id: ModelOptionId::new("reasoning-opaque"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("high-opaque"),
                },
            },
            ModelOptionSelection {
                id: ModelOptionId::new("speed-opaque"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("standard-opaque"),
                },
            },
        ],
    };
    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: Some(requested.clone()),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Use the selected Model".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create selected Session");
    let mut provider_session = provider.next_start().await.succeed(AgentIdentity {
        agent: AgentId::new("controlled"),
        selection: requested.clone(),
    });
    provider_session.next_turn().await.succeed();
    let mut feed = client
        .subscribe_session(created.session.id)
        .await
        .expect("subscribe to active Session");
    let SessionEvent::Snapshot(before) = feed
        .next()
        .await
        .expect("snapshot arrives")
        .expect("snapshot is valid")
    else {
        panic!("feed begins with a snapshot");
    };
    let turn_id = before.turns[0].id;
    let effective = AgentSelection {
        provider: ProviderId::new("controlled"),
        model: ModelId::new("effective-model"),
        options: vec![
            ModelOptionSelection {
                id: ModelOptionId::new("reasoning-opaque"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("low-opaque"),
                },
            },
            ModelOptionSelection {
                id: ModelOptionId::new("speed-opaque"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new("fast-opaque"),
                },
            },
        ],
    };

    provider_session.emit(ProviderEvent::AgentSelectionChanged {
        selection: effective.clone(),
    });
    let update = next_session_update(&mut feed).await;
    assert_eq!(update.revision, SessionRevision(before.revision.0 + 1));
    assert!(update.changes.iter().any(|change| matches!(
        change,
        SessionChange::AgentSelectionChanged { selection } if selection == &effective
    )));
    assert!(update.changes.iter().any(|change| matches!(
        change,
        SessionChange::TurnAgentChanged { turn_id: changed_turn_id, agent }
            if *changed_turn_id == turn_id && agent.selection == effective
    )));
    assert!(update.changes.iter().any(|change| matches!(
        change,
        SessionChange::ActivityAdded { activity: Activity::Status { turn_id: activity_turn_id, text, .. } }
            if *activity_turn_id == turn_id
                && text.contains("requested-model")
                && text.contains("effective-model")
    )));
    for (option_id, requested_choice, effective_choice) in [
        ("reasoning-opaque", "high-opaque", "low-opaque"),
        ("speed-opaque", "standard-opaque", "fast-opaque"),
    ] {
        assert!(update.changes.iter().any(|change| matches!(
            change,
            SessionChange::ActivityAdded { activity: Activity::Status { turn_id: activity_turn_id, text, .. } }
                if *activity_turn_id == turn_id
                    && text.contains(option_id)
                    && text.contains(requested_choice)
                    && text.contains(effective_choice)
        )));
    }
    let reconciled = client
        .read_session(created.session.id)
        .await
        .expect("read reconciled Session");
    assert_eq!(reconciled.session.agent_selection, Some(effective.clone()));
    assert_eq!(
        reconciled.turns[0]
            .agent
            .as_ref()
            .map(|agent| &agent.selection),
        Some(&effective)
    );

    provider_session.emit(ProviderEvent::TurnCompleted);
    drop(feed);
    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn rejected_selection_fails_visibly_and_prepares_a_fresh_prompt_for_retry() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "selection-rejection-test").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "selection-rejection-test")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_managed_client_initial_state(&mut client).await;
    let selection = AgentSelection {
        provider: ProviderId::new("controlled"),
        model: ModelId::new("unavailable-model"),
        options: Vec::new(),
    };
    let original_prompt_id = PromptId::new();
    let invocation = SkillInvocation {
        skill_id: SkillId::new("safe-retry-id"),
        name: "retry".to_owned(),
        scope: Some("Workspace".to_owned()),
        span: TextSpan { start: 0, end: 6 },
    };
    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: Some(selection.clone()),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: original_prompt_id,
                text: "$retry deliberately".to_owned(),
                skill_invocations: vec![invocation.clone()],
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create selected Session");
    let mut provider_session = provider.next_start().await.succeed(AgentIdentity {
        agent: AgentId::new("controlled"),
        selection: selection.clone(),
    });
    let turn = provider_session.next_turn().await;
    assert_eq!(turn.selection(), &selection);
    let provider_invocation = ProviderSkillInvocation {
        skill_id: invocation.skill_id.clone(),
        spans: vec![invocation.span],
    };
    assert_eq!(
        turn.skill_invocations(),
        std::slice::from_ref(&provider_invocation)
    );
    turn.reject_selection("selected Model is unavailable");

    let failed = timeout(PROGRESS_DEADLINE, async {
        loop {
            let snapshot = client
                .read_session(created.session.id)
                .await
                .expect("read rejected Session");
            if snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.status == TurnStatus::Failed)
            {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("selection rejection is projected");
    assert_eq!(failed.session.agent_selection, Some(selection.clone()));
    assert_eq!(
        failed.session.agent_selection_availability,
        ModelAvailability::Unavailable
    );
    assert_eq!(failed.turns.len(), 1);
    assert_eq!(failed.turns[0].status, TurnStatus::Failed);
    assert!(failed.activities.iter().any(|activity| matches!(
        activity,
        Activity::Error { turn_id, text, .. }
            if *turn_id == failed.turns[0].id
                && text.contains("selected Model is unavailable")
    )));
    assert_eq!(failed.prompts.len(), 2);
    assert_eq!(failed.prompts[0].id, original_prompt_id);
    assert_eq!(failed.prompts[0].status, PromptStatus::Delivered);
    assert_ne!(failed.prompts[1].id, original_prompt_id);
    assert_eq!(failed.prompts[1].text, "$retry deliberately");
    assert_eq!(
        failed.prompts[1].skill_invocations,
        vec![invocation.clone()]
    );
    assert_eq!(failed.prompts[1].status, PromptStatus::Pending);
    assert_eq!(failed.messages.len(), 1, "failed Turn history is retained");

    let first_retry_prompt_id = failed.prompts[1].id;
    let repeated_operation_id = AgentSelectionOperationId::new();
    client
        .update_agent_selection(
            created.session.id,
            UpdateAgentSelectionRequest {
                operation_id: repeated_operation_id,
                selection: selection.clone(),
            },
        )
        .await
        .expect("deliberately retry the same Model once");
    let repeated = timeout(PROGRESS_DEADLINE, provider_session.next_turn())
        .await
        .expect("first use of the operation schedules the restored Prompt");
    assert_eq!(repeated.prompt(), "$retry deliberately");
    assert_eq!(
        repeated.skill_invocations(),
        std::slice::from_ref(&provider_invocation)
    );
    repeated.reject_selection("selected Model remains unavailable");

    let failed_again = timeout(PROGRESS_DEADLINE, async {
        loop {
            let snapshot = client
                .read_session(created.session.id)
                .await
                .expect("read repeatedly rejected Session");
            if snapshot.turns.len() == 2
                && snapshot.turns[1].status == TurnStatus::Failed
                && snapshot.prompts.len() == 3
            {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("repeated selection rejection is projected");
    assert_eq!(failed_again.turns[1].prompt_id, Some(first_retry_prompt_id));
    assert_eq!(
        failed_again.prompts[2].skill_invocations,
        vec![invocation.clone()]
    );
    let retry_prompt_id = failed_again.prompts[2].id;

    client
        .update_agent_selection(
            created.session.id,
            UpdateAgentSelectionRequest {
                operation_id: repeated_operation_id,
                selection: selection.clone(),
            },
        )
        .await
        .expect("retry the exact prior Agent Selection operation");
    assert!(
        timeout(Duration::from_millis(100), provider_session.next_turn())
            .await
            .is_err(),
        "an exact operation retry must not schedule a rejected Prompt"
    );

    let retry_selection = AgentSelection {
        provider: ProviderId::new("controlled"),
        model: ModelId::new("available-model"),
        options: Vec::new(),
    };
    client
        .update_agent_selection(
            created.session.id,
            UpdateAgentSelectionRequest {
                operation_id: AgentSelectionOperationId::new(),
                selection: retry_selection.clone(),
            },
        )
        .await
        .expect("select an available Model for deliberate retry");
    let retry = timeout(PROGRESS_DEADLINE, provider_session.next_turn())
        .await
        .expect("restored Prompt is scheduled after Agent Selection recovery");
    assert_eq!(retry.prompt(), "$retry deliberately");
    assert_eq!(retry.skill_invocations(), &[provider_invocation]);
    assert_eq!(retry.selection(), &retry_selection);
    retry.succeed();

    let retried = client
        .read_session(created.session.id)
        .await
        .expect("read retried Session");
    assert_eq!(retried.turns.len(), 3);
    assert_eq!(retried.turns[2].prompt_id, Some(retry_prompt_id));
    assert_eq!(retried.turns[2].status, TurnStatus::Active);
    assert_eq!(
        retried.turns[2]
            .agent
            .as_ref()
            .map(|agent| &agent.selection),
        Some(&retry_selection)
    );
    assert_eq!(
        retried.session.agent_selection_availability,
        ModelAvailability::Available
    );

    drop(provider_session);
    drop(client);
    server.shutdown().await.expect("shut down server");
}
