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
        Activity, ActivityId, ActivityStatus, AdmitPromptRequest, AgentId, AgentIdentity,
        AgentSelection, AgentSelectionOperationId, CreateSessionRequest, FileChange, Health,
        InitialPrompt, LifecycleState, Message, MessageId, MessageRole, MessageStatus,
        ModelAvailability, ModelCatalog, ModelDescriptor, ModelId, ModelOptionChoice,
        ModelOptionChoiceId, ModelOptionDescriptor, ModelOptionId, ModelOptionKind,
        ModelOptionRole, ModelOptionSelection, ModelOptionValue, PROTOCOL_VERSION, Prompt,
        PromptDelivery, PromptId, PromptOrder, PromptStatus, ProviderCatalogStatus, ProviderId,
        ProviderModelCatalog, RuntimeDescriptor, SESSION_SNAPSHOT_EVENT, SESSION_UPDATED_EVENT,
        ServerIdentity, Session, SessionChange, SessionError, SessionErrorCode, SessionId,
        SessionListItem, SessionRevision, SessionSnapshot, SessionStatus, SessionSummary,
        SessionUpdate, TranscriptItem, Turn, TurnId, TurnStatus, UpdateAgentSelectionRequest,
        Workspace,
    },
    provider::{
        ProviderActivityId, ProviderCommandStatus, ProviderEvent, ProviderFileChangeStatus,
    },
    server::{self, AgentOutput, ServerConfig},
    tui::{Application, ApplicationEvent, ApplicationTransition, CommandId, SemanticCommandId},
};
use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use diesel::{Connection, SqliteConnection, connection::SimpleConnection};
use eventsource_stream::Eventsource;
use futures_util::{StreamExt, future::join_all, stream};
use ratatui::{Terminal, backend::TestBackend};
use tokio::time::{Duration, timeout};

#[path = "support/failing_provider.rs"]
mod failing_provider_support;
#[path = "support/provider.rs"]
mod provider_support;

use failing_provider_support::spawn_with_failing_provider;
use provider_support::ControlledProvider;

fn controlled_selection(model: &str, effort: &str, speed: &str) -> AgentSelection {
    AgentSelection {
        provider: ProviderId::new("controlled"),
        model: ModelId::new(model),
        options: vec![
            ModelOptionSelection {
                id: ModelOptionId::new("reasoning-opaque"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new(effort),
                },
            },
            ModelOptionSelection {
                id: ModelOptionId::new("speed-opaque"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new(speed),
                },
            },
        ],
    }
}

fn readable_session_summaries(items: Vec<SessionListItem>) -> Vec<SessionSummary> {
    items
        .into_iter()
        .map(|item| match item {
            SessionListItem::Readable(summary) => summary,
            SessionListItem::Unreadable(summary) => {
                panic!("expected readable Session {}, got unreadable", summary.id)
            }
        })
        .collect()
}

async fn next_session_update(
    subscription: &mut chidori::managed_client::SessionSubscription,
) -> SessionUpdate {
    let SessionEvent::Updated(update) = timeout(Duration::from_secs(1), subscription.next())
        .await
        .expect("Session update arrives")
        .expect("Session stream remains open")
        .expect("Session update is valid")
    else {
        panic!("expected a Session update");
    };
    update
}

