//! Prompt admission: ordering, idempotent retries, mutation, and rejection.

use crate::server_support::PROGRESS_DEADLINE;
use crate::{
    failing_provider_support::spawn_with_failing_provider,
    provider_support::ControlledProvider,
    support::{read_session_at_least_revision, receive_managed_client_initial_state},
};
use eventsource_stream::Eventsource;
use futures_util::{StreamExt, future::join_all};
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, SessionEvent},
    protocol::{
        Activity, ActivityId, AdmitPromptRequest, AgentId, AgentIdentity, AgentSelection,
        CreateSessionRequest, InitialPrompt, Message, MessageId, MessageRole, MessageStatus,
        ModelId, Prompt, PromptDelivery, PromptId, PromptOrder, PromptStatus, ProviderId,
        SESSION_UPDATED_EVENT, SessionChange, SessionError, SessionErrorCode, SessionRevision,
        SessionSnapshot, SessionStatus, SessionUpdate, SkillId, SkillInvocation, SkillMarkerSpan,
        Turn, TurnId, TurnStatus,
    },
    provider::{ProviderEvent, ProviderSkillInvocation},
    server::{self, AgentOutput, ServerConfig},
};
use tokio::time::{Duration, timeout};

#[tokio::test]
async fn authenticated_creation_returns_pending_before_async_provider_failure() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace_parent = tempfile::tempdir().expect("create workspace parent");
    let workspace = workspace_parent.path().join("workspace");
    std::fs::create_dir(&workspace).expect("create workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "session-create-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let prompt_id = PromptId::new();

    let client = reqwest::Client::new();
    let response = client
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace_parent.path().join(".").join("workspace"),
            },
            prompt: InitialPrompt {
                id: prompt_id,
                text: "Explain this workspace".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .send()
        .await
        .expect("create Session");

    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let snapshot = response
        .json::<SessionSnapshot>()
        .await
        .expect("decode created Session snapshot");
    assert_eq!(
        snapshot.session.workspace.path,
        suru::paths::canonical(&workspace).expect("canonicalize expected Workspace")
    );
    assert_eq!(snapshot.session.agent_selection, None);
    // The Prompt was admitted to begin a Turn, so the Session is already at
    // work over it (ADR 0024).
    assert_eq!(snapshot.session.status, SessionStatus::Active);
    assert!(snapshot.session.working_since.is_some());
    assert_eq!(snapshot.prompts.len(), 1);
    assert_eq!(snapshot.prompts[0].id, prompt_id);
    assert_eq!(snapshot.prompts[0].status, PromptStatus::Pending);
    assert!(snapshot.turns.is_empty());
    assert!(snapshot.messages.is_empty());
    assert!(snapshot.activities.is_empty());

    let failed = read_session_at_least_revision(
        &client,
        &descriptor,
        snapshot.session.id,
        SessionRevision(2),
    )
    .await;
    assert_eq!(failed.prompts[0].status, PromptStatus::Delivered);
    assert_eq!(failed.turns.len(), 1);
    assert_eq!(failed.turns[0].prompt_id, Some(prompt_id));
    assert_eq!(failed.turns[0].status, TurnStatus::Failed);
    assert_eq!(failed.messages.len(), 1);
    assert_eq!(failed.messages[0].role, MessageRole::User);
    assert_eq!(failed.messages[0].content, "Explain this workspace");
    assert_eq!(failed.activities.len(), 1);
    assert!(matches!(
        &failed.activities[0],
        Activity::Error { text, .. } if text.contains("Provider startup failed")
    ));
    assert!(
        failed
            .messages
            .iter()
            .all(|message| message.role != MessageRole::Agent),
        "an unavailable Agent must not be represented by a synthetic Agent Message"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn client_generated_prompt_ids_make_session_creation_retries_idempotent() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid workspace");
    let other_workspace = tempfile::tempdir().expect("create second workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "session-create-idempotency-test")
            .expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let client = reqwest::Client::new();
    let prompt_id = PromptId::new();
    let request = CreateSessionRequest {
        preparation_id: None,
        agent_selection: None,
        execution_directory: suru::protocol::ExecutionDirectory {
            path: workspace.path().to_owned(),
        },
        prompt: InitialPrompt {
            id: prompt_id,
            text: "Explain this workspace".to_owned(),
            skill_invocations: Vec::new(),
        },
    };

    let first = client
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&request)
        .send()
        .await
        .expect("create Session");
    assert_eq!(first.status(), reqwest::StatusCode::CREATED);
    let first = first
        .json::<SessionSnapshot>()
        .await
        .expect("decode created Session");
    let settled =
        read_session_at_least_revision(&client, &descriptor, first.session.id, SessionRevision(2))
            .await;

    let exact_retry = client
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&request)
        .send()
        .await
        .expect("retry Session creation");
    assert_eq!(exact_retry.status(), reqwest::StatusCode::OK);
    assert_eq!(
        exact_retry
            .json::<SessionSnapshot>()
            .await
            .expect("decode retried Session"),
        settled
    );

    for conflicting in [
        CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: request.execution_directory.clone(),
            prompt: InitialPrompt {
                id: prompt_id,
                text: "Different content".to_owned(),
                skill_invocations: Vec::new(),
            },
        },
        CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: other_workspace.path().to_owned(),
            },
            prompt: request.prompt.clone(),
        },
    ] {
        let response = client
            .post(format!("{}/v1/sessions", descriptor.base_url))
            .bearer_auth(&descriptor.token)
            .json(&conflicting)
            .send()
            .await
            .expect("reuse Prompt identity with conflicting creation metadata");
        assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
        assert_eq!(
            response
                .json::<SessionError>()
                .await
                .expect("decode Prompt conflict")
                .code,
            SessionErrorCode::PromptConflict
        );
    }

    std::fs::remove_dir(workspace.path()).expect("remove Workspace after accepted creation");
    let retry_after_workspace_disappears = client
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&request)
        .send()
        .await
        .expect("retry Session creation after Workspace disappears");
    assert_eq!(
        retry_after_workspace_disappears.status(),
        reqwest::StatusCode::OK
    );
    assert_eq!(
        retry_after_workspace_disappears
            .json::<SessionSnapshot>()
            .await
            .expect("decode retry after Workspace disappears"),
        settled
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn admitted_steers_stream_once_and_exact_retries_do_not_duplicate_them() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "prompt-admission-idempotency-test")
            .expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let client = reqwest::Client::new();
    let created = client
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Initial Prompt".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .send()
        .await
        .expect("create Session")
        .error_for_status()
        .expect("Session creation succeeds")
        .json::<SessionSnapshot>()
        .await
        .expect("decode Session");
    let settled = read_session_at_least_revision(
        &client,
        &descriptor,
        created.session.id,
        SessionRevision(2),
    )
    .await;
    let response = client
        .get(format!(
            "{}/v1/sessions/{}/events",
            descriptor.base_url, created.session.id
        ))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("open Session stream")
        .error_for_status()
        .expect("Session stream authenticates");
    let mut events = response.bytes_stream().eventsource();
    timeout(PROGRESS_DEADLINE, events.next())
        .await
        .expect("Session snapshot arrives")
        .expect("Session stream remains open")
        .expect("decode Session snapshot event");

    let prompt_id = PromptId::new();
    let invocation = SkillInvocation {
        skill_id: SkillId::new("safe-smaller-interface-id"),
        name: "smaller-interface".to_owned(),
        scope: Some("Workspace".to_owned()),
        marker: SkillMarkerSpan { start: 0, end: 18 },
    };
    let command = AdmitPromptRequest {
        prompt: InitialPrompt {
            id: prompt_id,
            text: "$smaller-interface use it".to_owned(),
            skill_invocations: vec![invocation.clone()],
        },
        delivery: PromptDelivery::Steer,
    };
    let admitted = client
        .post(format!(
            "{}/v1/sessions/{}/prompts",
            descriptor.base_url, created.session.id
        ))
        .bearer_auth(&descriptor.token)
        .json(&command)
        .send()
        .await
        .expect("admit steer");
    assert_eq!(admitted.status(), reqwest::StatusCode::CREATED);
    let admitted = admitted
        .json::<Prompt>()
        .await
        .expect("decode admitted Prompt");
    assert_eq!(admitted.id, prompt_id);
    assert_eq!(admitted.status, PromptStatus::Pending);

    let update_event = timeout(PROGRESS_DEADLINE, events.next())
        .await
        .expect("Session update arrives")
        .expect("Session stream remains open")
        .expect("decode Session update event");
    assert_eq!(update_event.event, SESSION_UPDATED_EVENT);
    let update = serde_json::from_str::<SessionUpdate>(&update_event.data)
        .expect("decode streamed Session update");
    assert_eq!(update.revision, SessionRevision(settled.revision.0 + 1));
    assert!(update.changes.iter().any(
        |change| matches!(change, SessionChange::PromptAdded { prompt } if prompt.id == prompt_id)
    ));

    let failure_event = timeout(PROGRESS_DEADLINE, events.next())
        .await
        .expect("Provider failure update arrives")
        .expect("Session stream remains open")
        .expect("decode Provider failure update");
    let failure = serde_json::from_str::<SessionUpdate>(&failure_event.data)
        .expect("decode Provider failure Session update");
    assert_eq!(failure.revision, SessionRevision(update.revision.0 + 1));
    let turn_id = failure
        .changes
        .iter()
        .find_map(|change| match change {
            SessionChange::TurnAdded { turn } if turn.prompt_id == Some(prompt_id) => Some(turn.id),
            _ => None,
        })
        .expect("delivered steer creates a Turn");
    assert!(failure.changes.iter().any(|change| {
        matches!(change, SessionChange::MessageAdded { message }
            if message.turn_id == turn_id
                && message.content == command.prompt.text
                && message.skill_invocations == vec![invocation.clone()])
    }));

    let exact_retry = client
        .post(format!(
            "{}/v1/sessions/{}/prompts",
            descriptor.base_url, created.session.id
        ))
        .bearer_auth(&descriptor.token)
        .json(&command)
        .send()
        .await
        .expect("retry steer admission");
    assert_eq!(exact_retry.status(), reqwest::StatusCode::OK);
    let retried = exact_retry
        .json::<Prompt>()
        .await
        .expect("decode retried Prompt");
    assert_eq!(retried.id, admitted.id);
    assert_eq!(retried.text, admitted.text);
    assert_eq!(retried.skill_invocations, vec![invocation.clone()]);
    assert_eq!(retried.delivery, admitted.delivery);
    assert_eq!(retried.status, PromptStatus::Delivered);
    assert!(
        timeout(Duration::from_millis(100), events.next())
            .await
            .is_err(),
        "an exact retry must not emit a duplicate Session update"
    );

    let conflicting = client
        .post(format!(
            "{}/v1/sessions/{}/prompts",
            descriptor.base_url, created.session.id
        ))
        .bearer_auth(&descriptor.token)
        .json(&AdmitPromptRequest {
            prompt: InitialPrompt {
                id: prompt_id,
                text: "Conflicting content".to_owned(),
                skill_invocations: Vec::new(),
            },
            delivery: PromptDelivery::Steer,
        })
        .send()
        .await
        .expect("reuse Prompt identity with conflicting content");
    assert_eq!(conflicting.status(), reqwest::StatusCode::CONFLICT);
    assert_eq!(
        conflicting
            .json::<SessionError>()
            .await
            .expect("decode Prompt conflict")
            .code,
        SessionErrorCode::PromptConflict
    );

    let conflicting_delivery = client
        .post(format!(
            "{}/v1/sessions/{}/prompts",
            descriptor.base_url, created.session.id
        ))
        .bearer_auth(&descriptor.token)
        .json(&AdmitPromptRequest {
            prompt: command.prompt.clone(),
            delivery: PromptDelivery::Queue,
        })
        .send()
        .await
        .expect("reuse Prompt identity with conflicting delivery");
    assert_eq!(conflicting_delivery.status(), reqwest::StatusCode::CONFLICT);
    assert_eq!(
        conflicting_delivery
            .json::<SessionError>()
            .await
            .expect("decode delivery conflict")
            .code,
        SessionErrorCode::PromptConflict
    );

    let conflicting_invocation = client
        .post(format!(
            "{}/v1/sessions/{}/prompts",
            descriptor.base_url, created.session.id
        ))
        .bearer_auth(&descriptor.token)
        .json(&AdmitPromptRequest {
            prompt: InitialPrompt {
                id: prompt_id,
                text: command.prompt.text.clone(),
                skill_invocations: vec![SkillInvocation {
                    skill_id: SkillId::new("different-safe-id"),
                    ..invocation
                }],
            },
            delivery: PromptDelivery::Steer,
        })
        .send()
        .await
        .expect("reuse Prompt identity with conflicting Skill binding");
    assert_eq!(
        conflicting_invocation.status(),
        reqwest::StatusCode::CONFLICT
    );

    drop(events);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn active_turn_admission_preserves_order_and_safe_steer_delivery() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "active-prompt-order-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "active-prompt-order-test")
            .expect("configure managed client"),
    )
    .await
    .expect("connect managed client");
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
                text: "Initial Prompt".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session");
    let session_id = created.session.id;
    let _settled = read_session_at_least_revision(
        &reqwest::Client::new(),
        server.descriptor(),
        session_id,
        SessionRevision(2),
    )
    .await;
    let active_prompt_id = PromptId::new();
    let active_turn_id = TurnId::new();
    server
        .session_event_sink()
        .publish(
            session_id,
            vec![
                SessionChange::PromptAdded {
                    prompt: Prompt {
                        id: active_prompt_id,
                        text: "Long-running work".to_owned(),
                        delivery: PromptDelivery::Steer,
                        admission_order: PromptOrder(2),
                        status: PromptStatus::Delivered,
                        skill_invocations: Vec::new(),
                    },
                },
                SessionChange::TurnAdded {
                    turn: Turn {
                        id: active_turn_id,
                        prompt_id: Some(active_prompt_id),
                        agent: None,
                        status: TurnStatus::Active,
                        started_at: None,
                        settled_at: None,
                        last_output_at: None,
                        usage: None,
                        cost: None,
                        cost_basis: None,
                        cost_details: None,
                    },
                },
                SessionChange::MessageAdded {
                    message: Message {
                        id: MessageId::new(),
                        turn_id: active_turn_id,
                        role: MessageRole::User,
                        status: MessageStatus::Completed,
                        content: "Long-running work".to_owned(),
                        truncated: false,
                        skill_invocations: Vec::new(),
                    },
                },
            ],
        )
        .await
        .expect("start an active provider Turn");
    assert_eq!(
        client
            .read_session(session_id)
            .await
            .expect("read active Session")
            .session
            .status,
        SessionStatus::Active
    );
    let rejected_prompt_id = PromptId::new();
    assert!(
        server
            .session_event_sink()
            .publish(
                session_id,
                vec![
                    SessionChange::PromptAdded {
                        prompt: Prompt {
                            id: rejected_prompt_id,
                            text: "Competing active work".to_owned(),
                            delivery: PromptDelivery::Steer,
                            admission_order: PromptOrder(3),
                            status: PromptStatus::Delivered,
                            skill_invocations: Vec::new(),
                        },
                    },
                    SessionChange::TurnAdded {
                        turn: Turn {
                            id: TurnId::new(),
                            prompt_id: Some(rejected_prompt_id),
                            agent: None,
                            status: TurnStatus::Active,
                            started_at: None,
                            settled_at: None,
                            last_output_at: None,
                            usage: None,
                            cost: None,
                            cost_basis: None,
                            cost_details: None,
                        },
                    },
                ],
            )
            .await
            .is_err(),
        "a Session must reject a competing active Turn"
    );
    assert!(
        !client
            .read_session(session_id)
            .await
            .expect("read Session after rejected competing Turn")
            .prompts
            .iter()
            .any(|prompt| prompt.id == rejected_prompt_id),
        "a rejected active Turn must not partially mutate the Session"
    );

    let queued = client
        .admit_prompt(
            session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Run this later".to_owned(),
                    skill_invocations: Vec::new(),
                },
                delivery: PromptDelivery::Queue,
            },
        )
        .await
        .expect("queue Prompt during active Turn");
    let second_queued = client
        .admit_prompt(
            session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Run this after the first queue".to_owned(),
                    skill_invocations: Vec::new(),
                },
                delivery: PromptDelivery::Queue,
            },
        )
        .await
        .expect("queue a second Prompt during active Turn");
    let steer = client
        .admit_prompt(
            session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Change direction now".to_owned(),
                    skill_invocations: Vec::new(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("admit steer during active Turn");
    assert_eq!(queued.admission_order, PromptOrder(3));
    assert_eq!(second_queued.admission_order, PromptOrder(4));
    assert_eq!(steer.admission_order, PromptOrder(5));
    assert_eq!(queued.status, PromptStatus::Pending);
    assert_eq!(second_queued.status, PromptStatus::Pending);
    assert_eq!(steer.status, PromptStatus::Pending);

    let delivered = server
        .agent_output()
        .continuation_boundary(session_id, active_turn_id)
        .await
        .expect("deliver pending steers at a safe continuation boundary");
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].id, steer.id);
    assert_eq!(delivered[0].status, PromptStatus::Delivered);
    let current = client
        .read_session(session_id)
        .await
        .expect("read Session after continuation boundary");
    assert_eq!(
        current
            .prompts
            .iter()
            .find(|prompt| prompt.id == queued.id)
            .expect("queued Prompt remains authoritative")
            .status,
        PromptStatus::Pending
    );
    assert_eq!(
        current
            .prompts
            .iter()
            .find(|prompt| prompt.id == steer.id)
            .expect("steer remains authoritative")
            .status,
        PromptStatus::Delivered
    );
    assert!(current.messages.iter().any(|message| {
        message.turn_id == active_turn_id && message.content == "Change direction now"
    }));

    server
        .session_event_sink()
        .publish(
            session_id,
            vec![SessionChange::TurnStatusChanged {
                turn_id: active_turn_id,
                status: TurnStatus::Completed,
                settled_at: None,
            }],
        )
        .await
        .expect("complete active Turn at an idle boundary");
    let completed = client
        .read_session(session_id)
        .await
        .expect("read Session after idle boundary");
    assert_eq!(completed.session.status, SessionStatus::Idle);
    assert_eq!(
        completed
            .turns
            .iter()
            .find(|turn| turn.id == active_turn_id)
            .expect("active Turn remains authoritative")
            .status,
        TurnStatus::Completed
    );
    assert_eq!(
        completed
            .prompts
            .iter()
            .find(|prompt| prompt.id == queued.id)
            .expect("queued Prompt remains authoritative")
            .status,
        PromptStatus::Pending
    );
    assert_eq!(
        completed
            .prompts
            .iter()
            .find(|prompt| prompt.id == second_queued.id)
            .expect("second queued Prompt remains authoritative")
            .status,
        PromptStatus::Pending
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn pending_prompt_mutations_and_interruption_converge_across_clients() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "prompt-mutation-test").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let mut first = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "prompt-mutation-test")
            .expect("configure first client"),
    )
    .await
    .expect("connect first client");
    let mut second = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "prompt-mutation-test")
            .expect("configure second client"),
    )
    .await
    .expect("connect second client");
    receive_managed_client_initial_state(&mut first).await;
    receive_managed_client_initial_state(&mut second).await;

    let created = first
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Long-running work".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session");
    let session_id = created.session.id;
    let start = provider.next_start().await;
    let mut provider_session = start.succeed(AgentIdentity {
        agent: AgentId::new("controlled"),
        selection: AgentSelection {
            provider: ProviderId::new("controlled"),
            model: ModelId::new("test-model"),
            options: Vec::new(),
        },
    });
    let turn_start = provider_session.next_turn().await;
    assert_eq!(turn_start.prompt(), "Long-running work");
    let active = first
        .read_session(session_id)
        .await
        .expect("read delivered initial Prompt");
    let active_turn_id = active.turns[0].id;
    turn_start.succeed();
    let promoted = first
        .admit_prompt(
            session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Promote this Prompt".to_owned(),
                    skill_invocations: Vec::new(),
                },
                delivery: PromptDelivery::Queue,
            },
        )
        .await
        .expect("admit Prompt to promote");
    let cancelled = first
        .admit_prompt(
            session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Cancel this Prompt".to_owned(),
                    skill_invocations: Vec::new(),
                },
                delivery: PromptDelivery::Queue,
            },
        )
        .await
        .expect("admit Prompt to cancel");
    let queued_invocation = SkillInvocation {
        skill_id: SkillId::new("safe-review-after-interrupt-id"),
        name: "review".to_owned(),
        scope: Some("Workspace".to_owned()),
        marker: SkillMarkerSpan { start: 0, end: 7 },
    };
    let after_interrupt = first
        .admit_prompt(
            session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "$review after interruption".to_owned(),
                    skill_invocations: vec![queued_invocation.clone()],
                },
                delivery: PromptDelivery::Queue,
            },
        )
        .await
        .expect("admit post-interrupt Prompt");

    let promoted = first
        .promote_prompt(session_id, promoted.id)
        .await
        .expect("promote queued Prompt");
    assert_eq!(promoted.delivery, PromptDelivery::Steer);
    assert_eq!(promoted.status, PromptStatus::Pending);
    assert!(
        second.cancel_prompt(session_id, promoted.id).await.is_err(),
        "a competing cancellation must not cancel a promoted steer"
    );
    let cancelled = first
        .cancel_prompt(session_id, cancelled.id)
        .await
        .expect("cancel queued Prompt");
    assert_eq!(cancelled.status, PromptStatus::Cancelled);
    assert!(
        second
            .promote_prompt(session_id, cancelled.id)
            .await
            .is_err(),
        "a competing mutation must not revive a cancelled Prompt"
    );

    let mut observer = second
        .attach_session(session_id)
        .await
        .expect("attach observing client");
    let SessionEvent::Snapshot(before_interrupt) = observer
        .next()
        .await
        .expect("observer receives snapshot")
        .expect("observer snapshot is valid")
    else {
        panic!("the attach must begin with a Session snapshot");
    };
    assert_eq!(before_interrupt.session.status, SessionStatus::Active);

    let (acknowledged, ()) = tokio::join!(first.interrupt_session(session_id), async {
        provider_session.next_interrupt().await.succeed();
    });
    acknowledged.expect("Provider acknowledges interruption");
    let during_interruption = second
        .read_session(session_id)
        .await
        .expect("read Session during cooperative interruption");
    assert_eq!(during_interruption.session.status, SessionStatus::Active);
    assert_eq!(
        during_interruption
            .turns
            .iter()
            .find(|turn| turn.id == active_turn_id)
            .expect("active Turn remains authoritative")
            .status,
        TurnStatus::Active
    );

    provider_session.emit(ProviderEvent::TurnInterrupted);
    let SessionEvent::Updated(interrupt_update) = timeout(PROGRESS_DEADLINE, observer.next())
        .await
        .expect("interruption update arrives")
        .expect("observer stream remains open")
        .expect("interruption update is valid")
    else {
        panic!("observer must receive an interruption update");
    };
    assert!(interrupt_update.changes.iter().any(|change| {
        matches!(change, SessionChange::TurnStatusChanged { turn_id, status: TurnStatus::Interrupted, .. }
            if *turn_id == active_turn_id)
    }));

    let current = second
        .read_session(session_id)
        .await
        .expect("read interrupted Session from second client");
    assert_eq!(
        current
            .turns
            .iter()
            .find(|turn| turn.id == active_turn_id)
            .expect("active Turn remains authoritative")
            .status,
        TurnStatus::Interrupted
    );
    assert_eq!(current.session.status, SessionStatus::Active);
    assert_eq!(current.turns.len(), 2);
    assert_eq!(current.turns[1].prompt_id, Some(after_interrupt.id));
    assert_eq!(current.turns[1].status, TurnStatus::Active);
    assert_eq!(
        current
            .prompts
            .iter()
            .find(|prompt| prompt.id == promoted.id)
            .expect("promoted Prompt remains authoritative")
            .status,
        PromptStatus::Delivered
    );
    assert!(current.messages.iter().any(|message| {
        message.turn_id == active_turn_id && message.content == "Promote this Prompt"
    }));
    assert!(current.messages.iter().any(|message| {
        message.turn_id == current.turns[1].id
            && message.content == "$review after interruption"
            && message.skill_invocations == vec![queued_invocation.clone()]
    }));

    assert!(
        server
            .agent_output()
            .emit(
                session_id,
                AgentOutput::Activity {
                    activity: Activity::Status {
                        id: ActivityId::new(),
                        turn_id: active_turn_id,
                        text: "Late provider output".to_owned(),
                    },
                },
            )
            .await
            .is_err(),
        "an interrupted Turn must reject later provider output"
    );
    assert!(
        server
            .session_event_sink()
            .publish(
                session_id,
                vec![SessionChange::TurnStatusChanged {
                    turn_id: active_turn_id,
                    status: TurnStatus::Active,
                    settled_at: None,
                }],
            )
            .await
            .is_err(),
        "an interrupted Turn must not be reopened"
    );
    let after_rejected_updates = second
        .read_session(session_id)
        .await
        .expect("read Session after rejected terminal Turn updates");
    assert_eq!(after_rejected_updates, current);
    assert_eq!(
        current
            .prompts
            .iter()
            .find(|prompt| prompt.id == after_interrupt.id)
            .expect("queued Prompt remains authoritative")
            .status,
        PromptStatus::Delivered
    );

    let queued_start = provider_session.next_turn().await;
    assert_eq!(queued_start.prompt(), "$review after interruption");
    assert_eq!(
        queued_start.skill_invocations(),
        &[ProviderSkillInvocation {
            skill_id: queued_invocation.skill_id,
            marker_spans: vec![queued_invocation.marker],
        }]
    );
    queued_start.succeed();
    provider_session.emit(ProviderEvent::TurnCompleted);
    timeout(PROGRESS_DEADLINE, async {
        loop {
            let snapshot = second
                .read_session(session_id)
                .await
                .expect("read completed queued Turn");
            if snapshot.session.status == SessionStatus::Idle {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("queued Turn reaches its terminal boundary");

    drop(observer);
    drop(provider_session);
    drop(second);
    drop(first);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn consecutive_prompt_admissions_and_failures_do_not_collapse_revisions() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "consecutive-prompt-admission-test")
            .expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let client = reqwest::Client::new();
    let created = client
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Initial Prompt".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .send()
        .await
        .expect("create Session")
        .error_for_status()
        .expect("Session creation succeeds")
        .json::<SessionSnapshot>()
        .await
        .expect("decode Session");
    let settled = read_session_at_least_revision(
        &client,
        &descriptor,
        created.session.id,
        SessionRevision(2),
    )
    .await;
    let response = client
        .get(format!(
            "{}/v1/sessions/{}/events",
            descriptor.base_url, created.session.id
        ))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("open Session stream")
        .error_for_status()
        .expect("Session stream authenticates");
    let mut events = response.bytes_stream().eventsource();
    timeout(PROGRESS_DEADLINE, events.next())
        .await
        .expect("Session snapshot arrives")
        .expect("Session stream remains open")
        .expect("decode Session snapshot event");

    let mut prompt_ids = (0..32).map(|_| PromptId::new()).collect::<Vec<_>>();
    let admissions = prompt_ids
        .iter()
        .copied()
        .enumerate()
        .map(|(index, prompt_id)| {
            client
                .post(format!(
                    "{}/v1/sessions/{}/prompts",
                    descriptor.base_url, created.session.id
                ))
                .bearer_auth(&descriptor.token)
                .json(&AdmitPromptRequest {
                    prompt: InitialPrompt {
                        id: prompt_id,
                        text: format!("Consecutive steer {}", index + 1),
                        skill_invocations: Vec::new(),
                    },
                    delivery: PromptDelivery::Steer,
                })
                .send()
        });
    for response in join_all(admissions).await {
        response
            .expect("admit consecutive steer")
            .error_for_status()
            .expect("consecutive steer is accepted");
    }

    let mut admitted_prompts = Vec::new();
    for offset in 1..=64 {
        let event = timeout(PROGRESS_DEADLINE, events.next())
            .await
            .expect("every consecutive Session update arrives")
            .expect("Session stream remains open")
            .expect("decode consecutive Session update");
        let update = serde_json::from_str::<SessionUpdate>(&event.data)
            .expect("decode consecutive Session update body");
        assert_eq!(
            update.revision,
            SessionRevision(settled.revision.0 + offset)
        );
        admitted_prompts.extend(update.changes.iter().filter_map(|change| match change {
            SessionChange::PromptAdded { prompt } => Some(prompt.id),
            _ => None,
        }));
    }
    admitted_prompts.sort_by_key(|prompt_id| prompt_id.as_uuid());
    prompt_ids.sort_by_key(|prompt_id| prompt_id.as_uuid());
    assert_eq!(admitted_prompts, prompt_ids);

    drop(events);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn invalid_workspace_and_blank_prompt_are_rejected_before_session_creation() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "session-validation-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let client = reqwest::Client::new();

    let unauthenticated = client
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .json(&CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Explain this workspace".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .send()
        .await
        .expect("create Session without authentication");
    assert_eq!(unauthenticated.status(), reqwest::StatusCode::UNAUTHORIZED);

    let blank = client
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: " \n\t ".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .send()
        .await
        .expect("submit blank Prompt");
    assert_eq!(blank.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        blank
            .json::<SessionError>()
            .await
            .expect("decode blank Prompt error")
            .code,
        SessionErrorCode::EmptyPrompt
    );

    let missing_workspace = client
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().join("missing"),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Explain this workspace".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .send()
        .await
        .expect("submit invalid Workspace");
    assert_eq!(
        missing_workspace.status(),
        reqwest::StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        missing_workspace
            .json::<SessionError>()
            .await
            .expect("decode invalid Workspace error")
            .code,
        SessionErrorCode::InvalidWorkspace
    );

    server.shutdown().await.expect("shut down server");
}
