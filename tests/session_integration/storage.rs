//! Session persistence: discovery, restart recovery, and undecodable records.

use crate::server_support::PROGRESS_DEADLINE;
use crate::{
    failing_provider_support::spawn_with_failing_provider,
    provider_support::ControlledProvider,
    support::{controlled_selection, read_session_at_least_revision},
};
use crossterm::event::{Event as InputEvent, KeyCode, KeyEvent, KeyModifiers};
use diesel::{Connection, SqliteConnection, connection::SimpleConnection};
use ratatui::{Terminal, backend::TestBackend, style::Color};
use suru::{
    managed_client::SessionEvent,
    protocol::{
        Activity, AdmitPromptRequest, AgentId, AgentIdentity, AgentSelectionOperationId, Cost,
        CostBasis, CreateSessionRequest, InitialPrompt, LatestTurnStatus, PromptDelivery, PromptId,
        SessionError, SessionErrorCode, SessionId, SessionListItem, SessionRevision,
        SessionSnapshot, SessionStatus, SessionSummary, SkillId, SkillInvocation, SkillMarkerSpan,
        TurnStatus, UpdateAgentSelectionRequest, Usage,
    },
    provider::{MeteredCost, ProviderActivityId, ProviderCommandStatus, ProviderEvent},
    server::{self, ServerConfig},
    tui::{Application, ApplicationEvent, ApplicationTransition, CommandId, SemanticCommandId},
};
use tokio::time::timeout;
use uuid::Uuid;

fn readable_session_summaries(items: Vec<SessionListItem>) -> Vec<SessionSummary> {
    items
        .into_iter()
        .map(|item| match item {
            SessionListItem::Readable(summary) => *summary,
            SessionListItem::Unreadable(summary) => {
                panic!("expected readable Session {}, got unreadable", summary.id)
            }
        })
        .collect()
}