async fn read_session_at_least_revision(
    client: &reqwest::Client,
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
    revision: SessionRevision,
) -> SessionSnapshot {
    timeout(Duration::from_secs(1), async {
        loop {
            let snapshot = client
                .get(format!("{}/v1/sessions/{session_id}", descriptor.base_url))
                .bearer_auth(&descriptor.token)
                .send()
                .await
                .expect("read Session while awaiting revision")
                .error_for_status()
                .expect("Session remains readable")
                .json::<SessionSnapshot>()
                .await
                .expect("decode Session while awaiting revision");
            if snapshot.revision >= revision {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Session reaches expected revision")
}

#[tokio::test]
async fn provider_session_drives_initial_prompt_through_snapshot_first_sse_for_multiple_clients() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "provider-session-test").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let mut first = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "provider-session-test")
            .expect("configure first client"),
    )
    .await
    .expect("connect first client");
    let mut second = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "provider-session-test")
            .expect("configure second client"),
    )
    .await
    .expect("connect second client");
    receive_managed_client_initial_state(&mut first).await;
    receive_managed_client_initial_state(&mut second).await;

    let prompt_id = PromptId::new();
    let created = first
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: prompt_id,
                text: "Explain the provider seam".to_owned(),
            },
        })
        .await
        .expect("create Session without waiting for Provider startup");
    assert_eq!(created.revision, SessionRevision::INITIAL);
    assert_eq!(created.session.agent_selection, None);
    assert_eq!(created.session.status, SessionStatus::Idle);
    assert_eq!(created.prompts.len(), 1);
    assert_eq!(created.prompts[0].status, PromptStatus::Pending);
    assert!(created.turns.is_empty());
    assert!(created.messages.is_empty());
    assert!(created.activities.is_empty());

    let mut first_feed = first
        .subscribe_session(created.session.id)
        .await
        .expect("subscribe first client");
    let mut second_feed = second
        .subscribe_session(created.session.id)
        .await
        .expect("subscribe second client");
    for feed in [&mut first_feed, &mut second_feed] {
        assert_eq!(
            timeout(Duration::from_secs(1), feed.next())
                .await
                .expect("Session snapshot arrives")
                .expect("Session stream remains open")
                .expect("Session snapshot is valid"),
            SessionEvent::Snapshot(created.clone())
        );
    }

    let start = timeout(Duration::from_secs(1), provider.next_start())
        .await
        .expect("Provider startup begins asynchronously");
    assert_eq!(start.workspace(), workspace.path());
    let identity = AgentIdentity {
        agent: AgentId::new("codex"),
        selection: AgentSelection {
            provider: ProviderId::new("codex"),
            model: ModelId::new("gpt-5.6-codex"),
            options: Vec::new(),
        },
    };
    let mut provider_session = start.succeed(identity.clone());
    let turn_request = timeout(Duration::from_secs(1), provider_session.next_turn())
        .await
        .expect("initial Prompt reaches the Provider Session");
    assert_eq!(turn_request.prompt(), "Explain the provider seam");

    let first_selection = next_session_update(&mut first_feed).await;
    let second_selection = next_session_update(&mut second_feed).await;
    assert_eq!(first_selection, second_selection);
    assert_eq!(first_selection.revision, SessionRevision(2));
    assert_eq!(
        first_selection.changes,
        vec![SessionChange::AgentSelectionChanged {
            selection: identity.selection.clone(),
        }]
    );

    let first_delivery = next_session_update(&mut first_feed).await;
    let second_delivery = next_session_update(&mut second_feed).await;
    assert_eq!(first_delivery, second_delivery);
    assert_eq!(first_delivery.revision, SessionRevision(3));
    let turn_id = first_delivery
        .changes
        .iter()
        .find_map(|change| match change {
            SessionChange::TurnAdded { turn } => Some(turn.id),
            _ => None,
        })
        .expect("Prompt delivery creates a Turn");
    assert!(first_delivery.changes.iter().any(|change| {
        matches!(change, SessionChange::TurnAdded { turn }
            if turn.agent.as_ref() == Some(&identity))
    }));
    assert!(first_delivery.changes.iter().any(|change| {
        matches!(change, SessionChange::PromptStatusChanged {
            prompt_id: changed_prompt_id,
            status: PromptStatus::Delivered,
        } if *changed_prompt_id == prompt_id)
    }));
    assert!(first_delivery.changes.iter().any(|change| {
        matches!(change, SessionChange::MessageAdded { message }
            if message.turn_id == turn_id
                && message.role == MessageRole::User
                && message.content == "Explain the provider seam")
    }));
    assert!(first_delivery.changes.iter().any(|change| {
        matches!(
            change,
            SessionChange::SessionStatusChanged {
                status: SessionStatus::Active,
            }
        )
    }));

    turn_request.succeed();
    for event in [
        ProviderEvent::CommandStarted {
            activity_id: ProviderActivityId::new("fixture-command"),
            command: "cargo test --test session_integration".to_owned(),
            cwd: Some(workspace.path().to_owned()),
        },
        ProviderEvent::CommandOutputDelta {
            activity_id: ProviderActivityId::new("fixture-command"),
            content: "running 1 test\n".to_owned(),
        },
        ProviderEvent::CommandOutputDelta {
            activity_id: ProviderActivityId::new("fixture-command"),
            content: "test result: ok\n".to_owned(),
        },
        ProviderEvent::CommandCompleted {
            activity_id: ProviderActivityId::new("fixture-command"),
            status: ProviderCommandStatus::Completed,
            exit_status: Some(0),
        },
        ProviderEvent::FileChangeStarted {
            activity_id: ProviderActivityId::new("fixture-file-change"),
            changes: vec![FileChange::Update {
                path: "src/protocol.rs".into(),
                moved_to: Some("src/protocol_v2.rs".into()),
            }],
        },
        ProviderEvent::FileChangeUpdated {
            activity_id: ProviderActivityId::new("fixture-file-change"),
            changes: vec![
                FileChange::Update {
                    path: "src/protocol.rs".into(),
                    moved_to: Some("src/protocol_v2.rs".into()),
                },
                FileChange::Add {
                    path: "tests/session_protocol.rs".into(),
                },
            ],
        },
        ProviderEvent::FileChangeCompleted {
            activity_id: ProviderActivityId::new("fixture-file-change"),
            status: ProviderFileChangeStatus::Completed,
        },
        ProviderEvent::AgentMessageStarted,
        ProviderEvent::AgentMessageDelta {
            content: "Hello".to_owned(),
        },
        ProviderEvent::AgentMessageDelta {
            content: " from the Provider".to_owned(),
        },
        ProviderEvent::AgentMessageCompleted,
        ProviderEvent::TurnCompleted,
    ] {
        provider_session.emit(event);
        let first_update = next_session_update(&mut first_feed).await;
        let second_update = next_session_update(&mut second_feed).await;
        assert_eq!(first_update, second_update);
    }

    let completed = first
        .read_session(created.session.id)
        .await
        .expect("read completed Session");
    assert_eq!(completed.revision, SessionRevision(15));
    assert_eq!(
        completed.session.agent_selection,
        Some(identity.selection.clone())
    );
    assert_eq!(completed.session.status, SessionStatus::Idle);
    assert_eq!(completed.prompts[0].status, PromptStatus::Delivered);
    assert_eq!(completed.turns.len(), 1);
    assert_eq!(completed.turns[0].agent, Some(identity));
    assert_eq!(completed.turns[0].status, TurnStatus::Completed);
    assert_eq!(completed.messages.len(), 2);
    assert_eq!(completed.messages[0].role, MessageRole::User);
    assert_eq!(completed.messages[1].role, MessageRole::Agent);
    assert_eq!(completed.messages[1].status, MessageStatus::Completed);
    assert_eq!(completed.messages[1].content, "Hello from the Provider");
    assert_eq!(completed.activities.len(), 2);
    let Activity::Command {
        id: command_activity_id,
        status,
        command,
        cwd,
        output,
        exit_status,
        ..
    } = &completed.activities[0]
    else {
        panic!("Provider command must project as command Activity");
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(command, "cargo test --test session_integration");
    assert_eq!(cwd.as_deref(), Some(workspace.path()));
    assert_eq!(output, "running 1 test\ntest result: ok\n");
    assert_eq!(*exit_status, Some(0));
    assert_eq!(
        completed
            .transcript
            .iter()
            .filter(|item| matches!(item,
                TranscriptItem::Activity { activity_id } if activity_id == command_activity_id))
            .count(),
        1,
        "streaming command output must not duplicate transcript rows"
    );
    let Activity::FileChange {
        id: file_change_activity_id,
        status: file_change_status,
        changes,
        ..
    } = &completed.activities[1]
    else {
        panic!("Provider file change must project as file-change Activity");
    };
    assert_eq!(*file_change_status, ActivityStatus::Completed);
    assert_eq!(
        changes,
        &[
            FileChange::Update {
                path: "src/protocol.rs".into(),
                moved_to: Some("src/protocol_v2.rs".into()),
            },
            FileChange::Add {
                path: "tests/session_protocol.rs".into(),
            },
        ]
    );
    assert_eq!(
        completed
            .transcript
            .iter()
            .filter(|item| matches!(item,
                TranscriptItem::Activity { activity_id }
                    if activity_id == file_change_activity_id))
            .count(),
        1,
        "file-change updates must not duplicate transcript rows"
    );
    assert!(matches!(
        completed.transcript.as_slice(),
        [
            TranscriptItem::Message { .. },
            TranscriptItem::Activity { activity_id: command_id },
            TranscriptItem::Activity { activity_id: file_change_id },
            TranscriptItem::Message { .. },
        ] if command_id == command_activity_id && file_change_id == file_change_activity_id
    ));

    let failing_prompt_id = PromptId::new();
    let admitted = first
        .admit_prompt(
            created.session.id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: failing_prompt_id,
                    text: "Fail while a command is active".to_owned(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("admit a second Prompt");
    assert_eq!(admitted.status, PromptStatus::Pending);
    let first_admission = next_session_update(&mut first_feed).await;
    let second_admission = next_session_update(&mut second_feed).await;
    assert_eq!(first_admission, second_admission);

    let failing_turn_request = provider_session.next_turn().await;
    let first_delivery = next_session_update(&mut first_feed).await;
    let second_delivery = next_session_update(&mut second_feed).await;
    assert_eq!(first_delivery, second_delivery);
    let failing_turn_id = first_delivery
        .changes
        .iter()
        .find_map(|change| match change {
            SessionChange::TurnAdded { turn } => Some(turn.id),
            _ => None,
        })
        .expect("second Prompt delivery creates a Turn");
    failing_turn_request.succeed();

    provider_session.emit(ProviderEvent::CommandStarted {
        activity_id: ProviderActivityId::new("failed-command"),
        command: "cargo test --all".to_owned(),
        cwd: None,
    });
    let first_start = next_session_update(&mut first_feed).await;
    let second_start = next_session_update(&mut second_feed).await;
    assert_eq!(first_start, second_start);
    provider_session.emit(ProviderEvent::FileChangeStarted {
        activity_id: ProviderActivityId::new("failed-file-change"),
        changes: vec![FileChange::Update {
            path: "src/provider.rs".into(),
            moved_to: None,
        }],
    });
    let first_file_change = next_session_update(&mut first_feed).await;
    let second_file_change = next_session_update(&mut second_feed).await;
    assert_eq!(first_file_change, second_file_change);
    provider_session.emit(ProviderEvent::TurnFailed {
        message: "Provider stopped while the command was running".to_owned(),
    });
    let first_failure = next_session_update(&mut first_feed).await;
    let second_failure = next_session_update(&mut second_feed).await;
    assert_eq!(first_failure, second_failure);

    let failed = first
        .read_session(created.session.id)
        .await
        .expect("read Session after active command failure");
    assert_eq!(
        failed
            .turns
            .iter()
            .find(|turn| turn.id == failing_turn_id)
            .map(|turn| turn.status),
        Some(TurnStatus::Failed)
    );
    assert!(failed.activities.iter().any(|activity| matches!(activity,
        Activity::Command {
            status: ActivityStatus::Failed,
            command,
            exit_status: None,
            ..
        } if command == "cargo test --all")));
    assert!(failed.activities.iter().any(|activity| matches!(activity,
    Activity::FileChange {
        status: ActivityStatus::Failed,
        changes,
        ..
    } if changes == &[FileChange::Update {
        path: "src/provider.rs".into(),
        moved_to: None,
    }])));

    drop(provider_session);
    drop(first_feed);
    drop(second_feed);
    drop(first);
    drop(second);
    server.shutdown().await.expect("shut down server");
}

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
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Keep this Turn on its effective Agent".to_owned(),
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
async fn provider_session_steers_the_active_turn_only_after_provider_acceptance() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "provider-steering-test").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "provider-steering-test")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_managed_client_initial_state(&mut client).await;
    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Begin through the Provider seam".to_owned(),
            },
        })
        .await
        .expect("create Session");
    let preactive_prompt_id = PromptId::new();
    client
        .admit_prompt(
            created.session.id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: preactive_prompt_id,
                    text: "Remain pending across Provider startup".to_owned(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("admit Prompt before the initial Turn becomes active");
    let start = provider.next_start().await;
    let identity = AgentIdentity {
        agent: AgentId::new("codex"),
        selection: AgentSelection {
            provider: ProviderId::new("codex"),
            model: ModelId::new("controlled-model"),
            options: Vec::new(),
        },
    };
    let mut provider_session = start.succeed(identity);
    let initial_turn = provider_session.next_turn().await;
    initial_turn.succeed();
    let mut feed = client
        .subscribe_session(created.session.id)
        .await
        .expect("subscribe to Session SSE");
    feed.next()
        .await
        .expect("Session feed remains open")
        .expect("Session snapshot is valid");

    let accepted_prompt_id = PromptId::new();
    client
        .admit_prompt(
            created.session.id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: accepted_prompt_id,
                    text: "Accept this steer".to_owned(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("admit accepted steer");
    next_session_update(&mut feed).await;
    let accepted = provider_session.next_steer().await;
    assert_eq!(accepted.prompt(), "Accept this steer");
    assert_eq!(
        client
            .read_session(created.session.id)
            .await
            .expect("read pending steer")
            .prompts
            .iter()
            .find(|prompt| prompt.id == accepted_prompt_id)
            .expect("accepted steer remains authoritative")
            .status,
        PromptStatus::Pending
    );
    accepted.succeed();
    next_session_update(&mut feed).await;

    let rejected_prompt_id = PromptId::new();
    client
        .admit_prompt(
            created.session.id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: rejected_prompt_id,
                    text: "Reject this steer".to_owned(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("admit rejected steer");
    next_session_update(&mut feed).await;
    let rejected = provider_session.next_steer().await;
    assert_eq!(rejected.prompt(), "Reject this steer");
    rejected.fail("controlled steering rejection");
    next_session_update(&mut feed).await;

    let following_prompt_id = PromptId::new();
    client
        .admit_prompt(
            created.session.id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: following_prompt_id,
                    text: "Accept the steer after rejection".to_owned(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("admit steer after rejection");
    next_session_update(&mut feed).await;
    let following = provider_session.next_steer().await;
    assert_eq!(following.prompt(), "Accept the steer after rejection");
    following.succeed();
    next_session_update(&mut feed).await;

    let snapshot = client
        .read_session(created.session.id)
        .await
        .expect("read steered Session");
    assert_eq!(snapshot.turns.len(), 1);
    assert_eq!(snapshot.turns[0].status, TurnStatus::Active);
    assert_eq!(
        snapshot
            .prompts
            .iter()
            .find(|prompt| prompt.id == preactive_prompt_id)
            .expect("preactive Prompt remains authoritative")
            .status,
        PromptStatus::Pending
    );
    assert_eq!(
        snapshot
            .prompts
            .iter()
            .find(|prompt| prompt.id == following_prompt_id)
            .expect("following steer remains authoritative")
            .status,
        PromptStatus::Delivered
    );
    assert_eq!(
        snapshot
            .prompts
            .iter()
            .find(|prompt| prompt.id == accepted_prompt_id)
            .expect("accepted steer remains authoritative")
            .status,
        PromptStatus::Delivered
    );
    assert_eq!(
        snapshot
            .prompts
            .iter()
            .find(|prompt| prompt.id == rejected_prompt_id)
            .expect("rejected steer remains authoritative")
            .status,
        PromptStatus::Pending
    );
    assert_eq!(
        snapshot
            .messages
            .iter()
            .filter(|message| message.content == "Accept this steer")
            .count(),
        1
    );
    assert!(
        !snapshot
            .messages
            .iter()
            .any(|message| message.content == "Reject this steer")
    );
    assert!(snapshot.activities.iter().any(|activity| matches!(activity,
                Activity::Error { text, .. } if text.contains("controlled steering rejection"))));

    provider_session.emit(ProviderEvent::TurnCompleted);
    next_session_update(&mut feed).await;
    drop(provider_session);
    drop(feed);
    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn provider_failures_fail_only_the_affected_turn_and_leave_the_session_usable() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "provider-failure-test").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "provider-failure-test")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_managed_client_initial_state(&mut client).await;

    let initial_prompt_id = PromptId::new();
    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: initial_prompt_id,
                text: "Fail during startup".to_owned(),
            },
        })
        .await
        .expect("create Session");
    let mut feed = client
        .subscribe_session(created.session.id)
        .await
        .expect("subscribe to Session");
    assert_eq!(
        timeout(Duration::from_secs(1), feed.next())
            .await
            .expect("initial snapshot arrives")
            .expect("Session stream remains open")
            .expect("initial snapshot is valid"),
        SessionEvent::Snapshot(created.clone())
    );

    provider
        .next_start()
        .await
        .fail("the deterministic runtime could not start");
    let startup_failure = next_session_update(&mut feed).await;
    assert_eq!(startup_failure.revision, SessionRevision(2));
    assert!(startup_failure.changes.iter().any(|change| {
        matches!(change, SessionChange::TurnAdded { turn }
            if turn.prompt_id == initial_prompt_id && turn.status == TurnStatus::Failed)
    }));
    assert!(startup_failure.changes.iter().any(|change| {
        matches!(change, SessionChange::ActivityAdded {
            activity: Activity::Error { text, .. },
        } if text.contains("deterministic runtime could not start"))
    }));

    let execution_prompt_id = PromptId::new();
    let execution_prompt = client
        .admit_prompt(
            created.session.id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: execution_prompt_id,
                    text: "Fail while starting the Turn".to_owned(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("admit a Prompt after startup failure");
    assert_eq!(execution_prompt.status, PromptStatus::Pending);
    let admitted = next_session_update(&mut feed).await;
    assert_eq!(admitted.revision, SessionRevision(3));

    let retry = provider.next_start().await;
    let identity = AgentIdentity {
        agent: AgentId::new("codex"),
        selection: AgentSelection {
            provider: ProviderId::new("codex"),
            model: ModelId::new("gpt-5.6-codex"),
            options: Vec::new(),
        },
    };
    let mut provider_session = retry.succeed(identity.clone());
    assert_eq!(
        next_session_update(&mut feed).await.revision,
        SessionRevision(4)
    );
    let delivery = next_session_update(&mut feed).await;
    assert_eq!(delivery.revision, SessionRevision(5));
    let execution_turn_id = delivery
        .changes
        .iter()
        .find_map(|change| match change {
            SessionChange::TurnAdded { turn } => Some(turn.id),
            _ => None,
        })
        .expect("Provider delivery creates the execution Turn");
    provider_session
        .next_turn()
        .await
        .fail("the deterministic Provider rejected the Turn");
    let execution_failure = next_session_update(&mut feed).await;
    assert_eq!(execution_failure.revision, SessionRevision(6));
    assert!(execution_failure.changes.iter().any(|change| {
        matches!(change, SessionChange::ActivityAdded {
            activity: Activity::Error { turn_id, text, .. },
        } if *turn_id == execution_turn_id
            && text.contains("deterministic Provider rejected the Turn"))
    }));
    assert!(execution_failure.changes.iter().any(|change| {
        matches!(change, SessionChange::TurnStatusChanged {
            turn_id,
            status: TurnStatus::Failed,
        } if *turn_id == execution_turn_id)
    }));

    let recovery_prompt_id = PromptId::new();
    let recovery_prompt = client
        .admit_prompt(
            created.session.id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: recovery_prompt_id,
                    text: "Succeed after both failures".to_owned(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("admit a Prompt after execution failure");
    assert_eq!(recovery_prompt.status, PromptStatus::Pending);
    assert_eq!(
        next_session_update(&mut feed).await.revision,
        SessionRevision(7)
    );
    let recovery_delivery = next_session_update(&mut feed).await;
    assert_eq!(recovery_delivery.revision, SessionRevision(8));
    let recovery_turn_id = recovery_delivery
        .changes
        .iter()
        .find_map(|change| match change {
            SessionChange::TurnAdded { turn } => Some(turn.id),
            _ => None,
        })
        .expect("recovery Prompt creates a Turn");
    provider_session.next_turn().await.succeed();
    provider_session.emit(ProviderEvent::TurnCompleted);
    let recovered = next_session_update(&mut feed).await;
    assert_eq!(recovered.revision, SessionRevision(9));

    let snapshot = client
        .read_session(created.session.id)
        .await
        .expect("read recovered Session");
    assert_eq!(snapshot.session.agent_selection, Some(identity.selection));
    assert_eq!(snapshot.session.status, SessionStatus::Idle);
    assert_eq!(snapshot.turns.len(), 3);
    assert_eq!(snapshot.turns[0].status, TurnStatus::Failed);
    assert_eq!(snapshot.turns[1].status, TurnStatus::Failed);
    assert_eq!(snapshot.turns[2].id, recovery_turn_id);
    assert_eq!(snapshot.turns[2].status, TurnStatus::Completed);
    assert_eq!(snapshot.activities.len(), 2);

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
            "workspace": { "path": workspace.path() },
            "prompt": {
                "id": PromptId::new(),
                "text": "Begin with my landing selection"
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
            agent_selection: None,
            workspace: Workspace {
                path: first_workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Use the existing default".to_owned(),
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
            agent_selection: None,
            workspace: Workspace {
                path: second_workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Reuse my remembered selection".to_owned(),
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
            agent_selection: Some(initial),
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Wait for a selected Turn".to_owned(),
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
            agent_selection: Some(opaque_cycling_selection("low")),
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Cycle Reasoning Effort rapidly".to_owned(),
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
    let mut application = Application::new(workspace.path());
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
            agent_selection: Some(AgentSelection {
                provider: ProviderId::new("controlled"),
                model: ModelId::new("initial"),
                options: Vec::new(),
            }),
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Wait while clients select concurrently".to_owned(),
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
            agent_selection: Some(first_selection.clone()),
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Begin on A".to_owned(),
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
            agent_selection: Some(requested.clone()),
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Use the selected Model".to_owned(),
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
    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: Some(selection.clone()),
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: original_prompt_id,
                text: "Retry me deliberately".to_owned(),
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
    turn.reject_selection("selected Model is unavailable");

    let failed = timeout(Duration::from_secs(1), async {
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
    assert_eq!(failed.prompts[1].text, "Retry me deliberately");
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
    let repeated = timeout(Duration::from_secs(1), provider_session.next_turn())
        .await
        .expect("first use of the operation schedules the restored Prompt");
    assert_eq!(repeated.prompt(), "Retry me deliberately");
    repeated.reject_selection("selected Model remains unavailable");

    let failed_again = timeout(Duration::from_secs(1), async {
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
    assert_eq!(failed_again.turns[1].prompt_id, first_retry_prompt_id);
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
    let retry = timeout(Duration::from_secs(1), provider_session.next_turn())
        .await
        .expect("restored Prompt is scheduled after Agent Selection recovery");
    assert_eq!(retry.prompt(), "Retry me deliberately");
    assert_eq!(retry.selection(), &retry_selection);
    retry.succeed();

    let retried = client
        .read_session(created.session.id)
        .await
        .expect("read retried Session");
    assert_eq!(retried.turns.len(), 3);
    assert_eq!(retried.turns[2].prompt_id, retry_prompt_id);
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
            agent_selection: None,
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
    assert_eq!(snapshot.session.agent_selection, None);
    assert_eq!(snapshot.session.status, SessionStatus::Idle);
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
    assert_eq!(failed.turns[0].prompt_id, prompt_id);
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
        agent_selection: None,
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
            agent_selection: None,
            workspace: request.workspace.clone(),
            prompt: InitialPrompt {
                id: prompt_id,
                text: "Different content".to_owned(),
            },
        },
        CreateSessionRequest {
            agent_selection: None,
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
            agent_selection: None,
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
    assert_eq!(admitted.status, PromptStatus::Pending);

    let update_event = timeout(Duration::from_secs(1), events.next())
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

    let failure_event = timeout(Duration::from_secs(1), events.next())
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
            SessionChange::TurnAdded { turn } if turn.prompt_id == prompt_id => Some(turn.id),
            _ => None,
        })
        .expect("delivered steer creates a Turn");
    assert!(failure.changes.iter().any(|change| {
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
    let retried = exact_retry
        .json::<Prompt>()
        .await
        .expect("decode retried Prompt");
    assert_eq!(retried.id, admitted.id);
    assert_eq!(retried.text, admitted.text);
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
            agent_selection: None,
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
                    },
                },
                SessionChange::TurnAdded {
                    turn: Turn {
                        id: active_turn_id,
                        prompt_id: active_prompt_id,
                        agent: None,
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
                            agent: None,
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
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Long-running work".to_owned(),
            },
        })
        .await
        .expect("create Session");
    let session_id = created.session.id;
    let start = provider.next_start().await;
    let mut provider_session = start.succeed(AgentIdentity {
        agent: AgentId::new("codex"),
        selection: AgentSelection {
            provider: ProviderId::new("codex"),
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

    let (acknowledged, ()) =
        tokio::join!(first.interrupt_turn(session_id, active_turn_id), async {
            provider_session.next_interrupt().await.succeed();
        });
    let acknowledged = acknowledged.expect("Provider acknowledges interruption");
    assert_eq!(acknowledged.status, TurnStatus::Active);
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
    assert_eq!(current.session.status, SessionStatus::Active);
    assert_eq!(current.turns.len(), 2);
    assert_eq!(current.turns[1].prompt_id, after_interrupt.id);
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
        message.turn_id == current.turns[1].id && message.content == "Run after interruption"
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
        PromptStatus::Delivered
    );

    let queued_start = provider_session.next_turn().await;
    assert_eq!(queued_start.prompt(), "Run after interruption");
    queued_start.succeed();
    provider_session.emit(ProviderEvent::TurnCompleted);
    timeout(Duration::from_secs(1), async {
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
            agent_selection: None,
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

    let mut admitted_prompts = Vec::new();
    for offset in 1..=64 {
        let event = timeout(Duration::from_secs(1), events.next())
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
async fn authenticated_clients_can_read_a_session_by_id() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
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
            agent_selection: None,
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
    let settled = read_session_at_least_revision(
        &client,
        &descriptor,
        created.session.id,
        SessionRevision(2),
    )
    .await;
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
    assert_eq!(read, settled);

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
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "session-list-test").expect("configure server"),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let client = reqwest::Client::new();

    let create = |path: &std::path::Path, text: &str| CreateSessionRequest {
        agent_selection: None,
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
        .json::<Vec<SessionListItem>>()
        .await
        .expect("decode Session summaries");
    let summaries = readable_session_summaries(summaries);
    assert_eq!(
        summaries
            .iter()
            .map(|summary| summary.session.id)
            .collect::<Vec<_>>(),
        vec![second.session.id, first.session.id]
    );
    assert_eq!(summaries[0].title, "Second Session");
    assert_eq!(summaries[0].session.workspace, second.session.workspace);
    assert_eq!(summaries[0].session.agent_selection, None);
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
        .json::<Vec<SessionListItem>>()
        .await
        .expect("decode filtered Session summaries");
    let filtered = readable_session_summaries(filtered);
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
async fn session_metadata_remains_listed_after_a_server_restart() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let initial_selection = controlled_selection("gpt-persisted", "low", "slow");
    let updated_selection = controlled_selection("gpt-persisted", "high", "fast");
    let config = ServerConfig::new(state_dir.path(), "session-metadata-restart-test")
        .expect("configure original server")
        .with_data_dir(data_dir.path());
    let (original_runtime, _original_provider) = ControlledProvider::new();
    let original = server::spawn_with_provider(config.clone(), original_runtime)
        .await
        .expect("spawn original server");
    let original_descriptor = original.descriptor().clone();
    let session = reqwest::Client::new()
        .post(format!("{}/v1/sessions", original_descriptor.base_url))
        .bearer_auth(&original_descriptor.token)
        .json(&CreateSessionRequest {
            agent_selection: Some(initial_selection),
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "  Durable Session  ".to_owned(),
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
    reqwest::Client::new()
        .post(format!(
            "{}/v1/sessions/{}/agent-selection",
            original_descriptor.base_url, session.session.id
        ))
        .bearer_auth(&original_descriptor.token)
        .json(&UpdateAgentSelectionRequest {
            operation_id: AgentSelectionOperationId::new(),
            selection: updated_selection.clone(),
        })
        .send()
        .await
        .expect("update Agent Selection before restart")
        .error_for_status()
        .expect("Agent Selection update succeeds before restart");
    let before_restart = reqwest::Client::new()
        .get(format!("{}/v1/sessions", original_descriptor.base_url))
        .bearer_auth(&original_descriptor.token)
        .send()
        .await
        .expect("list Sessions before restart")
        .error_for_status()
        .expect("Session listing succeeds before restart")
        .json::<Vec<SessionListItem>>()
        .await
        .expect("decode Session summaries before restart");
    let before_restart = readable_session_summaries(before_restart);
    assert_eq!(before_restart.len(), 1);
    assert_eq!(before_restart[0].session.id, session.session.id);
    assert_eq!(before_restart[0].title, "Durable Session");
    assert_eq!(
        before_restart[0].session.agent_selection,
        Some(updated_selection)
    );
    assert_eq!(
        before_restart[0].session.workspace.path,
        std::fs::canonicalize(workspace.path()).expect("canonicalize expected Workspace")
    );
    assert!(before_restart[0].created_at < before_restart[0].updated_at);
    original.shutdown().await.expect("stop original server");

    let (replacement_runtime, _replacement_provider) = ControlledProvider::new();
    let replacement = server::spawn_with_provider(config, replacement_runtime)
        .await
        .expect("spawn replacement server");
    let replacement_descriptor = replacement.descriptor().clone();
    let after_restart = reqwest::Client::new()
        .get(format!("{}/v1/sessions", replacement_descriptor.base_url))
        .bearer_auth(&replacement_descriptor.token)
        .send()
        .await
        .expect("list Sessions after restart")
        .error_for_status()
        .expect("Session listing succeeds after restart")
        .json::<Vec<SessionListItem>>()
        .await
        .expect("decode Session summaries after restart");
    let after_restart = readable_session_summaries(after_restart);
    assert_eq!(after_restart, before_restart);

    replacement
        .shutdown()
        .await
        .expect("shut down replacement server");
}

#[tokio::test]
async fn completed_transcript_is_readable_after_a_server_restart() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let config = ServerConfig::new(state_dir.path(), "transcript-restart-test")
        .expect("configure original server")
        .with_data_dir(data_dir.path());
    let (original_runtime, mut original_provider) = ControlledProvider::new();
    let original = server::spawn_with_provider(config.clone(), original_runtime)
        .await
        .expect("spawn original server");
    let descriptor = original.descriptor().clone();
    let client = reqwest::Client::new();
    let created = client
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Persist this whole Turn".to_owned(),
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

    let start = timeout(Duration::from_secs(1), original_provider.next_start())
        .await
        .expect("Provider startup begins");
    let mut provider_session = start.succeed(AgentIdentity {
        agent: AgentId::new("controlled-agent"),
        selection: controlled_selection("gpt-persisted", "high", "fast"),
    });
    timeout(Duration::from_secs(1), provider_session.next_turn())
        .await
        .expect("initial Turn reaches Provider")
        .succeed();
    for event in [
        ProviderEvent::CommandStarted {
            activity_id: ProviderActivityId::new("persisted-command"),
            command: "cargo test".to_owned(),
            cwd: Some(workspace.path().to_owned()),
        },
        ProviderEvent::CommandOutputDelta {
            activity_id: ProviderActivityId::new("persisted-command"),
            content: "first delta\n".to_owned(),
        },
        ProviderEvent::CommandOutputDelta {
            activity_id: ProviderActivityId::new("persisted-command"),
            content: "second delta\n".to_owned(),
        },
        ProviderEvent::CommandCompleted {
            activity_id: ProviderActivityId::new("persisted-command"),
            status: ProviderCommandStatus::Completed,
            exit_status: Some(0),
        },
        ProviderEvent::AgentMessageStarted,
        ProviderEvent::AgentMessageDelta {
            content: "streamed ".to_owned(),
        },
        ProviderEvent::AgentMessageDelta {
            content: "answer".to_owned(),
        },
        ProviderEvent::AgentMessageCompleted,
        ProviderEvent::TurnCompleted,
    ] {
        provider_session.emit(event);
    }
    let completed = read_session_at_least_revision(
        &client,
        &descriptor,
        created.session.id,
        SessionRevision(12),
    )
    .await;
    assert_eq!(completed.turns[0].status, TurnStatus::Completed);
    assert_eq!(completed.messages[1].content, "streamed answer");
    original.shutdown().await.expect("stop original server");

    let database_path = config.data_dir().join("chidori.db");
    let mut database = SqliteConnection::establish(
        database_path
            .to_str()
            .expect("fixture database path is valid UTF-8"),
    )
    .expect("open persisted Transcript fixture");
    database
        .batch_execute(
            r#"
            UPDATE sessions
            SET workspace = substr(workspace, 1, length(workspace) - 1) || ',"future_field":true}',
                agent_selection = substr(agent_selection, 1, length(agent_selection) - 1) || ',"future_field":true}';
            UPDATE prompts
            SET payload = substr(payload, 1, length(payload) - 1) || ',"future_field":true}';
            UPDATE turns
            SET payload = substr(payload, 1, length(payload) - 1) || ',"future_field":true}';
            UPDATE messages
            SET payload = substr(payload, 1, length(payload) - 1) || ',"future_field":true}';
            UPDATE activities
            SET payload = substr(payload, 1, length(payload) - 1) || ',"future_field":true}';
            "#,
        )
        .expect("add unknown fields to persisted JSON payloads");

    let (replacement_runtime, _replacement_provider) = ControlledProvider::new();
    let replacement = server::spawn_with_provider(config, replacement_runtime)
        .await
        .expect("spawn replacement server");
    let reopened = reqwest::Client::new()
        .get(format!(
            "{}/v1/sessions/{}",
            replacement.descriptor().base_url,
            created.session.id
        ))
        .bearer_auth(&replacement.descriptor().token)
        .send()
        .await
        .expect("reopen persisted Session")
        .error_for_status()
        .expect("persisted Session remains readable")
        .json::<SessionSnapshot>()
        .await
        .expect("decode reopened Session");
    assert_eq!(reopened, completed);

    replacement
        .shutdown()
        .await
        .expect("stop replacement server");
}

#[tokio::test]
async fn persisted_session_without_resume_state_starts_a_fresh_provider_conversation() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let config = ServerConfig::new(state_dir.path(), "missing-resume-state-test")
        .expect("configure original server")
        .with_data_dir(data_dir.path());
    let (original_runtime, mut original_provider) = ControlledProvider::new();
    let original = server::spawn_with_provider(config.clone(), original_runtime)
        .await
        .expect("spawn original server");
    let original_descriptor = original.descriptor().clone();
    let client = reqwest::Client::new();
    let created = client
        .post(format!("{}/v1/sessions", original_descriptor.base_url))
        .bearer_auth(&original_descriptor.token)
        .json(&CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Persist without Provider Resume State".to_owned(),
            },
        })
        .send()
        .await
        .expect("create original Session")
        .error_for_status()
        .expect("Session creation succeeds")
        .json::<SessionSnapshot>()
        .await
        .expect("decode created Session");
    let original_start = original_provider.next_start().await;
    assert!(original_start.resume_state().is_none());
    let mut original_session = original_start.succeed(AgentIdentity {
        agent: AgentId::new("controlled-agent"),
        selection: controlled_selection("gpt-fresh", "low", "slow"),
    });
    original_session.next_turn().await.succeed();
    original_session.emit(ProviderEvent::TurnCompleted);
    read_session_at_least_revision(
        &client,
        &original_descriptor,
        created.session.id,
        SessionRevision(4),
    )
    .await;
    original.shutdown().await.expect("stop original server");

    let (replacement_runtime, mut replacement_provider) = ControlledProvider::new();
    let replacement = server::spawn_with_provider(config, replacement_runtime)
        .await
        .expect("spawn replacement server");
    let replacement_descriptor = replacement.descriptor().clone();
    client
        .post(format!(
            "{}/v1/sessions/{}/prompts",
            replacement_descriptor.base_url, created.session.id
        ))
        .bearer_auth(&replacement_descriptor.token)
        .json(&AdmitPromptRequest {
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Continue without Provider Resume State".to_owned(),
            },
            delivery: PromptDelivery::Steer,
        })
        .send()
        .await
        .expect("admit Prompt to restored Session")
        .error_for_status()
        .expect("restored Session accepts a Prompt");
    let replacement_start = timeout(Duration::from_secs(1), replacement_provider.next_start())
        .await
        .expect("restored Session starts a Provider conversation");
    assert!(replacement_start.resume_state().is_none());
    let mut replacement_session = replacement_start.succeed(AgentIdentity {
        agent: AgentId::new("controlled-agent"),
        selection: controlled_selection("gpt-fresh", "low", "slow"),
    });
    let continued = replacement_session.next_turn().await;
    assert_eq!(continued.prompt(), "Continue without Provider Resume State");
    continued.succeed();
    replacement_session.emit(ProviderEvent::TurnCompleted);
    let reopened = read_session_at_least_revision(
        &client,
        &replacement_descriptor,
        created.session.id,
        SessionRevision(7),
    )
    .await;
    assert_eq!(reopened.turns.len(), 2);
    assert_eq!(reopened.turns[1].status, TurnStatus::Completed);

    replacement
        .shutdown()
        .await
        .expect("stop replacement server");
}

#[tokio::test]
async fn an_undecodable_stored_session_does_not_block_startup_and_remains_listed() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let config = ServerConfig::new(state_dir.path(), "unreadable-session-test")
        .expect("configure original server")
        .with_data_dir(data_dir.path());
    let original = spawn_with_failing_provider(config.clone())
        .await
        .expect("spawn original server");
    let created = reqwest::Client::new()
        .post(format!("{}/v1/sessions", original.descriptor().base_url))
        .bearer_auth(&original.descriptor().token)
        .json(&CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Keep this damaged Session visible".to_owned(),
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

    let database_path = config.data_dir().join("chidori.db");
    let mut database = SqliteConnection::establish(
        database_path
            .to_str()
            .expect("fixture database path is valid UTF-8"),
    )
    .expect("open persisted Session fixture");
    database
        .batch_execute("UPDATE prompts SET payload = '{';")
        .expect("doctor one stored Prompt payload");

    let replacement = spawn_with_failing_provider(config)
        .await
        .expect("an unreadable Session must not block startup");
    let listed = reqwest::Client::new()
        .get(format!("{}/v1/sessions", replacement.descriptor().base_url))
        .bearer_auth(&replacement.descriptor().token)
        .send()
        .await
        .expect("list Sessions after restart")
        .error_for_status()
        .expect("Session listing succeeds after restart")
        .json::<Vec<SessionListItem>>()
        .await
        .expect("decode Session listing");
    assert_eq!(listed.len(), 1);
    let SessionListItem::Unreadable(unreadable) = &listed[0] else {
        panic!("doctored Session must be marked unreadable");
    };
    assert_eq!(unreadable.id, created.session.id);
    assert_eq!(unreadable.title, "Keep this damaged Session visible");

    let mut application = Application::new(workspace.path());
    let ApplicationTransition::ListSessions(request) = application
        .handle_event(ApplicationEvent::Command(CommandId::InvokeSemantic(
            SemanticCommandId::SessionList,
        )))
        .expect("open Session picker")
    else {
        panic!("opening the Session picker must request Sessions");
    };
    application
        .handle_event(ApplicationEvent::SessionsListed {
            request,
            sessions: listed,
        })
        .expect("load Session picker");
    let mut terminal = Terminal::new(TestBackend::new(80, 15)).expect("create test terminal");
    terminal
        .draw(|frame| application.render(frame))
        .expect("render Session picker");
    let picker = terminal
        .backend()
        .buffer()
        .content()
        .chunks(80)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(picker.contains("Keep this damaged Session visible"));
    assert!(picker.contains("[unreadable]"));
    assert_eq!(
        application
            .handle_terminal_event(InputEvent::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .expect("unreadable Session owns no attachment action"),
        ApplicationTransition::Continue
    );

    replacement
        .shutdown()
        .await
        .expect("stop replacement server");
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
            agent_selection: None,
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
            agent_selection: None,
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
            agent_selection: None,
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
    let server = spawn_with_failing_provider(
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
            agent_selection: None,
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
    let first_snapshot = serde_json::from_str::<SessionSnapshot>(&first.data)
        .expect("decode Session snapshot event");
    assert_eq!(first_snapshot.session.id, created.session.id);
    assert_eq!(first.id, first_snapshot.revision.0.to_string());

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
    let fresh_snapshot_body = serde_json::from_str::<SessionSnapshot>(&fresh_snapshot.data)
        .expect("decode fresh Session snapshot event");
    assert_eq!(fresh_snapshot_body.session.id, created.session.id);
    assert_eq!(
        fresh_snapshot.id,
        fresh_snapshot_body.revision.0.to_string()
    );
    assert!(fresh_snapshot_body.revision >= first_snapshot.revision);

    drop(reconnected_events);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn real_session_stream_appends_and_completes_one_stable_agent_message() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
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
            agent_selection: None,
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
    let settled = read_session_at_least_revision(
        &reqwest::Client::new(),
        server.descriptor(),
        session_id,
        SessionRevision(2),
    )
    .await;
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
        SessionEvent::Snapshot(settled.clone())
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
                        agent: None,
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
    assert_eq!(
        active_update.revision,
        SessionRevision(settled.revision.0 + 1)
    );
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
            assert_eq!(
                update.revision,
                SessionRevision(settled.revision.0 + index as u64 + 2)
            );
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
    assert_eq!(completed.revision, SessionRevision(settled.revision.0 + 5));

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
    let server = spawn_with_failing_provider(
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
            agent_selection: None,
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
    let server = spawn_with_failing_provider(
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
            agent_selection: None,
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
    let settled = read_session_at_least_revision(
        &reqwest::Client::new(),
        server.descriptor(),
        created.session.id,
        SessionRevision(2),
    )
    .await;
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
        SessionEvent::Snapshot(settled.clone())
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
        SessionEvent::Snapshot(settled)
    );

    drop(second_subscription);
    drop(second_client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn managed_client_can_discover_read_and_attach_to_a_known_session() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
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
            agent_selection: None,
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
    let settled = read_session_at_least_revision(
        &reqwest::Client::new(),
        server.descriptor(),
        created.session.id,
        SessionRevision(2),
    )
    .await;

    assert_eq!(
        client
            .read_session(created.session.id)
            .await
            .expect("read Session through managed client"),
        settled
    );
    let summaries = client
        .list_sessions(Some(workspace.path()))
        .await
        .expect("discover Sessions through managed client");
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].id(), created.session.id);

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
        SessionEvent::Snapshot(settled)
    );

    drop(attachment);
    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn managed_client_switching_away_does_not_interrupt_an_active_turn() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
        ServerConfig::new(state_dir.path(), "managed-session-switch-test")
            .expect("configure server"),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "managed-session-switch-test")
            .expect("configure managed client"),
    )
    .await
    .expect("connect managed client");
    receive_managed_client_initial_state(&mut client).await;

    let first = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "First Session".to_owned(),
            },
        })
        .await
        .expect("create first Session");
    let first = read_session_at_least_revision(
        &reqwest::Client::new(),
        server.descriptor(),
        first.session.id,
        SessionRevision(2),
    )
    .await;
    let second = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Second Session".to_owned(),
            },
        })
        .await
        .expect("create second Session");
    let second = read_session_at_least_revision(
        &reqwest::Client::new(),
        server.descriptor(),
        second.session.id,
        SessionRevision(2),
    )
    .await;

    let mut first_attachment = client
        .attach_session(first.session.id)
        .await
        .expect("attach first Session");
    assert!(matches!(
        first_attachment.next().await,
        Some(Ok(SessionEvent::Snapshot(_)))
    ));
    let prompt_id = PromptId::new();
    let turn_id = TurnId::new();
    server
        .session_event_sink()
        .publish(
            first.session.id,
            vec![
                SessionChange::PromptAdded {
                    prompt: Prompt {
                        id: prompt_id,
                        text: "Keep working while detached".to_owned(),
                        delivery: PromptDelivery::Steer,
                        admission_order: PromptOrder(2),
                        status: PromptStatus::Delivered,
                    },
                },
                SessionChange::TurnAdded {
                    turn: Turn {
                        id: turn_id,
                        prompt_id,
                        agent: None,
                        status: TurnStatus::Active,
                    },
                },
                SessionChange::MessageAdded {
                    message: Message {
                        id: MessageId::new(),
                        turn_id,
                        role: MessageRole::User,
                        status: MessageStatus::Completed,
                        content: "Keep working while detached".to_owned(),
                    },
                },
                SessionChange::SessionStatusChanged {
                    status: SessionStatus::Active,
                },
            ],
        )
        .expect("start active Turn");
    assert!(matches!(
        first_attachment.next().await,
        Some(Ok(SessionEvent::Updated(_)))
    ));

    drop(first_attachment);
    let mut second_attachment = client
        .attach_session(second.session.id)
        .await
        .expect("switch attachment to second Session");
    assert!(matches!(
        second_attachment.next().await,
        Some(Ok(SessionEvent::Snapshot(_)))
    ));
    let still_active = client
        .read_session(first.session.id)
        .await
        .expect("read detached first Session");
    assert_eq!(still_active.session.status, SessionStatus::Active);
    assert_eq!(
        still_active
            .turns
            .iter()
            .find(|turn| turn.id == turn_id)
            .expect("active Turn remains in detached Session")
            .status,
        TurnStatus::Active
    );

    drop(second_attachment);
    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn two_clients_converge_on_one_session_without_observing_another_session() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = spawn_with_failing_provider(
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
            agent_selection: None,
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
            agent_selection: None,
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
    let shared_settled = read_session_at_least_revision(
        &reqwest::Client::new(),
        server.descriptor(),
        shared.session.id,
        SessionRevision(2),
    )
    .await;
    let isolated_settled = read_session_at_least_revision(
        &reqwest::Client::new(),
        server.descriptor(),
        isolated.session.id,
        SessionRevision(2),
    )
    .await;
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
    assert_eq!(
        first_projection,
        SessionEvent::Snapshot(shared_settled.clone())
    );
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
        SessionEvent::Snapshot(isolated_settled)
    );

    let before_update = first_client
        .list_sessions(None)
        .await
        .expect("list Sessions before update");
    assert_eq!(before_update[0].id(), isolated.session.id);
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
                        agent: None,
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
                    activity: Activity::Status {
                        id: ActivityId::new(),
                        turn_id,
                        text: "Working".to_owned(),
                    },
                },
                SessionChange::SessionStatusChanged {
                    status: SessionStatus::Active,
                },
            ],
        )
        .expect("publish provider-neutral Session changes");
    assert_eq!(
        update.revision,
        SessionRevision(shared_settled.revision.0 + 1)
    );

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
    let updated_summary = after_update[0]
        .readable()
        .expect("updated Session is readable");
    assert_eq!(updated_summary.session.id, shared.session.id);
    assert_eq!(updated_summary.session.status, SessionStatus::Active);
    assert!(updated_summary.updated_at > updated_summary.created_at);

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
    current.activities.push(Activity::Status {
        id: ActivityId::new(),
        turn_id: current.turns[0].id,
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
            agent_selection: None,
            agent_selection_availability: ModelAvailability::Available,
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
            agent: None,
            status: TurnStatus::Failed,
        }],
        messages: vec![Message {
            id: message_id,
            turn_id,
            role: MessageRole::User,
            status: MessageStatus::Completed,
            content: "Explain this workspace".to_owned(),
        }],
        activities: vec![Activity::Error {
            id: activity_id,
            turn_id,
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
