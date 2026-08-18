use std::{
    convert::Infallible,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{Path as AxumPath, State},
    http::{
        HeaderMap, StatusCode,
        header::{AUTHORIZATION, CONTENT_TYPE},
    },
    response::{IntoResponse, Response, sse::Event, sse::Sse},
    routing::get,
};
use chidori::{
    build_identity,
    managed_client::{ManagedClient, ManagedClientConfig, SessionEvent},
    protocol::{
        Activity, ActivityId, ActivityKind, AdmitPromptRequest, CreateSessionRequest,
        InitialPrompt, LifecycleState, Message, MessageId, MessageRole, MessageStatus,
        PROTOCOL_VERSION, Prompt, PromptDelivery, PromptId, PromptOrder, PromptStatus,
        RuntimeDescriptor, SESSION_SNAPSHOT_EVENT, SESSION_UPDATED_EVENT, ServerIdentity, Session,
        SessionChange, SessionError, SessionErrorCode, SessionId, SessionRevision, SessionSnapshot,
        SessionStatus, SessionSummary, SessionUpdate, TranscriptItem, Turn, TurnId, TurnStatus,
        Workspace,
    },
    server::{self, AgentOutput, ServerConfig},
};
use eventsource_stream::Eventsource;
use futures_util::{StreamExt, future::join_all, stream};
use tokio::time::{Duration, timeout};