#[tokio::test]
async fn safe_skill_invocations_are_readable_after_a_server_restart() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let config = ServerConfig::new(state_dir.path(), "skill-history-restart-test")
        .expect("configure original server")
        .with_data_dir(data_dir.path());
    let invocation = SkillInvocation {
        skill_id: SkillId::new("safe-review-id"),
        name: "review".to_owned(),
        scope: Some("Workspace".to_owned()),
        marker: SkillMarkerSpan { start: 0, end: 7 },
    };
    let original = spawn_with_failing_provider(config.clone())
        .await
        .expect("spawn original server");
    let created = reqwest::Client::new()
        .post(format!("{}/v1/sessions", original.descriptor().base_url))
        .bearer_auth(&original.descriptor().token)
        .json(&CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "$review persisted work".to_owned(),
                skill_invocations: vec![invocation.clone()],
            },
        })
        .send()
        .await
        .expect("create Skill-bearing Session")
        .error_for_status()
        .expect("Session creation succeeds")
        .json::<SessionSnapshot>()
        .await
        .expect("decode created Session");
    let before_restart = read_session_at_least_revision(
        &reqwest::Client::new(),
        original.descriptor(),
        created.session.id,
        SessionRevision(2),
    )
    .await;
    original.shutdown().await.expect("stop original server");

    let replacement = spawn_with_failing_provider(config)
        .await
        .expect("spawn replacement server");
    let restored = reqwest::Client::new()
        .get(format!(
            "{}/v1/sessions/{}",
            replacement.descriptor().base_url,
            created.session.id
        ))
        .bearer_auth(&replacement.descriptor().token)
        .send()
        .await
        .expect("read restored Session")
        .error_for_status()
        .expect("restored Session is readable")
        .json::<SessionSnapshot>()
        .await
        .expect("decode restored Session");

    assert_eq!(restored, before_restart);
    assert_eq!(
        restored.prompts[0].skill_invocations,
        vec![invocation.clone()]
    );
    assert_eq!(restored.messages[0].content, "$review persisted work");
    assert_eq!(restored.messages[0].skill_invocations, vec![invocation]);

    let mut application = Application::new(workspace.path(), Default::default());
    application
        .handle_event(ApplicationEvent::Session(SessionEvent::snapshot(restored)))
        .expect("hydrate restored Skill-bearing Session");
    let mut terminal = Terminal::new(TestBackend::new(80, 15)).expect("create test terminal");
    terminal
        .draw(|frame| application.render(frame))
        .expect("render restored Transcript");
    let cells = terminal.backend().buffer().content();
    let marker = cells
        .windows(7)
        .rev()
        .find(|window| window.iter().map(|cell| cell.symbol()).collect::<String>() == "$review")
        .expect("restored recognized marker is visible");
    assert!(
        marker.iter().all(|cell| cell.fg == Color::Cyan),
        "stored binding accents its marker without consulting the current catalog"
    );
    let transcript = terminal
        .backend()
        .buffer()
        .content()
        .chunks(80)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(transcript.contains("$review persisted work"));

    replacement
        .shutdown()
        .await
        .expect("shut down replacement server");
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
        preparation_id: None,
        agent_selection: None,
        execution_directory: suru::protocol::ExecutionDirectory {
            path: path.to_owned(),
        },
        prompt: InitialPrompt {
            id: PromptId::new(),
            text: text.to_owned(),
            skill_invocations: Vec::new(),
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
        suru::paths::canonical(first_workspace).expect("canonicalize expected Workspace")
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
            preparation_id: None,
            agent_selection: Some(initial_selection),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "  Durable Session  ".to_owned(),
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
        suru::paths::canonical(workspace.path()).expect("canonicalize expected Workspace")
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
    // A Prompt admitted to begin a Turn is owed one only by the process that
    // admitted it, so a restored Session with no durable Turn is no longer at
    // work; everything else about its metadata survives (ADR 0024).
    assert_eq!(before_restart[0].session.status, SessionStatus::Active);
    assert!(before_restart[0].session.working_since.is_some());
    assert_eq!(after_restart[0].session.status, SessionStatus::Idle);
    assert_eq!(after_restart[0].session.working_since, None);
    assert_eq!(
        after_restart,
        before_restart
            .into_iter()
            .map(|mut summary| {
                summary.session.status = SessionStatus::Idle;
                summary.session.working_since = None;
                summary
            })
            .collect::<Vec<_>>()
    );

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
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Persist this whole Turn".to_owned(),
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
        .expect("decode created Session");

    let start = timeout(PROGRESS_DEADLINE, original_provider.next_start())
        .await
        .expect("Provider startup begins");
    let mut provider_session = start.succeed(AgentIdentity {
        agent: AgentId::new("controlled-agent"),
        selection: controlled_selection("gpt-persisted", "high", "fast"),
    });
    timeout(PROGRESS_DEADLINE, provider_session.next_turn())
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
        ProviderEvent::CommandStarted {
            activity_id: ProviderActivityId::new("capped-command"),
            command: "emit unbounded output".to_owned(),
            cwd: Some(workspace.path().to_owned()),
        },
        ProviderEvent::CommandOutputDelta {
            activity_id: ProviderActivityId::new("capped-command"),
            content: "z".repeat(70 * 1024),
        },
        ProviderEvent::CommandCompleted {
            activity_id: ProviderActivityId::new("capped-command"),
            status: ProviderCommandStatus::Completed,
            exit_status: Some(0),
        },
        ProviderEvent::ReasoningStarted {
            activity_id: ProviderActivityId::new("capped-reasoning"),
        },
        ProviderEvent::ReasoningTitleChanged {
            activity_id: ProviderActivityId::new("capped-reasoning"),
            title: "Inspecting the seam".to_owned(),
        },
        ProviderEvent::ReasoningDelta {
            activity_id: ProviderActivityId::new("capped-reasoning"),
            content: "y".repeat(70 * 1024),
        },
        ProviderEvent::ReasoningCompleted {
            activity_id: ProviderActivityId::new("capped-reasoning"),
        },
        ProviderEvent::AgentMessageStarted,
        ProviderEvent::AgentMessageDelta {
            content: "streamed ".to_owned(),
        },
        ProviderEvent::AgentMessageDelta {
            content: "answer".to_owned(),
        },
        ProviderEvent::AgentMessageCompleted,
        ProviderEvent::AgentMessageStarted,
        ProviderEvent::AgentMessageDelta {
            content: "w".repeat(600 * 1024),
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
        SessionRevision(22),
    )
    .await;
    assert_eq!(completed.turns[0].status, TurnStatus::Completed);
    assert_eq!(completed.messages[1].content, "streamed answer");
    original.shutdown().await.expect("stop original server");

    let database_path = config.data_dir().join("suru.db");
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
    let Activity::Command {
        output_truncated, ..
    } = &reopened.activities[1]
    else {
        panic!("the capped Provider command projects as command Activity");
    };
    assert!(
        output_truncated,
        "a command whose output was capped stays truncated across a restart"
    );
    let Activity::Reasoning {
        title,
        content_truncated,
        duration_ms,
        ..
    } = &reopened.activities[2]
    else {
        panic!("the capped Provider Reasoning projects as a Reasoning Activity");
    };
    assert_eq!(
        title.as_deref(),
        Some("Inspecting the seam"),
        "a Reasoning title stays a typed property across a restart"
    );
    assert!(
        content_truncated,
        "Reasoning whose content was capped stays truncated across a restart"
    );
    assert!(
        duration_ms.is_some(),
        "the time a Reasoning block took survives a restart"
    );
    assert!(
        !reopened.messages[1].truncated,
        "a Message that ran to its end stays untruncated across a restart"
    );
    assert!(
        reopened.messages[2].truncated,
        "a Message whose content was capped stays truncated across a restart"
    );

    replacement
        .shutdown()
        .await
        .expect("stop replacement server");
}

