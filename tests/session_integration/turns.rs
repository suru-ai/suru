//! Turn execution: provider streaming, steering, interruption, and failure isolation.

use crate::{
    provider_support::ControlledProvider,
    support::{
        controlled_selection, next_session_update, read_session_at_least_revision,
        receive_managed_client_initial_state,
    },
};
use axum::http::StatusCode;
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, SessionEvent},
    protocol::{
        Activity, ActivityStatus, AdmitPromptRequest, AgentId, AgentIdentity, AgentSelection,
        CreateSessionRequest, FileChange, InitialPrompt, MessageRole, MessageStatus, ModelId,
        PromptDelivery, PromptId, PromptStatus, ProviderId, SessionChange, SessionError,
        SessionErrorCode, SessionRevision, SessionSnapshot, SessionStatus, TranscriptItem,
        TurnStatus, Workspace,
    },
    provider::{
        ProviderActivityId, ProviderCommandStatus, ProviderEvent, ProviderFileChangeStatus,
    },
    server::{self, ServerConfig},
};
use tokio::time::{Duration, timeout};

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
async fn provider_streams_store_only_printable_text_newlines_sgr_and_osc_8() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "provider-output-normalization-test")
            .expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let client = reqwest::Client::new();
    let descriptor = server.descriptor().clone();
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
                text: "Normalize provider output".to_owned(),
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

    let start = timeout(Duration::from_secs(1), provider.next_start())
        .await
        .expect("Provider startup begins");
    let mut provider_session = start.succeed(AgentIdentity {
        agent: AgentId::new("controlled-agent"),
        selection: controlled_selection("gpt-normalized", "high", "fast"),
    });
    timeout(Duration::from_secs(1), provider_session.next_turn())
        .await
        .expect("initial Turn reaches Provider")
        .succeed();

    for event in [
        ProviderEvent::CommandStarted {
            activity_id: ProviderActivityId::new("normalized-command"),
            command: "printf output".to_owned(),
            cwd: Some(workspace.path().to_owned()),
        },
        ProviderEvent::CommandOutputDelta {
            activity_id: ProviderActivityId::new("normalized-command"),
            content: "plain\t\x1b[38;5;".to_owned(),
        },
        ProviderEvent::CommandOutputDelta {
            activity_id: ProviderActivityId::new("normalized-command"),
            content: "42mgreen\x1b]0;discarded".to_owned(),
        },
        ProviderEvent::CommandOutputDelta {
            activity_id: ProviderActivityId::new("normalized-command"),
            content: " title\x07\r\x1b[2Kdone\x7f\n\x1b]8;id=docs;https://example".to_owned(),
        },
        ProviderEvent::CommandOutputDelta {
            activity_id: ProviderActivityId::new("normalized-command"),
            content: concat!(
                ".com/bel\x07BEL\x1b]8;;\x07 ",
                "\x1b]8;;https://example.com/st\x1b\\ST\x1b]8;;\x1b\\ ",
                "\x1b]0;discarded title\x07kept ",
                "\x1b]8;malformed\x07safe",
                "\x1b]8;;https://example.com/unterminated"
            )
            .to_owned(),
        },
        ProviderEvent::CommandCompleted {
            activity_id: ProviderActivityId::new("normalized-command"),
            status: ProviderCommandStatus::Completed,
            exit_status: Some(0),
        },
        ProviderEvent::CommandStarted {
            activity_id: ProviderActivityId::new("truncated-command"),
            command: "emit oversized styled output".to_owned(),
            cwd: Some(workspace.path().to_owned()),
        },
        ProviderEvent::CommandOutputDelta {
            activity_id: ProviderActivityId::new("truncated-command"),
            content: format!("\x1b[31m{}\x1b[", "x".repeat(65_520)),
        },
        ProviderEvent::CommandOutputDelta {
            activity_id: ProviderActivityId::new("truncated-command"),
            content: format!("{}mignored", "1".repeat(32)),
        },
        ProviderEvent::CommandCompleted {
            activity_id: ProviderActivityId::new("truncated-command"),
            status: ProviderCommandStatus::Completed,
            exit_status: Some(0),
        },
        ProviderEvent::CommandStarted {
            activity_id: ProviderActivityId::new("progress-command"),
            command: "show progress".to_owned(),
            cwd: Some(workspace.path().to_owned()),
        },
        ProviderEvent::CommandOutputDelta {
            activity_id: ProviderActivityId::new("progress-command"),
            content: "10%".to_owned(),
        },
        ProviderEvent::CommandOutputDelta {
            activity_id: ProviderActivityId::new("progress-command"),
            content: "\r50%\r\x1b[31mlonger frame".to_owned(),
        },
        ProviderEvent::CommandOutputDelta {
            activity_id: ProviderActivityId::new("progress-command"),
            content: "\r\x1b[32m100%\x1b[0m\nplain\r\nmulti\nline\r".to_owned(),
        },
        ProviderEvent::CommandCompleted {
            activity_id: ProviderActivityId::new("progress-command"),
            status: ProviderCommandStatus::Completed,
            exit_status: Some(0),
        },
        ProviderEvent::CommandStarted {
            activity_id: ProviderActivityId::new("redrawn-command"),
            command: "redraw a styled progress bar".to_owned(),
            cwd: Some(workspace.path().to_owned()),
        },
        ProviderEvent::CommandOutputDelta {
            activity_id: ProviderActivityId::new("redrawn-command"),
            content: (0..2_048)
                .map(|frame| format!("\r\x1b[31m{frame:4} [{}]", "#".repeat(32)))
                .chain(["\r\x1b[32mdone\x1b[0m\n".to_owned()])
                .collect::<String>(),
        },
        ProviderEvent::CommandCompleted {
            activity_id: ProviderActivityId::new("redrawn-command"),
            status: ProviderCommandStatus::Completed,
            exit_status: Some(0),
        },
        ProviderEvent::AgentMessageStarted,
        ProviderEvent::AgentMessageDelta {
            content: "\x1b[1;3".to_owned(),
        },
        ProviderEvent::AgentMessageDelta {
            content: "2mHello\t\x1bPdiscarded".to_owned(),
        },
        ProviderEvent::AgentMessageDelta {
            content: " payload\x1b\\\rworld\x07\n".to_owned(),
        },
        ProviderEvent::AgentMessageCompleted,
        ProviderEvent::AgentMessageStarted,
        ProviderEvent::AgentMessageDelta {
            content: format!("\x1b[31m{}\x1b[", "y".repeat(524_270)),
        },
        ProviderEvent::AgentMessageDelta {
            content: format!("{}mignored", "1".repeat(32)),
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
        SessionRevision(25),
    )
    .await;
    let Activity::Command { output, .. } = &completed.activities[0] else {
        panic!("Provider command projects as command Activity");
    };
    assert_eq!(
        output,
        concat!(
            "\x1b[38;5;42mdone\n",
            "\x1b]8;id=docs;https://example.com/bel\x07BEL\x1b]8;;\x07 ",
            "\x1b]8;;https://example.com/st\x1b\\ST\x1b]8;;\x1b\\ kept safe"
        )
    );
    let Activity::Command {
        output: truncated_output,
        output_truncated,
        ..
    } = &completed.activities[1]
    else {
        panic!("second Provider command projects as command Activity");
    };
    assert!(
        output_truncated,
        "the cap that cut the stream short is stored as a typed signal"
    );
    let truncated_text = truncated_output
        .strip_prefix("\x1b[31m")
        .and_then(|output| output.strip_suffix("\x1b[0m"))
        .expect("truncated styled output is bounded by complete SGR sequences");
    assert_eq!(truncated_text.len(), 65_520);
    assert!(truncated_text.chars().all(|character| character == 'x'));
    let Activity::Command {
        output: progress_output,
        ..
    } = &completed.activities[2]
    else {
        panic!("third Provider command projects as command Activity");
    };
    assert_eq!(progress_output, "\x1b[32m100%\x1b[0m\nplain\nmulti\nline");
    let Activity::Command {
        output: redrawn_output,
        ..
    } = &completed.activities[3]
    else {
        panic!("fourth Provider command projects as command Activity");
    };
    assert_eq!(redrawn_output, "\x1b[32mdone\x1b[0m\n");
    assert_eq!(completed.messages[1].content, "\x1b[1;32mHello    world\n");
    assert!(
        !completed.messages[1].truncated,
        "a Message that ran to its end is stored untruncated"
    );
    assert!(
        completed.messages[2].truncated,
        "the cap that cut the Message short is stored as a typed signal"
    );
    let truncated_prose = completed.messages[2]
        .content
        .strip_prefix("\x1b[31m")
        .and_then(|content| content.strip_suffix("\x1b[0m"))
        .expect("a truncated Message is bounded by complete SGR sequences");
    assert_eq!(truncated_prose.len(), 524_270);
    assert!(truncated_prose.chars().all(|character| character == 'y'));

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn reasoning_streams_into_a_titled_transcript_activity_that_settles_with_a_duration() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "reasoning-transcript-test").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "reasoning-transcript-test")
            .expect("configure client"),
    )
    .await
    .expect("connect client");

    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Explain the Transcript".to_owned(),
            },
        })
        .await
        .expect("create Session");
    let session_id = created.session.id;
    let start = provider.next_start().await;
    let mut provider_session = start.succeed(AgentIdentity {
        agent: AgentId::new("controlled-agent"),
        selection: controlled_selection("gpt-reasoning", "high", "fast"),
    });
    provider_session.next_turn().await.succeed();

    let reasoning = ProviderActivityId::new("streamed-reasoning");
    for event in [
        ProviderEvent::ReasoningStarted {
            activity_id: reasoning.clone(),
        },
        ProviderEvent::ReasoningTitleChanged {
            activity_id: reasoning.clone(),
            title: "Inspecting the seam".to_owned(),
        },
        ProviderEvent::ReasoningDelta {
            activity_id: reasoning.clone(),
            content: "Reading the projection ".to_owned(),
        },
        ProviderEvent::ReasoningDelta {
            activity_id: reasoning.clone(),
            content: "before the store.\n".to_owned(),
        },
        ProviderEvent::ReasoningCompleted {
            activity_id: reasoning,
        },
        ProviderEvent::AgentMessageStarted,
        ProviderEvent::AgentMessageDelta {
            content: "The Transcript is ordered.".to_owned(),
        },
        ProviderEvent::AgentMessageCompleted,
        ProviderEvent::TurnCompleted,
    ] {
        provider_session.emit(event);
    }

    let completed = timeout(Duration::from_secs(1), async {
        loop {
            let snapshot = client
                .read_session(session_id)
                .await
                .expect("read the reasoning Session");
            if snapshot.turns[0].status == TurnStatus::Completed {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the Turn completes");

    assert_eq!(completed.activities.len(), 1);
    let Activity::Reasoning {
        id: reasoning_activity_id,
        status,
        title,
        content,
        content_truncated,
        duration_ms,
        ..
    } = &completed.activities[0]
    else {
        panic!("Provider Reasoning must project as a Reasoning Activity");
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(title.as_deref(), Some("Inspecting the seam"));
    assert_eq!(content, "Reading the projection before the store.\n");
    assert!(!content_truncated);
    assert!(
        duration_ms.is_some(),
        "a Reasoning block that completed reports how long it took"
    );
    assert_eq!(
        completed
            .transcript
            .iter()
            .filter(|item| matches!(item,
                TranscriptItem::Activity { activity_id } if activity_id == reasoning_activity_id))
            .count(),
        1,
        "streaming Reasoning content must not duplicate transcript rows"
    );
    assert!(
        completed.transcript.iter().position(|item| matches!(item,
            TranscriptItem::Activity { activity_id } if activity_id == reasoning_activity_id))
            < completed
                .transcript
                .iter()
                .position(|item| matches!(item, TranscriptItem::Message { message_id }
                    if *message_id == completed.messages[1].id)),
        "Reasoning takes its place in the Transcript before the Message it led to"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn an_interrupted_command_stores_its_final_unterminated_output_line() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "interrupted-output-test").expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "interrupted-output-test")
            .expect("configure client"),
    )
    .await
    .expect("connect client");

    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Report progress".to_owned(),
            },
        })
        .await
        .expect("create Session");
    let session_id = created.session.id;
    let start = provider.next_start().await;
    let mut provider_session = start.succeed(AgentIdentity {
        agent: AgentId::new("controlled-agent"),
        selection: controlled_selection("gpt-normalized", "high", "fast"),
    });
    provider_session.next_turn().await.succeed();
    let active = client
        .read_session(session_id)
        .await
        .expect("read delivered initial Prompt");
    let active_turn_id = active.turns[0].id;
    let mut observer = client
        .attach_session(session_id)
        .await
        .expect("attach to the active Session");
    let SessionEvent::Snapshot(_) = observer
        .next()
        .await
        .expect("observer receives snapshot")
        .expect("observer snapshot is valid")
    else {
        panic!("attachment must begin with a Session snapshot");
    };

    for event in [
        ProviderEvent::CommandStarted {
            activity_id: ProviderActivityId::new("interrupted-command"),
            command: "report progress".to_owned(),
            cwd: Some(workspace.path().to_owned()),
        },
        ProviderEvent::CommandOutputDelta {
            activity_id: ProviderActivityId::new("interrupted-command"),
            content: "\x1b[31mstarting\nprogress 10%".to_owned(),
        },
        ProviderEvent::CommandOutputDelta {
            activity_id: ProviderActivityId::new("interrupted-command"),
            content: "\r\x1b[32mprogress 90%\x1b[1;3".to_owned(),
        },
    ] {
        provider_session.emit(event);
    }

    let (acknowledged, ()) =
        tokio::join!(client.interrupt_turn(session_id, active_turn_id), async {
            provider_session.next_interrupt().await.succeed();
        });
    acknowledged.expect("Provider acknowledges interruption");
    provider_session.emit(ProviderEvent::TurnInterrupted);
    timeout(Duration::from_secs(1), async {
        loop {
            let SessionEvent::Updated(update) = observer
                .next()
                .await
                .expect("observer stream remains open")
                .expect("Session update is valid")
            else {
                panic!("attachment sends exactly one Session snapshot");
            };
            if update.changes.iter().any(|change| {
                matches!(
                    change,
                    SessionChange::TurnStatusChanged {
                        status: TurnStatus::Interrupted,
                        ..
                    }
                )
            }) {
                return;
            }
        }
    })
    .await
    .expect("interruption update arrives");

    let interrupted = client
        .read_session(session_id)
        .await
        .expect("read interrupted Session");
    let Activity::Command { output, status, .. } = &interrupted.activities[0] else {
        panic!("Provider command projects as command Activity");
    };
    assert_eq!(output, "\x1b[31mstarting\n\x1b[0m\x1b[32mprogress 90%");
    assert_eq!(*status, ActivityStatus::Failed);

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn stopping_a_provider_actor_settles_the_command_it_left_in_flight() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (runtime, mut provider) = ControlledProvider::new();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "actor-shutdown-settle-test")
            .expect("configure server"),
        runtime,
    )
    .await
    .expect("spawn server");
    let client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "actor-shutdown-settle-test")
            .expect("configure client"),
    )
    .await
    .expect("connect client");

    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Report progress".to_owned(),
            },
        })
        .await
        .expect("create Session");
    let session_id = created.session.id;
    let start = provider.next_start().await;
    let mut provider_session = start.succeed(AgentIdentity {
        agent: AgentId::new("controlled-agent"),
        selection: controlled_selection("gpt-normalized", "high", "fast"),
    });
    provider_session.next_turn().await.succeed();
    let mut observer = client
        .attach_session(session_id)
        .await
        .expect("attach to the active Session");
    let SessionEvent::Snapshot(_) = observer
        .next()
        .await
        .expect("observer receives snapshot")
        .expect("observer snapshot is valid")
    else {
        panic!("attachment must begin with a Session snapshot");
    };

    for event in [
        ProviderEvent::CommandStarted {
            activity_id: ProviderActivityId::new("abandoned-command"),
            command: "report progress".to_owned(),
            cwd: Some(workspace.path().to_owned()),
        },
        ProviderEvent::CommandOutputDelta {
            activity_id: ProviderActivityId::new("abandoned-command"),
            content: "starting\nprogress 90%".to_owned(),
        },
    ] {
        provider_session.emit(event);
    }
    // The delta that carries the pending line is the one that publishes the line
    // before it, so its update arriving proves the normalizer holds the rest.
    let command_activity_id = timeout(Duration::from_secs(1), async {
        loop {
            for change in next_session_update(&mut observer).await.changes {
                if let SessionChange::CommandOutputAppended { activity_id, .. } = change {
                    return activity_id;
                }
            }
        }
    })
    .await
    .expect("command output reaches the Session");

    client
        .delete_session(session_id)
        .await
        .expect("delete the Session its Provider actor still owns");

    let settle_changes = timeout(Duration::from_secs(1), async {
        let mut changes = Vec::new();
        loop {
            changes.extend(next_session_update(&mut observer).await.changes);
            if changes.iter().any(|change| {
                matches!(
                    change,
                    SessionChange::TurnStatusChanged {
                        status: TurnStatus::Failed,
                        ..
                    }
                )
            }) {
                return changes;
            }
        }
    })
    .await
    .expect("stopping the Provider actor settles the Turn");

    let output_index = settle_changes
        .iter()
        .position(|change| {
            change
                == &SessionChange::CommandOutputAppended {
                    activity_id: command_activity_id,
                    content: "progress 90%".to_owned(),
                }
        })
        .expect("the pending output line is stored before the Session goes away");
    let settled_index = settle_changes
        .iter()
        .position(|change| {
            change
                == &SessionChange::CommandStatusChanged {
                    activity_id: command_activity_id,
                    status: ActivityStatus::Failed,
                    exit_status: None,
                }
        })
        .expect("the in-flight command Activity settles");
    assert!(
        output_index < settled_index,
        "stored output must reach a command Activity before it settles: {settle_changes:?}"
    );

    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn interrupting_a_turn_without_a_provider_actor_settles_its_in_flight_command() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let config =
        ServerConfig::new(state_dir.path(), "actorless-interrupt-test").expect("configure server");
    let (runtime, mut provider) = ControlledProvider::new();
    let original = server::spawn_with_provider(config.clone(), runtime)
        .await
        .expect("spawn original server");
    let original_descriptor = original.descriptor().clone();
    let http = reqwest::Client::new();

    let created = http
        .post(format!("{}/v1/sessions", original_descriptor.base_url))
        .bearer_auth(&original_descriptor.token)
        .json(&CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Report progress".to_owned(),
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
    let session_id = created.session.id;
    let start = provider.next_start().await;
    let mut provider_session = start.succeed(AgentIdentity {
        agent: AgentId::new("controlled-agent"),
        selection: controlled_selection("gpt-normalized", "high", "fast"),
    });
    provider_session.next_turn().await.succeed();
    provider_session.emit(ProviderEvent::CommandStarted {
        activity_id: ProviderActivityId::new("abandoned-command"),
        command: "report progress".to_owned(),
        cwd: Some(workspace.path().to_owned()),
    });
    let running = timeout(Duration::from_secs(1), async {
        loop {
            let snapshot = read_session_at_least_revision(
                &http,
                &original_descriptor,
                session_id,
                SessionRevision::INITIAL,
            )
            .await;
            if !snapshot.activities.is_empty() {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the command Activity reaches the Session");
    let turn_id = running.turns[0].id;
    let command_activity_id = running.activities[0].id();
    original.shutdown().await.expect("stop original server");

    let (replacement_runtime, _replacement_provider) = ControlledProvider::new();
    let replacement = server::spawn_with_provider(config, replacement_runtime)
        .await
        .expect("spawn replacement server");
    let replacement_descriptor = replacement.descriptor().clone();
    let restored = http
        .get(format!(
            "{}/v1/sessions/{session_id}",
            replacement_descriptor.base_url
        ))
        .bearer_auth(&replacement_descriptor.token)
        .send()
        .await
        .expect("read restored Session")
        .error_for_status()
        .expect("restored Session remains readable")
        .json::<SessionSnapshot>()
        .await
        .expect("decode restored Session");
    assert_eq!(
        restored.turns[0].status,
        TurnStatus::Active,
        "a restart leaves the Turn it cut off active and without a Provider actor"
    );
    let Activity::Command { status, .. } = &restored.activities[0] else {
        panic!("Provider command projects as command Activity");
    };
    assert_eq!(*status, ActivityStatus::Active);

    let refused = http
        .post(format!(
            "{}/v1/sessions/{session_id}/turns/{turn_id}/interrupt",
            replacement_descriptor.base_url
        ))
        .bearer_auth(&replacement_descriptor.token)
        .send()
        .await
        .expect("request interruption of the restored Turn");
    assert_eq!(refused.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(
        refused
            .json::<SessionError>()
            .await
            .expect("decode interruption failure")
            .code,
        SessionErrorCode::TurnInterruptionFailed
    );

    let interrupted = http
        .get(format!(
            "{}/v1/sessions/{session_id}",
            replacement_descriptor.base_url
        ))
        .bearer_auth(&replacement_descriptor.token)
        .send()
        .await
        .expect("read the failed Session")
        .error_for_status()
        .expect("the failed Session remains readable")
        .json::<SessionSnapshot>()
        .await
        .expect("decode the failed Session");
    assert_eq!(interrupted.turns[0].status, TurnStatus::Failed);
    assert_eq!(interrupted.session.status, SessionStatus::Idle);
    let Some(Activity::Command { status, .. }) = interrupted
        .activities
        .iter()
        .find(|activity| activity.id() == command_activity_id)
    else {
        panic!("the command Activity survives the failed interruption");
    };
    assert_eq!(
        *status,
        ActivityStatus::Failed,
        "an interruption that never reaches a Provider actor still settles what it left in flight"
    );

    replacement
        .shutdown()
        .await
        .expect("shut down replacement server");
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