#[tokio::test]
async fn authenticated_first_prompt_atomically_creates_a_failed_session_turn() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace_parent = tempfile::tempdir().expect("create workspace parent");
    let workspace = workspace_parent.path().join("workspace");
    std::fs::create_dir(&workspace).expect("create workspace");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "session-create-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let prompt_id = PromptId::new();

    let response = reqwest::Client::new()
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&CreateSessionRequest {
            workspace: Workspace {
                path: workspace_parent.path().join(".").join("workspace"),
            },
            prompt: InitialPrompt {
                id: prompt_id,
                text: "Explain this workspace".to_owned(),
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
        std::fs::canonicalize(&workspace).expect("canonicalize expected Workspace")
    );
    assert_eq!(snapshot.session.agent, None);
    assert_eq!(snapshot.session.status, SessionStatus::Idle);
    assert_eq!(snapshot.prompts.len(), 1);
    assert_eq!(snapshot.prompts[0].id, prompt_id);
    assert_eq!(snapshot.prompts[0].status, PromptStatus::Delivered);
    assert_eq!(snapshot.turns.len(), 1);
    assert_eq!(snapshot.turns[0].prompt_id, prompt_id);
    assert_eq!(snapshot.turns[0].status, TurnStatus::Failed);
    assert_eq!(snapshot.messages.len(), 1);
    assert_eq!(snapshot.messages[0].role, MessageRole::User);
    assert_eq!(snapshot.messages[0].content, "Explain this workspace");
    assert_eq!(snapshot.activities.len(), 1);
    assert_eq!(snapshot.activities[0].kind, ActivityKind::Error);
    assert!(snapshot.activities[0].text.contains("No Agent"));
    assert!(
        snapshot
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
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "session-create-idempotency-test")
            .expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let client = reqwest::Client::new();
    let prompt_id = PromptId::new();
    let request = CreateSessionRequest {
        workspace: Workspace {
            path: workspace.path().to_owned(),
        },
        prompt: InitialPrompt {
            id: prompt_id,
            text: "Explain this workspace".to_owned(),
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
        first
    );

    for conflicting in [
        CreateSessionRequest {
            workspace: request.workspace.clone(),
            prompt: InitialPrompt {
                id: prompt_id,
                text: "Different content".to_owned(),
            },
        },
        CreateSessionRequest {
            workspace: Workspace {
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
        first
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn admitted_steers_stream_once_and_exact_retries_do_not_duplicate_them() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid workspace");
    let server = server::spawn(
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
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Initial Prompt".to_owned(),
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
    timeout(Duration::from_secs(1), events.next())
        .await
        .expect("Session snapshot arrives")
        .expect("Session stream remains open")
        .expect("decode Session snapshot event");

    let prompt_id = PromptId::new();
    let command = AdmitPromptRequest {
        prompt: InitialPrompt {
            id: prompt_id,
            text: "Use the smaller interface".to_owned(),
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
    assert_eq!(admitted.status, PromptStatus::Delivered);

    let update_event = timeout(Duration::from_secs(1), events.next())
        .await
        .expect("Session update arrives")
        .expect("Session stream remains open")
        .expect("decode Session update event");
    assert_eq!(update_event.event, SESSION_UPDATED_EVENT);
    let update = serde_json::from_str::<SessionUpdate>(&update_event.data)
        .expect("decode streamed Session update");
    assert_eq!(update.revision, SessionRevision(2));
    assert!(update.changes.iter().any(
        |change| matches!(change, SessionChange::PromptAdded { prompt } if prompt.id == prompt_id)
    ));
    let turn_id = update
        .changes
        .iter()
        .find_map(|change| match change {
            SessionChange::TurnAdded { turn } if turn.prompt_id == prompt_id => Some(turn.id),
            _ => None,
        })
        .expect("delivered steer creates a Turn");
    assert!(update.changes.iter().any(|change| {
        matches!(change, SessionChange::MessageAdded { message }
            if message.turn_id == turn_id && message.content == command.prompt.text)
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
    assert_eq!(
        exact_retry
            .json::<Prompt>()
            .await
            .expect("decode retried Prompt"),
        admitted
    );
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

    drop(events);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn active_turn_admission_orders_queue_and_steer_before_safe_delivery() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn(
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
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Initial Prompt".to_owned(),
            },
        })
        .await
        .expect("create Session");
    let session_id = created.session.id;
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
                    },
                },
                SessionChange::TurnAdded {
                    turn: Turn {
                        id: active_turn_id,
                        prompt_id: active_prompt_id,
                        status: TurnStatus::Active,
                    },
                },
                SessionChange::MessageAdded {
                    message: Message {
                        id: MessageId::new(),
                        turn_id: active_turn_id,
                        role: MessageRole::User,
                        status: MessageStatus::Completed,
                        content: "Long-running work".to_owned(),
                    },
                },
            ],
        )
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
                        },
                    },
                    SessionChange::TurnAdded {
                        turn: Turn {
                            id: TurnId::new(),
                            prompt_id: rejected_prompt_id,
                            status: TurnStatus::Active,
                        },
                    },
                ],
            )
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
            }],
        )
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
        PromptStatus::Delivered
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
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "prompt-mutation-test").expect("configure server"),
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
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Initial Prompt".to_owned(),
            },
        })
        .await
        .expect("create Session");
    let session_id = created.session.id;
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
                    },
                },
                SessionChange::TurnAdded {
                    turn: Turn {
                        id: active_turn_id,
                        prompt_id: active_prompt_id,
                        status: TurnStatus::Active,
                    },
                },
                SessionChange::MessageAdded {
                    message: Message {
                        id: MessageId::new(),
                        turn_id: active_turn_id,
                        role: MessageRole::User,
                        status: MessageStatus::Completed,
                        content: "Long-running work".to_owned(),
                    },
                },
            ],
        )
        .expect("start active Turn");
    let promoted = first
        .admit_prompt(
            session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Promote this Prompt".to_owned(),
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
                },
                delivery: PromptDelivery::Queue,
            },
        )
        .await
        .expect("admit Prompt to cancel");
    let after_interrupt = first
        .admit_prompt(
            session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Run after interruption".to_owned(),
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
        panic!("attachment must begin with a Session snapshot");
    };
    assert_eq!(before_interrupt.session.status, SessionStatus::Active);

    let interrupted = first
        .interrupt_turn(session_id, active_turn_id)
        .await
        .expect("interrupt active Turn");
    assert_eq!(interrupted.status, TurnStatus::Interrupted);
    let SessionEvent::Updated(interrupt_update) = timeout(Duration::from_secs(1), observer.next())
        .await
        .expect("interruption update arrives")
        .expect("observer stream remains open")
        .expect("interruption update is valid")
    else {
        panic!("observer must receive an interruption update");
    };
    assert!(interrupt_update.changes.iter().any(|change| {
        matches!(change, SessionChange::TurnStatusChanged { turn_id, status: TurnStatus::Interrupted }
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
    assert_eq!(current.session.status, SessionStatus::Idle);

    assert!(
        server
            .agent_output()
            .emit(
                session_id,
                AgentOutput::Activity {
                    activity_id: ActivityId::new(),
                    turn_id: active_turn_id,
                    kind: ActivityKind::Status,
                    text: "Late provider output".to_owned(),
                },
            )
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
                }],
            )
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
        PromptStatus::Pending
    );

    drop(observer);
    drop(second);
    drop(first);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn consecutive_prompt_admissions_are_delivered_without_collapsing_revisions() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid workspace");
    let server = server::spawn(
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
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Initial Prompt".to_owned(),
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
    timeout(Duration::from_secs(1), events.next())
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

    let mut delivered_prompts = Vec::new();
    for expected_revision in 2..=33 {
        let event = timeout(Duration::from_secs(1), events.next())
            .await
            .expect("every consecutive Session update arrives")
            .expect("Session stream remains open")
            .expect("decode consecutive Session update");
        let update = serde_json::from_str::<SessionUpdate>(&event.data)
            .expect("decode consecutive Session update body");
        assert_eq!(update.revision, SessionRevision(expected_revision));
        delivered_prompts.extend(update.changes.iter().filter_map(|change| match change {
            SessionChange::PromptAdded { prompt } => Some(prompt.id),
            _ => None,
        }));
    }
    delivered_prompts.sort_by_key(|prompt_id| prompt_id.as_uuid());
    prompt_ids.sort_by_key(|prompt_id| prompt_id.as_uuid());
    assert_eq!(delivered_prompts, prompt_ids);

    drop(events);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn authenticated_clients_can_read_a_session_by_id() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "session-read-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let client = reqwest::Client::new();
    let created = client
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&CreateSessionRequest {
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Explain this workspace".to_owned(),
            },
        })
        .send()
        .await
        .expect("create Session")
        .error_for_status()
        .expect("Session creation succeeds")
        .json::<SessionSnapshot>()
        .await
        .expect("decode created Session");
    let session_url = format!("{}/v1/sessions/{}", descriptor.base_url, created.session.id);

    let unauthenticated = client
        .get(&session_url)
        .send()
        .await
        .expect("read Session without authentication");
    assert_eq!(unauthenticated.status(), reqwest::StatusCode::UNAUTHORIZED);
    let read = client
        .get(&session_url)
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("read authenticated Session")
        .error_for_status()
        .expect("Session read succeeds")
        .json::<SessionSnapshot>()
        .await
        .expect("decode Session read");
    assert_eq!(read, created);

    let missing = client
        .get(format!(
            "{}/v1/sessions/{}",
            descriptor.base_url,
            SessionId::new()
        ))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("read missing Session");
    assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);
    assert_eq!(
        missing
            .json::<SessionError>()
            .await
            .expect("decode missing Session error")
            .code,
        SessionErrorCode::SessionNotFound
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn session_discovery_lists_newest_first_and_filters_by_canonical_workspace() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let first_workspace_parent = tempfile::tempdir().expect("create first Workspace parent");
    let first_workspace = first_workspace_parent.path().join("workspace");
    std::fs::create_dir(&first_workspace).expect("create first Workspace");
    let second_workspace = tempfile::tempdir().expect("create second Workspace");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "session-list-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let client = reqwest::Client::new();

    let create = |path: &std::path::Path, text: &str| CreateSessionRequest {
        workspace: Workspace {
            path: path.to_owned(),
        },
        prompt: InitialPrompt {
            id: PromptId::new(),
            text: text.to_owned(),
        },
    };
    let first = client
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&create(&first_workspace, "First Session"))
        .send()
        .await
        .expect("create first Session")
        .error_for_status()
        .expect("first Session creation succeeds")
        .json::<SessionSnapshot>()
        .await
        .expect("decode first Session");
    let second = client
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&create(second_workspace.path(), "  Second Session  "))
        .send()
        .await
        .expect("create second Session")
        .error_for_status()
        .expect("second Session creation succeeds")
        .json::<SessionSnapshot>()
        .await
        .expect("decode second Session");

    let unauthenticated = client
        .get(format!("{}/v1/sessions", descriptor.base_url))
        .send()
        .await
        .expect("list Sessions without authentication");
    assert_eq!(unauthenticated.status(), reqwest::StatusCode::UNAUTHORIZED);
    let summaries = client
        .get(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("list Sessions")
        .error_for_status()
        .expect("Session listing succeeds")
        .json::<Vec<SessionSummary>>()
        .await
        .expect("decode Session summaries");
    assert_eq!(
        summaries
            .iter()
            .map(|summary| summary.session.id)
            .collect::<Vec<_>>(),
        vec![second.session.id, first.session.id]
    );
    assert_eq!(summaries[0].title, "Second Session");
    assert_eq!(summaries[0].session.workspace, second.session.workspace);
    assert_eq!(summaries[0].session.agent, None);
    assert_eq!(summaries[0].session.status, SessionStatus::Idle);
    assert!(summaries[0].created_at <= summaries[0].updated_at);
    assert!(summaries[0].updated_at > summaries[1].updated_at);

    let filtered = client
        .get(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .query(&[(
            "workspace",
            first_workspace_parent.path().join(".").join("workspace"),
        )])
        .send()
        .await
        .expect("list Sessions for one Workspace")
        .error_for_status()
        .expect("filtered Session listing succeeds")
        .json::<Vec<SessionSummary>>()
        .await
        .expect("decode filtered Session summaries");
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].session.id, first.session.id);
    assert_eq!(filtered[0].title, "First Session");
    assert_eq!(
        filtered[0].session.workspace.path,
        std::fs::canonicalize(first_workspace).expect("canonicalize expected Workspace")
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_new_server_instance_does_not_expose_the_replaced_instances_sessions() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let channel = "ephemeral-session-replacement-test";
    let original = server::spawn(
        ServerConfig::new(state_dir.path(), channel).expect("configure original server"),
    )
    .await
    .expect("spawn original server");
    let original_descriptor = original.descriptor().clone();
    let session = reqwest::Client::new()
        .post(format!("{}/v1/sessions", original_descriptor.base_url))
        .bearer_auth(&original_descriptor.token)
        .json(&CreateSessionRequest {
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Ephemeral Session".to_owned(),
            },
        })
        .send()
        .await
        .expect("create Session")
        .error_for_status()
        .expect("Session creation succeeds")
        .json::<SessionSnapshot>()
        .await
        .expect("decode created Session");
    original.shutdown().await.expect("stop original server");

    let replacement = server::spawn(
        ServerConfig::new(state_dir.path(), channel).expect("configure replacement server"),
    )
    .await
    .expect("spawn replacement server");
    let replacement_descriptor = replacement.descriptor().clone();
    let response = reqwest::Client::new()
        .get(format!(
            "{}/v1/sessions/{}",
            replacement_descriptor.base_url, session.session.id
        ))
        .bearer_auth(&replacement_descriptor.token)
        .send()
        .await
        .expect("read old Session from replacement server");
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);

    replacement
        .shutdown()
        .await
        .expect("shut down replacement server");
}