#[tokio::test]
async fn persisted_session_without_resume_state_starts_a_fresh_provider_conversation() {
    resume_after_summary_mutation(None, false, false).await;
}

#[tokio::test]
async fn summary_mutation_of_an_unopened_session_preserves_opaque_resume_state_and_history() {
    resume_after_summary_mutation(
        Some(serde_json::json!({
            "opaque/provider-token": ["resume-session", {"generation": 3}],
            "unknown_future_fields": {"keep": true}
        })),
        false,
        false,
    )
    .await;
}

#[tokio::test]
async fn legacy_path_only_session_preserves_exact_execution_directory_and_resume_state() {
    resume_after_summary_mutation(
        Some(serde_json::json!({"opaque": ["legacy", 7]})),
        true,
        false,
    )
    .await;
}

#[tokio::test]
async fn regrouped_session_resumes_in_its_exact_execution_directory() {
    resume_after_summary_mutation(
        Some(serde_json::json!({"opaque": ["regrouped", 9]})),
        false,
        true,
    )
    .await;
}

#[tokio::test]
async fn legacy_worktree_session_regroups_without_changing_opaque_resume_or_exact_directory() {
    resume_after_summary_mutation(
        Some(serde_json::json!({"opaque": ["legacy-worktree", 17]})),
        true,
        true,
    )
    .await;
}

async fn resume_after_summary_mutation(
    resume_state: Option<serde_json::Value>,
    legacy: bool,
    regroup: bool,
) {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let execution_root = if regroup {
        let root = suru::paths::canonical(workspace.path()).unwrap();
        crate::repositories::git(&root, &["init", "-b", "main"]);
        crate::repositories::git(
            &root,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ],
        );
        let linked = root.join("external checkout");
        crate::repositories::git(
            &root,
            &["worktree", "add", "-b", "topic", linked.to_str().unwrap()],
        );
        linked
    } else {
        workspace.path().to_owned()
    };
    let execution_directory = execution_root
        .join("packages")
        .join("nested agent directory");
    std::fs::create_dir_all(&execution_directory).unwrap();
    let execution_directory = suru::paths::canonical(execution_directory).unwrap();
    let grouping_directory = if regroup {
        suru::paths::canonical(workspace.path()).unwrap()
    } else {
        execution_directory.clone()
    };
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
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: execution_directory.clone(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Persist without Provider Resume State".to_owned(),
                skill_invocations: Vec::new(),
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
    assert_eq!(original_start.execution_directory(), execution_directory);
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

    if legacy || regroup {
        use diesel::RunQueryDsl;
        let location = if legacy {
            serde_json::json!({"path": execution_directory})
        } else {
            serde_json::json!({"path": grouping_directory, "execution_directory": {"path": execution_directory}})
        };
        let mut database =
            SqliteConnection::establish(config.data_dir().join("suru.db").to_str().unwrap())
                .unwrap();
        diesel::sql_query("UPDATE sessions SET workspace = ? WHERE id = ?")
            .bind::<diesel::sql_types::Text, _>(location.to_string())
            .bind::<diesel::sql_types::Text, _>(created.session.id.to_string())
            .execute(&mut database)
            .unwrap();
    }
    if let Some(payload) = &resume_state {
        use diesel::RunQueryDsl;
        let mut database =
            SqliteConnection::establish(config.data_dir().join("suru.db").to_str().unwrap())
                .unwrap();
        diesel::sql_query("INSERT INTO provider_resume_states(session_id, provider, payload) VALUES (?, 'controlled', ?)")
            .bind::<diesel::sql_types::Text, _>(created.session.id.to_string())
            .bind::<diesel::sql_types::Text, _>(payload.to_string())
            .execute(&mut database).unwrap();
    }
    // The first operation after restart mutates only the summary, but the
    // writer must retain complete durable history and opaque Resume State.
    let (checkpoint_runtime, _checkpoint_provider) = ControlledProvider::new();
    let checkpoint = server::spawn_with_provider(config.clone(), checkpoint_runtime)
        .await
        .unwrap();
    checkpoint.workspace_discovery_settled().await;
    let listed = client
        .get(format!("{}/v1/sessions", checkpoint.descriptor().base_url))
        .bearer_auth(&checkpoint.descriptor().token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Vec<SessionListItem>>()
        .await
        .unwrap();
    let listed = readable_session_summaries(listed);
    assert_eq!(
        listed[0].session.execution_directory.path,
        execution_directory
    );
    assert_eq!(listed[0].session.workspace.path, grouping_directory);
    client
        .post(format!(
            "{}/v1/sessions/{}/settlement",
            checkpoint.descriptor().base_url,
            created.session.id
        ))
        .bearer_auth(&checkpoint.descriptor().token)
        .json(&suru::protocol::SettleSessionRequest { settled: true })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    checkpoint.shutdown().await.unwrap();

    let (replacement_runtime, mut replacement_provider) = ControlledProvider::new();
    let replacement = server::spawn_with_provider(config, replacement_runtime)
        .await
        .expect("spawn replacement server");
    replacement.workspace_discovery_settled().await;
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
                skill_invocations: Vec::new(),
            },
            delivery: PromptDelivery::Steer,
        })
        .send()
        .await
        .expect("admit Prompt to restored Session")
        .error_for_status()
        .expect("restored Session accepts a Prompt");
    let replacement_start = timeout(PROGRESS_DEADLINE, replacement_provider.next_start())
        .await
        .expect("restored Session starts a Provider conversation");
    assert_eq!(
        replacement_start
            .resume_state()
            .map(|state| state.payload()),
        resume_state.as_ref()
    );
    assert_eq!(replacement_start.execution_directory(), execution_directory);
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
    assert_eq!(
        reopened.messages[0].content,
        "Persist without Provider Resume State"
    );
    assert_eq!(
        reopened.session.execution_directory.path,
        execution_directory
    );
    assert_eq!(reopened.session.workspace.path, grouping_directory);
    assert_eq!(reopened.turns.len(), 2);
    assert_eq!(reopened.turns[1].status, TurnStatus::Completed);

    replacement
        .shutdown()
        .await
        .expect("stop replacement server");
}