#[tokio::test]
async fn invalid_workspace_and_blank_prompt_are_rejected_before_session_creation() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid workspace");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "session-validation-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let client = reqwest::Client::new();

    let unauthenticated = client
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .json(&CreateSessionRequest {
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Explain this workspace".to_owned(),
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
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: " \n\t ".to_owned(),
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
            workspace: Workspace {
                path: workspace.path().join("missing"),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Explain this workspace".to_owned(),
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

#[tokio::test]
async fn authenticated_session_stream_starts_with_a_complete_revisioned_snapshot() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid workspace");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "session-events-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let client = reqwest::Client::new();
    let created = client
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&CreateSessionRequest {
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Explain this workspace".to_owned(),
            },
        })
        .send()
        .await
        .expect("create Session")
        .error_for_status()
        .expect("Session creation succeeds")
        .json::<SessionSnapshot>()
        .await
        .expect("decode created Session");
    let events_url = format!(
        "{}/v1/sessions/{}/events",
        descriptor.base_url, created.session.id
    );

    let unauthenticated = client
        .get(&events_url)
        .send()
        .await
        .expect("request Session stream without authentication");
    assert_eq!(unauthenticated.status(), reqwest::StatusCode::UNAUTHORIZED);

    let response = client
        .get(&events_url)
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("open authenticated Session stream")
        .error_for_status()
        .expect("Session stream authenticates");
    let mut events = response.bytes_stream().eventsource();
    let first = timeout(Duration::from_secs(1), events.next())
        .await
        .expect("Session snapshot arrives")
        .expect("Session stream remains open")
        .expect("decode Session SSE event");

    assert_eq!(first.event, SESSION_SNAPSHOT_EVENT);
    assert_eq!(first.id, created.revision.0.to_string());
    assert_eq!(
        serde_json::from_str::<SessionSnapshot>(&first.data)
            .expect("decode Session snapshot event"),
        created
    );

    drop(events);
    let reconnected = client
        .get(&events_url)
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("reconnect to Session stream")
        .error_for_status()
        .expect("Session remains available after stream disconnect");
    let mut reconnected_events = reconnected.bytes_stream().eventsource();
    let fresh_snapshot = timeout(Duration::from_secs(1), reconnected_events.next())
        .await
        .expect("fresh Session snapshot arrives")
        .expect("reconnected Session stream remains open")
        .expect("decode reconnected Session SSE event");
    assert_eq!(fresh_snapshot.event, SESSION_SNAPSHOT_EVENT);
    assert_eq!(fresh_snapshot.id, created.revision.0.to_string());

    drop(reconnected_events);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn real_session_stream_appends_and_completes_one_stable_agent_message() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "agent-output-stream-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "agent-output-stream-test")
            .expect("configure managed client"),
    )
    .await
    .expect("connect managed client");
    receive_managed_client_initial_state(&mut client).await;

    let created = client
        .create_session(CreateSessionRequest {
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Explain the stream".to_owned(),
            },
        })
        .await
        .expect("create Session");
    let session_id = created.session.id;
    let prompt_id = PromptId::new();
    let turn_id = TurnId::new();
    let message_id = MessageId::new();
    let mut subscription = client
        .subscribe_session(session_id)
        .await
        .expect("subscribe to Session");
    assert_eq!(
        timeout(Duration::from_secs(1), subscription.next())
            .await
            .expect("Session snapshot arrives")
            .expect("Session stream remains open")
            .expect("Session snapshot is valid"),
        SessionEvent::Snapshot(created)
    );

    let active_update = server
        .session_event_sink()
        .publish(
            session_id,
            vec![
                SessionChange::PromptAdded {
                    prompt: Prompt {
                        id: prompt_id,
                        text: "Continue with an active Agent".to_owned(),
                        delivery: PromptDelivery::Steer,
                        admission_order: PromptOrder(2),
                        status: PromptStatus::Delivered,
                    },
                },
                SessionChange::TurnAdded {
                    turn: Turn {
                        id: turn_id,
                        prompt_id,
                        status: TurnStatus::Active,
                    },
                },
                SessionChange::MessageAdded {
                    message: Message {
                        id: MessageId::new(),
                        turn_id,
                        role: MessageRole::User,
                        status: MessageStatus::Completed,
                        content: "Continue with an active Agent".to_owned(),
                    },
                },
            ],
        )
        .expect("start an active Turn for Agent output");
    assert_eq!(active_update.revision, SessionRevision(2));
    assert_eq!(
        timeout(Duration::from_secs(1), subscription.next())
            .await
            .expect("active Turn update arrives")
            .expect("Session stream remains open")
            .expect("active Turn update is valid"),
        SessionEvent::Updated(active_update)
    );

    let output = server.agent_output();
    let expected = [
        AgentOutput::MessageStarted {
            message_id,
            turn_id,
        },
        AgentOutput::MessageDelta {
            message_id,
            content: "Hello".to_owned(),
        },
        AgentOutput::MessageDelta {
            message_id,
            content: " world".to_owned(),
        },
        AgentOutput::MessageCompleted { message_id },
    ];
    let published = expected
        .into_iter()
        .enumerate()
        .map(|(index, event)| {
            let update = output
                .emit(session_id, event)
                .expect("publish provider-neutral Agent output");
            assert_eq!(update.revision, SessionRevision(index as u64 + 3));
            update
        })
        .collect::<Vec<_>>();
    for expected_update in published {
        assert_eq!(
            timeout(Duration::from_secs(1), subscription.next())
                .await
                .expect("Session update arrives")
                .expect("Session stream remains open")
                .expect("Session update is valid"),
            SessionEvent::Updated(expected_update)
        );
    }

    let mut reconnected = client
        .subscribe_session(session_id)
        .await
        .expect("reconnect to completed Session");
    let SessionEvent::Snapshot(completed) = timeout(Duration::from_secs(1), reconnected.next())
        .await
        .expect("fresh completed snapshot arrives")
        .expect("reconnected Session stream remains open")
        .expect("completed Session snapshot is valid")
    else {
        panic!("reconnected Session must begin with a snapshot");
    };
    let agent_messages = completed
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::Agent)
        .collect::<Vec<_>>();
    assert_eq!(
        agent_messages.len(),
        1,
        "chunks must not create Message rows"
    );
    assert_eq!(agent_messages[0].id, message_id);
    assert_eq!(agent_messages[0].content, "Hello world");
    assert_eq!(agent_messages[0].status, MessageStatus::Completed);
    assert_eq!(completed.revision, SessionRevision(6));

    drop(reconnected);
    drop(subscription);
    drop(client);
    drop(output);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn active_session_stream_does_not_delay_graceful_server_shutdown() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid workspace");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "session-shutdown-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let client = reqwest::Client::new();
    let created = client
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&CreateSessionRequest {
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Explain this workspace".to_owned(),
            },
        })
        .send()
        .await
        .expect("create Session")
        .error_for_status()
        .expect("Session creation succeeds")
        .json::<SessionSnapshot>()
        .await
        .expect("decode created Session");
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
    timeout(Duration::from_secs(1), events.next())
        .await
        .expect("Session snapshot arrives")
        .expect("Session stream remains open")
        .expect("decode Session snapshot event");

    timeout(Duration::from_secs(1), server.shutdown())
        .await
        .expect("active Session stream does not delay graceful shutdown")
        .expect("shut down server");
    assert!(
        timeout(Duration::from_secs(1), events.next())
            .await
            .expect("Session stream closes on shutdown")
            .is_none()
    );
}