#[tokio::test]
async fn unopened_history_is_not_decoded_or_rewritten_and_failed_hydration_invalidates_its_listing()
{
    use futures_util::StreamExt;
    use suru::protocol::SessionCatalogChange;
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
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Keep this damaged Session visible".to_owned(),
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
        .expect("decode created Session");
    original.shutdown().await.expect("stop original server");

    let database_path = config.data_dir().join("suru.db");
    let mut database = SqliteConnection::establish(
        database_path
            .to_str()
            .expect("fixture database path is valid UTF-8"),
    )
    .expect("open persisted Session fixture");
    database
        .batch_execute("UPDATE prompts SET payload = '{';")
        .expect("doctor one stored Prompt payload");

    let restart_config = config.clone();
    let replacement = spawn_with_failing_provider(config)
        .await
        .expect("an unreadable Session must not block startup");
    let initial = reqwest::Client::new()
        .get(format!("{}/v1/sessions", replacement.descriptor().base_url))
        .bearer_auth(&replacement.descriptor().token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Vec<SessionListItem>>()
        .await
        .unwrap();
    assert!(
        matches!(&initial[..], [SessionListItem::Readable(summary)] if summary.session.id == created.session.id && summary.session.execution_directory == created.session.execution_directory),
        "listing does not inspect unopened Prompt content"
    );
    replacement.shutdown().await.unwrap();
    // A shutdown with no history access must not flush partial writer state
    // over the durable Prompt; its corruption must still be discovered below.
    let replacement = spawn_with_failing_provider(restart_config).await.unwrap();
    let (_, mut events) =
        crate::support::open_catalog_stream_with_snapshot(replacement.descriptor()).await;
    let failed = reqwest::Client::new()
        .get(format!(
            "{}/v1/sessions/{}",
            replacement.descriptor().base_url,
            created.session.id
        ))
        .bearer_auth(&replacement.descriptor().token)
        .send()
        .await
        .unwrap();
    assert_eq!(failed.status(), reqwest::StatusCode::NOT_FOUND);
    let changed = timeout(PROGRESS_DEADLINE, events.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        changed.change,
        SessionCatalogChange::Invalidated {
            session_id: created.session.id
        }
    );
    drop(events);
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

    let mut application = Application::new(workspace.path(), Default::default());
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
async fn turn_timing_survives_a_restart_and_a_session_stored_before_it_stays_readable() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let config = ServerConfig::new(state_dir.path(), "turn-timing-storage-test")
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
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Persist when this Turn worked".to_owned(),
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
        .expect("decode created Session");

    let mut provider_session = timeout(PROGRESS_DEADLINE, original_provider.next_start())
        .await
        .expect("Provider startup begins")
        .succeed(AgentIdentity {
            agent: AgentId::new("controlled-agent"),
            selection: controlled_selection("gpt-timed", "high", "fast"),
        });
    timeout(PROGRESS_DEADLINE, provider_session.next_turn())
        .await
        .expect("initial Turn reaches Provider")
        .succeed();
    provider_session.emit(ProviderEvent::TurnCompleted);
    let completed = read_session_at_least_revision(
        &client,
        &descriptor,
        created.session.id,
        SessionRevision(4),
    )
    .await;
    assert_eq!(completed.turns[0].status, TurnStatus::Completed);
    assert!(
        completed.turns[0].started_at.is_some() && completed.turns[0].settled_at.is_some(),
        "a settled Turn knows when it started and when it settled"
    );
    drop(provider_session);
    original.shutdown().await.expect("stop original server");

    let (restart_runtime, _restart_provider) = ControlledProvider::new();
    let restarted = server::spawn_with_provider(config.clone(), restart_runtime)
        .await
        .expect("spawn restarted server");
    let reopened = read_persisted_session(restarted.descriptor(), created.session.id).await;
    assert_eq!(
        reopened.turns, completed.turns,
        "Turn timing survives a restart"
    );
    let listing = client
        .get(format!("{}/v1/sessions", restarted.descriptor().base_url))
        .bearer_auth(&restarted.descriptor().token)
        .send()
        .await
        .expect("list Sessions after restart")
        .error_for_status()
        .expect("restored listing succeeds")
        .json::<Vec<SessionListItem>>()
        .await
        .expect("decode restored listing");
    assert_eq!(
        listing[0]
            .readable()
            .expect("the restored Session remains readable")
            .standing_inputs
            .latest_turn,
        Some(LatestTurnStatus {
            status: TurnStatus::Completed,
            settled_at: completed.turns[0].settled_at,
        }),
        "the restored listing derives its latest Turn reading from durable Turns"
    );
    restarted.shutdown().await.expect("stop restarted server");

    let database_path = config.data_dir().join("suru.db");
    let mut database = SqliteConnection::establish(
        database_path
            .to_str()
            .expect("fixture database path is valid UTF-8"),
    )
    .expect("open persisted Turn fixture");
    database
        .batch_execute(
            "UPDATE turns SET payload = json_remove(payload, '$.started_at', '$.settled_at');",
        )
        .expect("age the stored Turn back to before Suru recorded Turn timing");
    drop(database);

    let (replacement_runtime, _replacement_provider) = ControlledProvider::new();
    let replacement = server::spawn_with_provider(config, replacement_runtime)
        .await
        .expect("spawn replacement server");
    let aged = read_persisted_session(replacement.descriptor(), created.session.id).await;
    assert_eq!(
        aged.turns[0].started_at, None,
        "a Turn stored before Suru recorded Turn timing decodes without it"
    );
    assert_eq!(aged.turns[0].settled_at, None);
    assert_eq!(
        SessionSnapshot {
            turns: completed.turns.clone(),
            ..aged.clone()
        },
        completed,
        "only Turn timing is missing from a Session stored before it"
    );
    replacement
        .shutdown()
        .await
        .expect("stop replacement server");
}

#[tokio::test]
async fn turn_usage_survives_a_restart() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let config = ServerConfig::new(state_dir.path(), "turn-usage-storage-test")
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
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Persist this Turn's Usage".to_owned(),
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
        .expect("decode created Session");

    let mut provider_session = timeout(PROGRESS_DEADLINE, original_provider.next_start())
        .await
        .expect("Provider startup begins")
        .succeed(AgentIdentity {
            agent: AgentId::new("controlled-agent"),
            selection: controlled_selection("gpt-metered", "high", "fast"),
        });
    timeout(PROGRESS_DEADLINE, provider_session.next_turn())
        .await
        .expect("initial Turn reaches Provider")
        .succeed();
    provider_session.emit(ProviderEvent::Usage {
        usage: Usage {
            fresh_input_tokens: Some(4_000),
            cache_read_tokens: Some(800),
            cache_write_tokens: Some(200),
            output_tokens: Some(190),
            reasoning_tokens: Some(10),
            native_meter: None,
            model_context_window: Some(200_000),
        },
        cost: Cost::from_usd(0.03).map(MeteredCost::reported),
    });
    provider_session.emit(ProviderEvent::TurnCompleted);
    let completed = read_session_at_least_revision(
        &client,
        &descriptor,
        created.session.id,
        SessionRevision(5),
    )
    .await;
    assert_eq!(completed.turns[0].cost_basis, Some(CostBasis::Reported));
    drop(provider_session);
    original.shutdown().await.expect("stop original server");

    let (restart_runtime, _restart_provider) = ControlledProvider::new();
    let restarted = server::spawn_with_provider(config.clone(), restart_runtime)
        .await
        .expect("spawn restarted server");
    let reopened = read_persisted_session(restarted.descriptor(), created.session.id).await;
    assert_eq!(
        reopened.turns, completed.turns,
        "Turn Usage and frozen Cost survive a restart"
    );
    restarted.shutdown().await.expect("stop restarted server");
}