#[tokio::test]
async fn managed_clients_can_reconnect_to_a_session_that_outlives_its_first_client() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid workspace");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "managed-session-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let mut first_client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "managed-session-test")
            .expect("configure first managed client"),
    )
    .await
    .expect("connect first managed client");
    receive_managed_client_initial_state(&mut first_client).await;

    let created = first_client
        .create_session(CreateSessionRequest {
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Explain this workspace".to_owned(),
            },
        })
        .await
        .expect("create Session through managed client");
    let mut first_subscription = first_client
        .attach_session(created.session.id)
        .await
        .expect("subscribe through first managed client");
    assert_eq!(
        first_subscription
            .next()
            .await
            .expect("first Session event arrives")
            .expect("first Session event is valid"),
        SessionEvent::Snapshot(created.clone())
    );

    drop(first_subscription);
    drop(first_client);

    let mut second_client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "managed-session-test")
            .expect("configure second managed client"),
    )
    .await
    .expect("connect second managed client");
    receive_managed_client_initial_state(&mut second_client).await;
    let mut second_subscription = second_client
        .attach_session(created.session.id)
        .await
        .expect("reconnect to existing Session");
    assert_eq!(
        second_subscription
            .next()
            .await
            .expect("reconnected Session event arrives")
            .expect("reconnected Session event is valid"),
        SessionEvent::Snapshot(created)
    );

    drop(second_subscription);
    drop(second_client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn managed_client_can_discover_read_and_attach_to_a_known_session() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "managed-session-attach-test")
            .expect("configure server"),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "managed-session-attach-test")
            .expect("configure managed client"),
    )
    .await
    .expect("connect managed client");
    receive_managed_client_initial_state(&mut client).await;
    let created = client
        .create_session(CreateSessionRequest {
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Attach to this Session".to_owned(),
            },
        })
        .await
        .expect("create Session");

    assert_eq!(
        client
            .read_session(created.session.id)
            .await
            .expect("read Session through managed client"),
        created
    );
    let summaries = client
        .list_sessions(Some(workspace.path()))
        .await
        .expect("discover Sessions through managed client");
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].session.id, created.session.id);

    let mut attachment = client
        .attach_session(created.session.id)
        .await
        .expect("attach to known Session ID");
    assert_eq!(
        attachment
            .next()
            .await
            .expect("attached Session event arrives")
            .expect("attached Session event is valid"),
        SessionEvent::Snapshot(created)
    );

    drop(attachment);
    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn two_clients_converge_on_one_session_without_observing_another_session() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn(
        ServerConfig::new(state_dir.path(), "shared-session-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let session_events = server.session_event_sink();
    let mut first_client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "shared-session-test")
            .expect("configure first client"),
    )
    .await
    .expect("connect first client");
    let mut second_client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "shared-session-test")
            .expect("configure second client"),
    )
    .await
    .expect("connect second client");
    receive_managed_client_initial_state(&mut first_client).await;
    receive_managed_client_initial_state(&mut second_client).await;

    let shared = first_client
        .create_session(CreateSessionRequest {
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Shared Session".to_owned(),
            },
        })
        .await
        .expect("create shared Session");
    let isolated = first_client
        .create_session(CreateSessionRequest {
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Isolated Session".to_owned(),
            },
        })
        .await
        .expect("create isolated Session");
    let mut first_attachment = first_client
        .attach_session(shared.session.id)
        .await
        .expect("attach first client to shared Session");
    let mut second_attachment = second_client
        .attach_session(shared.session.id)
        .await
        .expect("attach second client to shared Session");

    let first_projection = first_attachment
        .next()
        .await
        .expect("first client receives shared Session")
        .expect("first shared Session snapshot is valid");
    let second_projection = second_attachment
        .next()
        .await
        .expect("second client receives shared Session")
        .expect("second shared Session snapshot is valid");
    assert_eq!(first_projection, SessionEvent::Snapshot(shared.clone()));
    assert_eq!(second_projection, first_projection);

    let mut isolated_attachment = second_client
        .attach_session(isolated.session.id)
        .await
        .expect("attach second client to isolated Session");
    assert_eq!(
        isolated_attachment
            .next()
            .await
            .expect("isolated Session snapshot arrives")
            .expect("isolated Session snapshot is valid"),
        SessionEvent::Snapshot(isolated.clone())
    );

    let before_update = first_client
        .list_sessions(None)
        .await
        .expect("list Sessions before update");
    assert_eq!(before_update[0].session.id, isolated.session.id);
    let prompt_id = PromptId::new();
    let turn_id = TurnId::new();
    let update = session_events
        .publish(
            shared.session.id,
            vec![
                SessionChange::PromptAdded {
                    prompt: Prompt {
                        id: prompt_id,
                        text: "Observe this change".to_owned(),
                        delivery: PromptDelivery::Steer,
                        admission_order: PromptOrder(2),
                        status: PromptStatus::Delivered,
                    },
                },
                SessionChange::TurnAdded {
                    turn: Turn {
                        id: turn_id,
                        prompt_id,
                        status: TurnStatus::Active,
                    },
                },
                SessionChange::MessageAdded {
                    message: Message {
                        id: MessageId::new(),
                        turn_id,
                        role: MessageRole::User,
                        status: MessageStatus::Completed,
                        content: "Observe this change".to_owned(),
                    },
                },
                SessionChange::ActivityAdded {
                    activity: Activity {
                        id: ActivityId::new(),
                        turn_id,
                        kind: ActivityKind::Status,
                        text: "Working".to_owned(),
                    },
                },
                SessionChange::SessionStatusChanged {
                    status: SessionStatus::Active,
                },
            ],
        )
        .expect("publish provider-neutral Session changes");
    assert_eq!(update.revision, SessionRevision(2));

    let first_update = first_attachment
        .next()
        .await
        .expect("first client receives Session update")
        .expect("first client Session update is valid");
    let second_update = second_attachment
        .next()
        .await
        .expect("second client receives Session update")
        .expect("second client Session update is valid");
    assert_eq!(first_update, SessionEvent::Updated(update.clone()));
    assert_eq!(second_update, first_update);
    assert!(
        timeout(Duration::from_millis(100), isolated_attachment.next())
            .await
            .is_err(),
        "an update for the shared Session must not appear on another Session stream"
    );

    let current = first_client
        .read_session(shared.session.id)
        .await
        .expect("read updated shared Session");
    assert_eq!(current.revision, update.revision);
    assert_eq!(current.prompts.len(), 2);
    assert_eq!(current.turns.len(), 2);
    assert_eq!(current.messages.len(), 2);
    assert_eq!(current.activities.len(), 2);
    assert_eq!(current.session.status, SessionStatus::Active);
    let after_update = first_client
        .list_sessions(None)
        .await
        .expect("list Sessions after update");
    assert_eq!(after_update[0].session.id, shared.session.id);
    assert_eq!(after_update[0].session.status, SessionStatus::Active);
    assert!(after_update[0].updated_at > after_update[0].created_at);

    drop(isolated_attachment);
    drop(second_attachment);
    drop(first_attachment);
    drop(second_client);
    drop(first_client);
    server.shutdown().await.expect("shut down server");
}

async fn receive_managed_client_initial_state(client: &mut ManagedClient) {
    assert!(matches!(
        client.next().await,
        Some(chidori::managed_client::ManagedEvent::Connecting)
    ));
    assert!(matches!(
        client.next().await,
        Some(chidori::managed_client::ManagedEvent::Connected(_))
    ));
}

#[tokio::test]
async fn managed_session_stream_rejects_a_non_monotonic_revision() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let session_id = SessionId::new();
    let snapshot = failed_session_snapshot(session_id, workspace.path());
    let fixture = MalformedSessionStreamFixture::spawn(
        state_dir.path(),
        "malformed-session-stream-test",
        snapshot.clone(),
    )
    .await;
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "malformed-session-stream-test")
            .expect("configure managed client"),
    )
    .await
    .expect("connect managed client to fixture server");
    receive_managed_client_initial_state(&mut client).await;
    let mut subscription = client
        .attach_session(session_id)
        .await
        .expect("subscribe to fixture Session");

    assert_eq!(
        subscription
            .next()
            .await
            .expect("Session snapshot arrives")
            .expect("Session snapshot is valid"),
        SessionEvent::Snapshot(snapshot)
    );
    let error = subscription
        .next()
        .await
        .expect("invalid Session update arrives")
        .expect_err("duplicate revision must be rejected");
    assert!(error.to_string().contains("revision is not monotonic"));
    assert!(!error.is_recoverable());

    drop(subscription);
    drop(client);
    drop(fixture);
}