#[tokio::test]
async fn a_session_stored_before_turn_usage_stays_readable_through_the_server_api() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let config = ServerConfig::new(state_dir.path(), "legacy-turn-usage-storage-test")
        .expect("configure fixture server")
        .with_data_dir(data_dir.path());
    std::fs::create_dir_all(config.data_dir()).expect("create fixture data directory");
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/pre_usage_session.db"
        ),
        config.data_dir().join("suru.db"),
    )
    .expect("copy the pre-Usage database fixture");
    let (runtime, _provider) = ControlledProvider::new();
    let running = server::spawn_with_provider(config, runtime)
        .await
        .expect("spawn server over the pre-Usage database fixture");
    let session_id = SessionId::from_uuid(
        Uuid::parse_str("0198b27e-26ec-7c4c-a83b-a83a4787453f")
            .expect("fixture Session ID is valid"),
    );

    let restored = read_persisted_session(running.descriptor(), session_id).await;

    assert_eq!(restored.turns.len(), 1);
    assert_eq!(restored.turns[0].status, TurnStatus::Completed);
    assert_eq!(restored.turns[0].usage, None);
    assert_eq!(restored.turns[0].cost, None);
    assert_eq!(restored.turns[0].cost_basis, None);
    running.shutdown().await.expect("stop fixture server");
}