#[tokio::test]
async fn managed_session_stream_classifies_body_failures_as_recoverable() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let session_id = SessionId::new();
    let snapshot = failed_session_snapshot(session_id, workspace.path());
    let fixture = MalformedSessionStreamFixture::spawn_transport_failure(
        state_dir.path(),
        "broken-session-transport-test",
        snapshot.clone(),
    )
    .await;
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "broken-session-transport-test")
            .expect("configure managed client"),
    )
    .await
    .expect("connect managed client to fixture server");
    receive_managed_client_initial_state(&mut client).await;
    let mut subscription = client
        .subscribe_session(session_id)
        .await
        .expect("subscribe to fixture Session");

    assert_eq!(
        subscription
            .next()
            .await
            .expect("Session snapshot arrives")
            .expect("Session snapshot is valid"),
        SessionEvent::Snapshot(snapshot)
    );
    let error = subscription
        .next()
        .await
        .expect("transport failure arrives")
        .expect_err("broken response body must fail");
    assert!(error.is_recoverable());

    drop(subscription);
    drop(client);
    drop(fixture);
}

#[tokio::test]
async fn managed_attachment_rehydrates_before_live_deltas_after_same_server_disconnect() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let session_id = SessionId::new();
    let initial = failed_session_snapshot(session_id, workspace.path());
    let mut current = initial.clone();
    current.revision = SessionRevision(2);
    current.activities.push(Activity {
        id: ActivityId::new(),
        turn_id: current.turns[0].id,
        kind: ActivityKind::Status,
        text: "Recovered current state".to_owned(),
    });
    let update = SessionUpdate {
        session_id,
        revision: SessionRevision(3),
        changes: vec![SessionChange::SessionStatusChanged {
            status: SessionStatus::Active,
        }],
    };
    let fixture = ReconnectingSessionStreamFixture::spawn(
        state_dir.path(),
        "reconnecting-session-stream-test",
        initial.clone(),
        current.clone(),
        update.clone(),
    )
    .await;
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "reconnecting-session-stream-test")
            .expect("configure managed client"),
    )
    .await
    .expect("connect managed client to fixture server");
    receive_managed_client_initial_state(&mut client).await;
    let mut attachment = client
        .attach_session(session_id)
        .await
        .expect("attach to fixture Session");

    assert_eq!(
        attachment
            .next()
            .await
            .expect("initial Session snapshot arrives")
            .expect("initial Session snapshot is valid"),
        SessionEvent::Snapshot(initial)
    );
    assert_eq!(
        timeout(Duration::from_secs(1), attachment.next())
            .await
            .expect("attachment reconnects")
            .expect("fresh Session snapshot arrives")
            .expect("fresh Session snapshot is valid"),
        SessionEvent::Snapshot(current)
    );
    assert_eq!(
        attachment
            .next()
            .await
            .expect("live Session delta arrives")
            .expect("live Session delta is valid"),
        SessionEvent::Updated(update)
    );

    drop(attachment);
    drop(client);
    drop(fixture);
}

fn failed_session_snapshot(session_id: SessionId, workspace: &std::path::Path) -> SessionSnapshot {
    let prompt_id = PromptId::new();
    let turn_id = TurnId::new();
    let message_id = MessageId::new();
    let activity_id = ActivityId::new();
    SessionSnapshot {
        session: Session {
            id: session_id,
            workspace: Workspace {
                path: workspace.to_owned(),
            },
            agent: None,
            status: SessionStatus::Idle,
        },
        revision: SessionRevision::INITIAL,
        prompts: vec![Prompt {
            id: prompt_id,
            text: "Explain this workspace".to_owned(),
            delivery: PromptDelivery::Steer,
            admission_order: PromptOrder::INITIAL,
            status: PromptStatus::Delivered,
        }],
        turns: vec![Turn {
            id: turn_id,
            prompt_id,
            status: TurnStatus::Failed,
        }],
        messages: vec![Message {
            id: message_id,
            turn_id,
            role: MessageRole::User,
            status: MessageStatus::Completed,
            content: "Explain this workspace".to_owned(),
        }],
        activities: vec![Activity {
            id: activity_id,
            turn_id,
            kind: ActivityKind::Error,
            text: "No Agent is selected".to_owned(),
        }],
        transcript: vec![
            TranscriptItem::Message { message_id },
            TranscriptItem::Activity { activity_id },
        ],
    }
}

#[derive(Clone)]
struct MalformedSessionStreamState {
    descriptor: RuntimeDescriptor,
    snapshot: SessionSnapshot,
    transport_failure: bool,
}

struct MalformedSessionStreamFixture {
    task: tokio::task::JoinHandle<()>,
}

impl MalformedSessionStreamFixture {
    async fn spawn(state_dir: &std::path::Path, channel: &str, snapshot: SessionSnapshot) -> Self {
        Self::spawn_with_transport_failure(state_dir, channel, snapshot, false).await
    }

    async fn spawn_transport_failure(
        state_dir: &std::path::Path,
        channel: &str,
        snapshot: SessionSnapshot,
    ) -> Self {
        Self::spawn_with_transport_failure(state_dir, channel, snapshot, true).await
    }

    async fn spawn_with_transport_failure(
        state_dir: &std::path::Path,
        channel: &str,
        snapshot: SessionSnapshot,
        transport_failure: bool,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind malformed Session stream fixture");
        let descriptor = RuntimeDescriptor::new(
            format!(
                "http://{}",
                listener.local_addr().expect("read fixture address")
            ),
            "malformed-session-stream-token".to_owned(),
            ServerIdentity {
                instance_id: uuid::Uuid::new_v4(),
                pid: std::process::id(),
                protocol_version: PROTOCOL_VERSION,
                build_identity: build_identity::for_current_executable()
                    .expect("identify fixture test executable"),
            },
        );
        let runtime_dir = state_dir.join(channel);
        std::fs::create_dir_all(&runtime_dir).expect("create fixture runtime directory");
        serde_json::to_writer(
            std::fs::File::create(runtime_dir.join("runtime.json"))
                .expect("create fixture runtime descriptor"),
            &descriptor,
        )
        .expect("write fixture runtime descriptor");
        let state = Arc::new(MalformedSessionStreamState {
            descriptor,
            snapshot,
            transport_failure,
        });
        let app = Router::new()
            .route("/health", get(malformed_fixture_health))
            .route("/v1/events", get(malformed_fixture_server_events))
            .route(
                "/v1/sessions/{session_id}/events",
                get(malformed_fixture_session_events),
            )
            .with_state(state);
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve malformed Session stream fixture");
        });
        Self { task }
    }
}

impl Drop for MalformedSessionStreamFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn malformed_fixture_health(
    State(state): State<Arc<MalformedSessionStreamState>>,
    headers: HeaderMap,
) -> Response {
    if !fixture_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(state.descriptor.health(LifecycleState::Ready)).into_response()
}

async fn malformed_fixture_server_events(
    State(state): State<Arc<MalformedSessionStreamState>>,
    headers: HeaderMap,
) -> Response {
    if !fixture_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let first =
        stream::once(async move { Ok::<_, Infallible>(Event::default().comment("connected")) });
    Sse::new(first.chain(stream::pending())).into_response()
}