#[tokio::test]
async fn a_restored_summary_reads_the_turn_it_cut_off_as_settled() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let config = ServerConfig::new(state_dir.path(), "working-since-storage-test")
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
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Leave this Turn running".to_owned(),
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
        .expect("decode created Session");

    let mut provider_session = timeout(PROGRESS_DEADLINE, original_provider.next_start())
        .await
        .expect("Provider startup begins")
        .succeed(AgentIdentity {
            agent: AgentId::new("controlled-agent"),
            selection: controlled_selection("gpt-working", "high", "fast"),
        });
    timeout(PROGRESS_DEADLINE, provider_session.next_turn())
        .await
        .expect("initial Turn reaches Provider")
        .succeed();
    let running = read_session_at_least_revision(
        &client,
        &descriptor,
        created.session.id,
        SessionRevision(3),
    )
    .await;
    assert_eq!(running.turns[0].status, TurnStatus::Active);
    drop(provider_session);
    original.shutdown().await.expect("stop original server");

    // The Turn was still running when the server went down, so it is stored
    // running; nothing survives a stop to finish it, so the restart settles
    // it and the listing reads no live work off it (ADR 0029).
    let (restart_runtime, _restart_provider) = ControlledProvider::new();
    let restarted = server::spawn_with_provider(config, restart_runtime)
        .await
        .expect("spawn restarted server");
    let summaries = readable_session_summaries(
        reqwest::Client::new()
            .get(format!("{}/v1/sessions", restarted.descriptor().base_url))
            .bearer_auth(&restarted.descriptor().token)
            .send()
            .await
            .expect("list restored Sessions")
            .error_for_status()
            .expect("Session listing succeeds")
            .json::<Vec<SessionListItem>>()
            .await
            .expect("decode the restored listing"),
    );
    let restored = summaries
        .into_iter()
        .find(|summary| summary.session.id == created.session.id)
        .expect("the Session is listed after the restart");
    assert_eq!(restored.session.status, SessionStatus::Idle);
    assert_eq!(
        restored.session.working_since, None,
        "a restored listing reads no live work from a Turn no process is running"
    );
    let latest = restored
        .standing_inputs
        .latest_turn
        .expect("the settled Turn is the latest Turn");
    assert_eq!(latest.status, TurnStatus::Failed);
    restarted.shutdown().await.expect("stop restarted server");
}

async fn read_persisted_session(
    descriptor: &suru::protocol::RuntimeDescriptor,
    session_id: SessionId,
) -> SessionSnapshot {
    reqwest::Client::new()
        .get(format!("{}/v1/sessions/{session_id}", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("reopen persisted Session")
        .error_for_status()
        .expect("persisted Session remains readable")
        .json::<SessionSnapshot>()
        .await
        .expect("decode reopened Session")
}