async fn malformed_fixture_session_events(
    State(state): State<Arc<MalformedSessionStreamState>>,
    AxumPath(session_id): AxumPath<SessionId>,
    headers: HeaderMap,
) -> Response {
    if !fixture_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if session_id != state.snapshot.session.id {
        return StatusCode::NOT_FOUND.into_response();
    }
    let snapshot = state.snapshot.clone();
    if state.transport_failure {
        let snapshot_event = format!(
            "event: {SESSION_SNAPSHOT_EVENT}\nid: {}\ndata: {}\n\n",
            snapshot.revision.0,
            serde_json::to_string(&snapshot).expect("serialize fixture Session snapshot"),
        );
        let snapshot_chunk =
            stream::once(async move { Ok::<_, std::io::Error>(Bytes::from(snapshot_event)) });
        let failed_chunk = stream::once(async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Err::<Bytes, _>(std::io::Error::other("fixture Session transport failure"))
        });
        return Response::builder()
            .header(CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(snapshot_chunk.chain(failed_chunk)))
            .expect("build broken Session stream response");
    }
    let update = SessionUpdate {
        session_id,
        revision: SessionRevision::INITIAL,
        changes: Vec::new(),
    };
    let events = vec![
        Event::default()
            .event(SESSION_SNAPSHOT_EVENT)
            .id(snapshot.revision.0.to_string())
            .json_data(snapshot)
            .expect("serialize fixture Session snapshot"),
        Event::default()
            .event(SESSION_UPDATED_EVENT)
            .id(update.revision.0.to_string())
            .json_data(update)
            .expect("serialize fixture Session update"),
    ]
    .into_iter()
    .map(Ok::<_, Infallible>);
    Sse::new(stream::iter(events)).into_response()
}

fn fixture_authenticated(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == format!("Bearer {token}"))
}

#[derive(Clone)]
struct ReconnectingSessionStreamState {
    descriptor: RuntimeDescriptor,
    connections: Arc<AtomicUsize>,
    initial: SessionSnapshot,
    current: SessionSnapshot,
    update: SessionUpdate,
}

struct ReconnectingSessionStreamFixture {
    task: tokio::task::JoinHandle<()>,
}

impl ReconnectingSessionStreamFixture {
    async fn spawn(
        state_dir: &std::path::Path,
        channel: &str,
        initial: SessionSnapshot,
        current: SessionSnapshot,
        update: SessionUpdate,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind reconnecting Session stream fixture");
        let descriptor = RuntimeDescriptor::new(
            format!(
                "http://{}",
                listener.local_addr().expect("read fixture address")
            ),
            "reconnecting-session-stream-token".to_owned(),
            ServerIdentity {
                instance_id: uuid::Uuid::new_v4(),
                pid: std::process::id(),
                protocol_version: PROTOCOL_VERSION,
                build_identity: build_identity::for_current_executable()
                    .expect("identify fixture test executable"),
            },
        );
        let runtime_dir = state_dir.join(channel);
        std::fs::create_dir_all(&runtime_dir).expect("create fixture runtime directory");
        serde_json::to_writer(
            std::fs::File::create(runtime_dir.join("runtime.json"))
                .expect("create fixture runtime descriptor"),
            &descriptor,
        )
        .expect("write fixture runtime descriptor");
        let state = Arc::new(ReconnectingSessionStreamState {
            descriptor,
            connections: Arc::new(AtomicUsize::new(0)),
            initial,
            current,
            update,
        });
        let app = Router::new()
            .route("/health", get(reconnecting_fixture_health))
            .route("/v1/events", get(reconnecting_fixture_server_events))
            .route(
                "/v1/sessions/{session_id}/events",
                get(reconnecting_fixture_session_events),
            )
            .with_state(state);
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve reconnecting Session stream fixture");
        });
        Self { task }
    }
}

impl Drop for ReconnectingSessionStreamFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn reconnecting_fixture_health(
    State(state): State<Arc<ReconnectingSessionStreamState>>,
    headers: HeaderMap,
) -> Response {
    if !fixture_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(state.descriptor.health(LifecycleState::Ready)).into_response()
}

async fn reconnecting_fixture_server_events(
    State(state): State<Arc<ReconnectingSessionStreamState>>,
    headers: HeaderMap,
) -> Response {
    if !fixture_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Sse::new(
        stream::once(async move { Ok::<_, Infallible>(Event::default().comment("connected")) })
            .chain(stream::pending()),
    )
    .into_response()
}

async fn reconnecting_fixture_session_events(
    State(state): State<Arc<ReconnectingSessionStreamState>>,
    AxumPath(session_id): AxumPath<SessionId>,
    headers: HeaderMap,
) -> Response {
    if !fixture_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if session_id != state.initial.session.id {
        return StatusCode::NOT_FOUND.into_response();
    }

    if state.connections.fetch_add(1, Ordering::SeqCst) == 0 {
        let snapshot = state.initial.clone();
        return Sse::new(stream::once(async move {
            Ok::<_, Infallible>(
                Event::default()
                    .event(SESSION_SNAPSHOT_EVENT)
                    .id(snapshot.revision.0.to_string())
                    .json_data(snapshot)
                    .expect("serialize initial fixture Session snapshot"),
            )
        }))
        .into_response();
    }

    let snapshot = state.current.clone();
    let update = state.update.clone();
    let events = stream::iter([
        Event::default()
            .event(SESSION_SNAPSHOT_EVENT)
            .id(snapshot.revision.0.to_string())
            .json_data(snapshot)
            .expect("serialize current fixture Session snapshot"),
        Event::default()
            .event(SESSION_UPDATED_EVENT)
            .id(update.revision.0.to_string())
            .json_data(update)
            .expect("serialize fixture Session update"),
    ])
    .map(Ok::<_, Infallible>)
    .chain(stream::pending());
    Sse::new(events).into_response()
}
