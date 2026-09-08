//! Turn execution, transcript projection, and durability across a restart.

use crate::{
    server_support::request_server_shutdown,
    support::{ScriptedCodex, receive_initial_state},
};
use serde_json::Value;
use std::{process::Stdio, sync::Arc};
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, SessionEvent},
    protocol::{
        Activity, ActivityId, ActivityStatus, AdmitPromptRequest, AgentId, CreateSessionRequest,
        FileChange, InitialPrompt, MessageRole, MessageStatus, ModelId, PromptDelivery, PromptId,
        PromptStatus, ProviderId, ReasoningSummaryDetail, RuntimeDescriptor, SessionChange,
        SessionSnapshot, SessionStatus, SettingMutation, ShutdownReason, TranscriptItem,
        TurnStatus,
    },
    provider::CodexRuntime,
    server::{self, ServerConfig},
    tui::{Application, ApplicationEvent},
};
use tokio::{
    process::{Child, Command},
    time::{Duration, timeout},
};

const SCRIPTED_CODEX: &str = r#"#!/bin/sh
if [ "$1" != "app-server" ]; then
  exit 64
fi

i=0
while [ "$i" -lt 5000 ]; do
  printf 'fixture diagnostic output that must stay off stdout\n' >&2
  i=$((i + 1))
done

while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":999,"result":{"ignored":"uncorrelated response"}}'
      printf '%s' '{"id":"1","result":{"userAgent":"fixture","futureField":true'
      printf '%s\n' '}}'
      ;;
    *'"method":"initialized"'*)
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"native-thread","futureField":true},"model":"gpt-fixture","modelProvider":"fixture","futureField":true}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":"3","result":{"turn":{"id":"native-turn","status":"inProgress","futureField":true}}}'
      while [ ! -e "$CODEX_FIXTURE_RELEASE" ]; do
        sleep 0.01
      done
      printf '%s\n' '{"method":"future/notification","params":{"ignored":true}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"futureItem","id":"ignored-item","payload":{"unknown":true}}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"futureItem","id":"ignored-item","payload":{"unknown":true}}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"other-thread","turn":{"id":"other-turn","status":"completed","items":[]}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"other-thread","turnId":"other-turn","item":{"type":"agentMessage","id":"other-message","text":""}}}'
      printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"other-thread","turnId":"other-turn","itemId":"other-message","delta":"wrong Session content"}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"other-thread","turnId":"other-turn","item":{"type":"agentMessage","id":"other-message","text":"wrong Session content"}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"commandExecution","id":"native-command","command":"cargo test --test codex_integration","cwd":"/fixture/work","status":"inProgress","futureField":true},"futureField":true}}'
      printf '%s\n' '{"method":"item/commandExecution/outputDelta","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"other-command","delta":"wrong command output"}}'
      printf '%s\n' '{"method":"item/commandExecution/outputDelta","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"native-command","delta":"running ","futureField":true}}'
      printf '%s\n' '{"method":"item/commandExecution/outputDelta","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"native-command","delta":"tests\n"}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"commandExecution","id":"native-command","command":"cargo test --test codex_integration","cwd":"/fixture/work","status":"completed","aggregatedOutput":"running tests\nall green\n","exitCode":0,"futureField":true},"futureField":true}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"fileChange","id":"native-file-change","changes":[{"path":"src/protocol.rs","kind":{"type":"update","movePath":null},"diff":"private start patch"}],"status":"inProgress","futureField":{"opaque":true}},"futureField":true}}'
      printf '%s\n' '{"method":"item/fileChange/patchUpdated","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"other-file-change","changes":[{"path":"wrong-session.txt","kind":{"type":"add"},"diff":"wrong item patch"}]}}'
      printf '%s\n' '{"method":"item/fileChange/patchUpdated","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"native-file-change","changes":[{"path":"src/protocol.rs","kind":{"type":"update","movePath":"src/protocol_v2.rs"},"diff":"private updated patch"},{"path":"tests/session_protocol.rs","kind":{"type":"add"},"diff":"private added patch"}],"futureField":true}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"fileChange","id":"native-file-change","changes":[{"path":"src/protocol.rs","kind":{"type":"update","movePath":"src/protocol_v2.rs"},"diff":"private final patch"},{"path":"tests/session_protocol.rs","kind":{"type":"add"},"diff":"private final test patch"},{"path":"obsolete.txt","kind":{"type":"delete"},"diff":"private deleted patch"}],"status":"completed","futureField":{"native":true}},"futureField":true}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"reasoning","id":"native-reasoning","summary":[],"content":[],"futureField":true},"futureField":true}}'
      printf '%s\n' '{"method":"item/reasoning/summaryPartAdded","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"native-reasoning","summaryIndex":0}}'
      printf '%s\n' '{"method":"item/reasoning/summaryTextDelta","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"other-reasoning","delta":"wrong reasoning content","summaryIndex":0}}'
      printf '%s\n' '{"method":"item/reasoning/summaryTextDelta","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"native-reasoning","delta":"**Inspecting the","summaryIndex":0,"futureField":true}}'
      printf '%s\n' '{"method":"item/reasoning/summaryTextDelta","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"native-reasoning","delta":" harness**\n\nReading the fixture.","summaryIndex":0}}'
      printf '%s\n' '{"method":"item/reasoning/textDelta","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"native-reasoning","delta":"private raw chain of thought","contentIndex":0}}'
      printf '%s\n' '{"method":"item/reasoning/summaryPartAdded","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"native-reasoning","summaryIndex":1}}'
      printf '%s\n' '{"method":"item/reasoning/summaryTextDelta","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"native-reasoning","delta":"Then the store.","summaryIndex":1}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"reasoning","id":"native-reasoning","summary":["**Inspecting the harness**\n\nReading the fixture.","Then the store."],"content":["private raw chain of thought"]},"futureField":true}}'
      printf '%s' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"agentMessage","id":"native-message","text":"","futureField":true},"futureField":true'
      printf '%s\n' '}}'
      printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"other-message","delta":"wrong item content"}}'
      printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"native-message","delta":"Hello","futureField":true}}'
      printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"native-message","delta":" from Codex"}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"agentMessage","id":"native-message","text":"Hello from Codex"},"futureField":true}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"completed","items":[],"futureField":true},"futureField":true}}'
      ;;
  esac
done
"#;

const DURABILITY_CODEX_PREFIX: &str = r#"#!/bin/sh
if [ "$1" != "app-server" ]; then
  exit 64
fi

while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{"userAgent":"durability-fixture"}}'
      ;;
    *'"method":"initialized"'*)
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"durable-thread"},"model":"gpt-fixture","modelProvider":"fixture"}}'
      ;;
    *'"method":"turn/start"'*)
"#;

const COMPLETED_TURN_DURABILITY_EVENTS: &str = r#"
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"durable-turn","status":"inProgress"}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"durable-thread","turnId":"durable-turn","item":{"type":"agentMessage","id":"durable-message","text":""}}}'
      printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"durable-thread","turnId":"durable-turn","itemId":"durable-message","delta":"boundary durable"}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"durable-thread","turnId":"durable-turn","item":{"type":"agentMessage","id":"durable-message","text":"boundary durable"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"durable-thread","turn":{"id":"durable-turn","status":"completed","items":[]}}}'
"#;

const IDLE_FLUSH_DURABILITY_EVENTS: &str = r#"
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"durable-turn","status":"inProgress"}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"durable-thread","turnId":"durable-turn","item":{"type":"agentMessage","id":"durable-message","text":""}}}'
      printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"durable-thread","turnId":"durable-turn","itemId":"durable-message","delta":"coalesced "}}'
      printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"durable-thread","turnId":"durable-turn","itemId":"durable-message","delta":"tail"}}'
      touch "$CODEX_FIXTURE_READY"
      while :; do sleep 1; done
"#;

const DURABILITY_CODEX_SUFFIX: &str = r#"
      ;;
  esac
done
"#;

const PERSISTED_RESUME_CODEX: &str = r#"
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"persisted-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"thread/resume"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"persisted-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"persisted-turn"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"persisted-thread","turn":{"id":"persisted-turn","status":"completed","items":[]}}}'
      ;;
"#;

const REASONING_SUMMARY_CODEX: &str = r#"
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"summary-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"summary-turn"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"summary-thread","turn":{"id":"summary-turn","status":"completed","items":[]}}}'
      ;;
"#;

#[tokio::test]
async fn the_pinned_reasoning_summary_setting_is_what_a_turn_asks_codex_for() {
    let fixture = ScriptedCodex::new_multiprocess(REASONING_SUMMARY_CODEX);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{
            // Say as much about the thinking as the Model will.
            "provider": { "codex": { "reasoningSummary": "detailed" } },
        }"#,
    )
    .expect("write Config Document");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-reasoning-summary")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-reasoning-summary")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Explain the pinned Setting".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session");

    fixture.wait_for_method("turn/start").await;

    let requests = fixture.requests();
    let started = requests
        .iter()
        .find(|request| request.get("method").and_then(Value::as_str) == Some("turn/start"))
        .expect("the Turn reached Codex");
    assert_eq!(
        started["params"]["summary"], "detailed",
        "the Turn asks Codex for the Reasoning summary detail the Config Document pinned"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_mutated_reasoning_summary_setting_governs_the_next_turn() {
    let fixture = ScriptedCodex::new_multiprocess(REASONING_SUMMARY_CODEX);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-reasoning-summary-mutation")
            .expect("configure server")
            .with_config_dir(config_dir.path()),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-reasoning-summary-mutation")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;

    client
        .mutate_setting(SettingMutation::ProviderCodexReasoningSummary {
            value: Some(ReasoningSummaryDetail::Concise),
        })
        .await
        .expect("change the Setting over the protocol");
    client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Explain the changed Setting".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session");

    fixture.wait_for_method("turn/start").await;

    let requests = fixture.requests();
    let started = requests
        .iter()
        .find(|request| request.get("method").and_then(Value::as_str) == Some("turn/start"))
        .expect("the Turn reached Codex");
    assert_eq!(
        started["params"]["summary"], "concise",
        "a Turn asks Codex for the Reasoning summary detail the mutation left in force"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn scripted_codex_runs_initial_prompt_through_stdio_and_session_sse() {
    let fixture = ScriptedCodex::new(SCRIPTED_CODEX);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-scripted-success").expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-scripted-success")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;

    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Explain the native harness".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session without waiting for Codex startup");
    assert_eq!(created.session.agent_selection, None);
    assert_eq!(created.session.status, SessionStatus::Idle);
    assert_eq!(created.prompts[0].status, PromptStatus::Pending);
    assert!(created.turns.is_empty());

    let mut feed = client
        .subscribe_session(created.session.id)
        .await
        .expect("subscribe to Session SSE");
    let initial_event = timeout(Duration::from_secs(2), feed.next())
        .await
        .expect("Session snapshot arrives")
        .expect("Session feed remains open")
        .expect("Session snapshot is valid");
    assert!(matches!(initial_event, SessionEvent::Snapshot(_)));
    let mut application = Application::new(workspace.path(), Default::default());
    application
        .handle_event(ApplicationEvent::Session(initial_event))
        .expect("initial SSE snapshot hydrates the client projection");
    fixture.wait_for_method("turn/start").await;
    fixture.release();

    let mut streamed_command_id: Option<ActivityId> = None;
    let mut streamed_command_output = String::new();
    let mut saw_command_completion = false;
    let mut streamed_file_change_id: Option<ActivityId> = None;
    let mut streamed_file_changes = Vec::new();
    let mut file_change_update_count = 0;
    let mut saw_file_change_completion = false;
    timeout(Duration::from_secs(2), async {
        loop {
            let event = feed
                .next()
                .await
                .expect("Session feed remains open")
                .expect("Session update is valid");
            let mut turn_completed = false;
            if let SessionEvent::Updated(update) = &event {
                for change in &update.changes {
                    match change {
                        SessionChange::ActivityAdded {
                            activity:
                                Activity::Command {
                                    id,
                                    status: ActivityStatus::Active,
                                    ..
                                },
                        } => {
                            assert!(
                                streamed_command_id.replace(*id).is_none(),
                                "SSE must add exactly one command Activity"
                            );
                        }
                        SessionChange::CommandOutputAppended {
                            activity_id,
                            content,
                        } => {
                            assert_eq!(Some(*activity_id), streamed_command_id);
                            streamed_command_output.push_str(content);
                        }
                        SessionChange::CommandStatusChanged {
                            activity_id,
                            status: ActivityStatus::Completed,
                            exit_status: Some(0),
                        } => {
                            assert_eq!(Some(*activity_id), streamed_command_id);
                            saw_command_completion = true;
                        }
                        SessionChange::ActivityAdded {
                            activity:
                                Activity::FileChange {
                                    id,
                                    status: ActivityStatus::Active,
                                    changes,
                                    ..
                                },
                        } => {
                            assert!(
                                streamed_file_change_id.replace(*id).is_none(),
                                "SSE must add exactly one file-change Activity"
                            );
                            streamed_file_changes.clone_from(changes);
                        }
                        SessionChange::FileChangeUpdated {
                            activity_id,
                            changes,
                        } => {
                            assert_eq!(Some(*activity_id), streamed_file_change_id);
                            streamed_file_changes.clone_from(changes);
                            file_change_update_count += 1;
                        }
                        SessionChange::FileChangeStatusChanged {
                            activity_id,
                            status: ActivityStatus::Completed,
                        } => {
                            assert_eq!(Some(*activity_id), streamed_file_change_id);
                            saw_file_change_completion = true;
                        }
                        SessionChange::TurnStatusChanged {
                            status: TurnStatus::Completed,
                            ..
                        } => turn_completed = true,
                        _ => {}
                    }
                }
            }
            application
                .handle_event(ApplicationEvent::Session(event))
                .expect("SSE update applies through the client projection");
            if turn_completed {
                break;
            }
        }
    })
    .await
    .expect("Codex Turn reaches a terminal Session state");
    assert!(streamed_command_id.is_some());
    assert_eq!(streamed_command_output, "running tests\nall green\n");
    assert!(saw_command_completion);
    assert!(streamed_file_change_id.is_some());
    assert_eq!(file_change_update_count, 2);
    assert_eq!(
        streamed_file_changes,
        [
            FileChange::Update {
                path: "src/protocol.rs".into(),
                moved_to: Some("src/protocol_v2.rs".into()),
            },
            FileChange::Add {
                path: "tests/session_protocol.rs".into(),
            },
            FileChange::Delete {
                path: "obsolete.txt".into(),
            },
        ]
    );
    assert!(saw_file_change_completion);

    let completed = client
        .read_session(created.session.id)
        .await
        .expect("read completed Session");
    let selection = completed
        .session
        .agent_selection
        .as_ref()
        .expect("effective Codex Agent Selection is published");
    assert_eq!(selection.provider, ProviderId::new("codex"));
    assert_eq!(selection.model, ModelId::new("gpt-fixture"));
    assert!(selection.options.is_empty());
    let identity = completed.turns[0]
        .agent
        .as_ref()
        .expect("Codex Turn captures its effective Agent");
    assert_eq!(identity.agent, AgentId::new("codex"));
    assert_eq!(&identity.selection, selection);
    assert_eq!(completed.session.status, SessionStatus::Idle);
    assert_eq!(completed.prompts[0].status, PromptStatus::Delivered);
    assert_eq!(completed.turns[0].status, TurnStatus::Completed);
    let agent_message = completed
        .messages
        .iter()
        .find(|message| message.role == MessageRole::Agent)
        .expect("Codex Agent Message is projected");
    assert_eq!(agent_message.status, MessageStatus::Completed);
    assert_eq!(agent_message.content, "Hello from Codex");
    assert!(!agent_message.content.contains("fixture diagnostic"));
    assert_eq!(completed.activities.len(), 4);
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
        panic!("Codex command must project as command Activity");
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(command, "cargo test --test codex_integration");
    assert_eq!(cwd.as_deref(), Some(std::path::Path::new("/fixture/work")));
    assert_eq!(output, "running tests\nall green\n");
    assert_eq!(*exit_status, Some(0));
    assert_eq!(
        completed
            .transcript
            .iter()
            .filter(|item| matches!(item,
                TranscriptItem::Activity { activity_id } if activity_id == command_activity_id))
            .count(),
        1,
        "Codex command deltas must update one transcript row"
    );
    let Activity::FileChange {
        id: file_change_activity_id,
        status: file_change_status,
        changes,
        ..
    } = &completed.activities[1]
    else {
        panic!("Codex file changes must project as file-change Activity");
    };
    assert_eq!(*file_change_status, ActivityStatus::Completed);
    assert_eq!(changes, &streamed_file_changes);
    assert_eq!(
        completed
            .transcript
            .iter()
            .filter(|item| matches!(item,
                TranscriptItem::Activity { activity_id }
                    if activity_id == file_change_activity_id))
            .count(),
        1,
        "Codex file-change updates must update one transcript row"
    );
    // Each summary section Codex sent is a Reasoning Activity of its own, headed
    // by the title that section led with and timed over its own stretch of the Turn.
    for (index, expected_title, expected_content) in [
        (2, Some("Inspecting the harness"), "Reading the fixture."),
        (3, None, "Then the store."),
    ] {
        let Activity::Reasoning {
            id: reasoning_activity_id,
            status: reasoning_status,
            title,
            content,
            content_truncated,
            duration_ms,
            ..
        } = &completed.activities[index]
        else {
            panic!("Codex Reasoning must project as a Reasoning Activity");
        };
        assert_eq!(*reasoning_status, ActivityStatus::Completed);
        assert_eq!(title.as_deref(), expected_title);
        assert_eq!(content, expected_content);
        assert!(!content_truncated);
        assert!(duration_ms.is_some());
        assert_eq!(
            completed
                .transcript
                .iter()
                .filter(|item| matches!(item,
                    TranscriptItem::Activity { activity_id }
                        if activity_id == reasoning_activity_id))
                .count(),
            1,
            "Codex Reasoning deltas must update one transcript row"
        );
    }

    let persisted = serde_json::to_string(&completed).expect("encode completed Session");
    for excluded in [
        "private start patch",
        "private updated patch",
        "private final patch",
        "private raw chain of thought",
        "wrong item patch",
        "wrong reasoning content",
        "futureField",
        "opaque",
    ] {
        assert!(
            !persisted.contains(excluded),
            "Session transcript persisted excluded native content {excluded:?}"
        );
    }

    let requests = fixture.requests();
    let methods = requests
        .iter()
        .filter_map(|request| request.get("method").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert_eq!(
        methods,
        ["initialize", "initialized", "thread/start", "turn/start"]
    );
    assert_eq!(
        requests[0]["params"]["capabilities"]["experimentalApi"],
        false
    );
    assert_eq!(requests[1], serde_json::json!({ "method": "initialized" }));
    assert_eq!(
        requests[2]["params"]["cwd"],
        workspace.path().to_string_lossy().as_ref()
    );
    assert_eq!(requests[2]["params"]["approvalPolicy"], "never");
    assert_eq!(requests[2]["params"]["sandbox"], "danger-full-access");
    assert_eq!(requests[2]["params"]["ephemeral"], false);
    assert!(requests[2]["params"].get("model").is_none());
    assert_eq!(requests[3]["params"]["threadId"], "native-thread");
    assert_eq!(
        requests[3]["params"]["input"],
        serde_json::json!([{ "type": "text", "text": "Explain the native harness" }])
    );
    assert_eq!(requests[3]["params"]["model"], "gpt-fixture");
    assert_eq!(
        requests[3]["params"]["summary"], "auto",
        "with the Reasoning summary Setting unpinned a Turn asks for its \
         built-in default, because Codex only summarizes when asked to"
    );

    drop(feed);
    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn abrupt_restart_keeps_completed_turns_and_idle_coalesced_tail() {
    let state_root = tempfile::tempdir().expect("create isolated state root");
    let data_root = tempfile::tempdir().expect("create isolated data root");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let channel = "codex-abrupt-transcript-restart";
    let config = ServerConfig::new(state_root.path(), channel)
        .expect("configure isolated server")
        .with_data_dir(data_root.path());
    let descriptor_path = config.descriptor_path();

    let boundary_codex = durability_codex(COMPLETED_TURN_DURABILITY_EVENTS);
    let mut boundary_process = spawn_server_process(
        state_root.path(),
        data_root.path(),
        channel,
        boundary_codex.executable(),
    );
    let boundary_descriptor = wait_for_descriptor(&descriptor_path, None).await;
    let mut boundary_client = ManagedClient::connect(
        ManagedClientConfig::new(state_root.path(), channel)
            .expect("configure boundary client")
            .with_data_dir(data_root.path())
            .with_server_executable(env!("CARGO_BIN_EXE_suru")),
    )
    .await
    .expect("connect boundary client");
    receive_initial_state(&mut boundary_client).await;
    let boundary_created = boundary_client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Commit this Turn boundary".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create boundary Session");
    let boundary_snapshot = timeout(Duration::from_secs(3), async {
        loop {
            let snapshot = boundary_client
                .read_session(boundary_created.session.id)
                .await
                .expect("read boundary Session");
            if snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.status == TurnStatus::Completed)
            {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first Turn reaches its durable boundary");
    assert_eq!(
        boundary_snapshot
            .messages
            .last()
            .map(|message| message.content.as_str()),
        Some("boundary durable")
    );
    drop(boundary_client);
    kill_server_process(&mut boundary_process).await;

    let idle_codex = durability_codex(IDLE_FLUSH_DURABILITY_EVENTS);
    let mut idle_process = spawn_server_process(
        state_root.path(),
        data_root.path(),
        channel,
        idle_codex.executable(),
    );
    let idle_descriptor =
        wait_for_descriptor(&descriptor_path, Some(boundary_descriptor.instance_id)).await;
    let reopened_boundary = reqwest::Client::new()
        .get(format!(
            "{}/v1/sessions/{}",
            idle_descriptor.base_url, boundary_created.session.id
        ))
        .bearer_auth(&idle_descriptor.token)
        .send()
        .await
        .expect("read boundary Session after abrupt restart")
        .error_for_status()
        .expect("boundary Session remains readable")
        .json::<SessionSnapshot>()
        .await
        .expect("decode boundary Session after abrupt restart");
    assert_eq!(reopened_boundary, boundary_snapshot);

    let mut idle_client = ManagedClient::connect(
        ManagedClientConfig::new(state_root.path(), channel)
            .expect("configure idle client")
            .with_data_dir(data_root.path())
            .with_server_executable(env!("CARGO_BIN_EXE_suru")),
    )
    .await
    .expect("connect idle client");
    receive_initial_state(&mut idle_client).await;
    let idle_created = idle_client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Flush this streaming tail on idle".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create idle-flush Session");
    idle_codex.wait_until_ready().await;
    let idle_snapshot = timeout(Duration::from_secs(3), async {
        loop {
            let snapshot = idle_client
                .read_session(idle_created.session.id)
                .await
                .expect("read streaming Session");
            if snapshot
                .messages
                .last()
                .is_some_and(|message| message.content == "coalesced tail")
            {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both streaming deltas reach the in-memory Session");
    tokio::time::sleep(Duration::from_millis(250)).await;
    drop(idle_client);
    kill_server_process(&mut idle_process).await;

    let final_codex = durability_codex(COMPLETED_TURN_DURABILITY_EVENTS);
    let mut final_process = spawn_server_process(
        state_root.path(),
        data_root.path(),
        channel,
        final_codex.executable(),
    );
    let final_descriptor =
        wait_for_descriptor(&descriptor_path, Some(idle_descriptor.instance_id)).await;
    let final_client = reqwest::Client::new();
    let final_boundary = final_client
        .get(format!(
            "{}/v1/sessions/{}",
            final_descriptor.base_url, boundary_created.session.id
        ))
        .bearer_auth(&final_descriptor.token)
        .send()
        .await
        .expect("read completed Session after second abrupt restart")
        .error_for_status()
        .expect("completed Session remains readable")
        .json::<SessionSnapshot>()
        .await
        .expect("decode completed Session after second abrupt restart");
    let final_idle = final_client
        .get(format!(
            "{}/v1/sessions/{}",
            final_descriptor.base_url, idle_created.session.id
        ))
        .bearer_auth(&final_descriptor.token)
        .send()
        .await
        .expect("read idle-flushed Session after abrupt restart")
        .error_for_status()
        .expect("idle-flushed Session remains readable")
        .json::<SessionSnapshot>()
        .await
        .expect("decode idle-flushed Session after abrupt restart");
    assert_eq!(final_boundary, boundary_snapshot);
    assert_eq!(final_idle, idle_snapshot);
    assert_eq!(final_idle.turns[0].status, TurnStatus::Active);
    assert_eq!(
        final_idle
            .messages
            .last()
            .map(|message| message.content.as_str()),
        Some("coalesced tail")
    );

    request_server_shutdown(&final_descriptor, ShutdownReason::Manual).await;
    timeout(Duration::from_secs(3), final_process.wait())
        .await
        .expect("replacement server exits after graceful cleanup")
        .expect("reap replacement server");
}

#[tokio::test]
async fn reopened_session_resumes_its_persisted_codex_thread_after_a_server_restart() {
    let fixture = ScriptedCodex::new_multiprocess(PERSISTED_RESUME_CODEX);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let execution_directory = workspace.path().join("packages/nested agent directory");
    std::fs::create_dir_all(&execution_directory).unwrap();
    let execution_directory = std::fs::canonicalize(execution_directory).unwrap();
    let channel = "codex-persisted-resume-state";
    let config = ServerConfig::new(state_dir.path(), channel)
        .expect("configure original server")
        .with_data_dir(data_dir.path());

    let original = server::spawn_with_provider(
        config.clone(),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn original server");
    let mut original_client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), channel)
            .expect("configure original client")
            .with_data_dir(data_dir.path()),
    )
    .await
    .expect("connect original client");
    receive_initial_state(&mut original_client).await;
    let created = original_client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: execution_directory.clone(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Establish durable Codex context".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create original Session");
    timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = original_client
                .read_session(created.session.id)
                .await
                .expect("read original Session");
            if snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.status == TurnStatus::Completed)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("original Turn completes");
    drop(original_client);
    original.shutdown().await.expect("stop original server");

    let replacement =
        server::spawn_with_provider(config, Arc::new(CodexRuntime::new(fixture.executable())))
            .await
            .expect("spawn replacement server");
    let mut replacement_client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), channel)
            .expect("configure replacement client")
            .with_data_dir(data_dir.path()),
    )
    .await
    .expect("connect replacement client");
    receive_initial_state(&mut replacement_client).await;
    replacement_client
        .admit_prompt(
            created.session.id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Continue durable Codex context".to_owned(),
                    skill_invocations: Vec::new(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("admit Prompt to reopened Session");
    timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = replacement_client
                .read_session(created.session.id)
                .await
                .expect("read reopened Session");
            if snapshot.turns.len() == 2 && snapshot.turns[1].status == TurnStatus::Completed {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("continued Turn completes after restart");

    let requests = fixture.requests();
    assert_eq!(
        fixture.methods(),
        [
            "initialize",
            "initialized",
            "thread/start",
            "turn/start",
            "initialize",
            "initialized",
            "thread/resume",
            "turn/start"
        ]
    );
    let resume = requests
        .iter()
        .find(|request| request["method"] == "thread/resume")
        .expect("replacement server resumes the persisted Codex Thread");
    assert_eq!(resume["params"]["threadId"], "persisted-thread");
    for method in ["thread/start", "thread/resume"] {
        let request = requests
            .iter()
            .find(|request| request["method"] == method)
            .unwrap();
        assert_eq!(
            request["params"]["cwd"],
            execution_directory.to_string_lossy().as_ref()
        );
    }

    drop(replacement_client);
    replacement
        .shutdown()
        .await
        .expect("stop replacement server");
}

#[tokio::test]
async fn scripted_codex_projects_failed_and_interrupted_terminal_outcomes() {
    let failed = SCRIPTED_CODEX
        .replace(
            "\"cwd\":\"/fixture/work\",\"status\":\"completed\"",
            "\"cwd\":\"/fixture/work\",\"status\":\"failed\"",
        )
        .replace("\"exitCode\":0", "\"exitCode\":17")
        .replace(
            "\"status\":\"completed\",\"items\":[]",
            "\"status\":\"failed\",\"error\":{\"message\":\"fixture Turn failed\"},\"items\":[]",
        );
    run_terminal_fixture(
        &failed,
        "codex-scripted-failed",
        TurnStatus::Failed,
        Some("fixture Turn failed"),
        ActivityStatus::Failed,
        Some(17),
    )
    .await;

    let interrupted = SCRIPTED_CODEX.replace(
        "\"status\":\"completed\",\"items\":[]",
        "\"status\":\"interrupted\",\"items\":[]",
    );
    run_terminal_fixture(
        &interrupted,
        "codex-scripted-interrupted",
        TurnStatus::Interrupted,
        None,
        ActivityStatus::Completed,
        Some(0),
    )
    .await;
}

#[tokio::test]
async fn scripted_codex_uses_completed_agent_text_when_no_deltas_arrive() {
    let without_deltas = SCRIPTED_CODEX
        .replace(
            "      printf '%s\\n' '{\"method\":\"item/agentMessage/delta\",\"params\":{\"threadId\":\"native-thread\",\"turnId\":\"native-turn\",\"itemId\":\"native-message\",\"delta\":\"Hello\",\"futureField\":true}}'\n",
            "",
        )
        .replace(
            "      printf '%s\\n' '{\"method\":\"item/agentMessage/delta\",\"params\":{\"threadId\":\"native-thread\",\"turnId\":\"native-turn\",\"itemId\":\"native-message\",\"delta\":\" from Codex\"}}'\n",
            "",
        );
    run_terminal_fixture(
        &without_deltas,
        "codex-scripted-completed-text",
        TurnStatus::Completed,
        None,
        ActivityStatus::Completed,
        Some(0),
    )
    .await;
}

#[tokio::test]
async fn scripted_codex_uses_the_turns_final_message_when_item_completion_is_missing() {
    let final_message_fallback = SCRIPTED_CODEX
        .replace(
            "      printf '%s\\n' '{\"method\":\"item/completed\",\"params\":{\"threadId\":\"native-thread\",\"turnId\":\"native-turn\",\"item\":{\"type\":\"agentMessage\",\"id\":\"native-message\",\"text\":\"Hello from Codex\"},\"futureField\":true}}'\n",
            "",
        )
        .replace(
            "      printf '%s\\n' '{\"method\":\"turn/completed\",\"params\":{\"threadId\":\"native-thread\",\"turn\":{\"id\":\"native-turn\",\"status\":\"completed\",\"items\":[],\"futureField\":true},\"futureField\":true}}'",
            "      printf '%s\\n' '{\"method\":\"turn/completed\",\"params\":{\"threadId\":\"native-thread\",\"turn\":{\"id\":\"native-turn\",\"status\":\"completed\",\"items\":[{\"type\":\"agentMessage\",\"id\":\"native-message\",\"text\":\"Hello from Codex\"}],\"futureField\":true},\"futureField\":true}}'",
        );

    run_terminal_fixture(
        &final_message_fallback,
        "codex-scripted-final-message-fallback",
        TurnStatus::Completed,
        None,
        ActivityStatus::Completed,
        Some(0),
    )
    .await;
}

async fn run_terminal_fixture(
    script: &str,
    channel: &str,
    expected_status: TurnStatus,
    expected_error: Option<&str>,
    expected_command_status: ActivityStatus,
    expected_exit_status: Option<i32>,
) {
    let fixture = ScriptedCodex::new(script);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), channel).expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), channel).expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Reach the requested terminal state".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session");
    let mut feed = client
        .subscribe_session(created.session.id)
        .await
        .expect("subscribe to Session SSE");
    feed.next()
        .await
        .expect("Session feed remains open")
        .expect("Session snapshot is valid");
    fixture.wait_for_method("turn/start").await;
    fixture.release();

    let completed = timeout(Duration::from_secs(2), async {
        loop {
            feed.next()
                .await
                .expect("Session feed remains open")
                .expect("Session update is valid");
            let snapshot = client
                .read_session(created.session.id)
                .await
                .expect("read Session");
            if snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.status == expected_status)
            {
                return snapshot;
            }
        }
    })
    .await
    .expect("native terminal outcome reaches Session SSE");

    assert_eq!(completed.session.status, SessionStatus::Idle);
    assert_eq!(completed.turns[0].status, expected_status);
    assert!(
        completed
            .activities
            .iter()
            .any(|activity| matches!(activity,
        Activity::Command {
            status,
            exit_status,
            ..
        } if *status == expected_command_status && *exit_status == expected_exit_status))
    );
    assert_eq!(
        completed.messages.last().map(|message| message.status),
        Some(MessageStatus::Completed)
    );
    assert_eq!(
        completed
            .messages
            .last()
            .map(|message| message.content.as_str()),
        Some("Hello from Codex")
    );
    match expected_error {
        Some(expected_error) => assert!(
            completed
                .activities
                .iter()
                .any(|activity| matches!(activity,
                    Activity::Error { text, .. } if text.contains(expected_error)))
        ),
        None => assert!(
            !completed
                .activities
                .iter()
                .any(|activity| matches!(activity, Activity::Error { .. })),
            "a successful or interrupted Turn must not add an Error Activity"
        ),
    }

    drop(feed);
    drop(client);
    server.shutdown().await.expect("shut down server");
}

fn spawn_server_process(
    state_root: &std::path::Path,
    data_root: &std::path::Path,
    channel: &str,
    codex: &std::path::Path,
) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_suru"));
    command
        .arg("__server")
        .arg("--state-dir")
        .arg(state_root)
        .arg("--data-dir")
        .arg(data_root)
        .arg("--channel")
        .arg(channel)
        .env("SURU_CODEX_PATH", codex)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command.spawn().expect("spawn isolated server process")
}

async fn wait_for_descriptor(
    path: &std::path::Path,
    previous: Option<uuid::Uuid>,
) -> RuntimeDescriptor {
    timeout(Duration::from_secs(3), async {
        loop {
            let descriptor = std::fs::File::open(path)
                .ok()
                .and_then(|file| serde_json::from_reader::<_, RuntimeDescriptor>(file).ok());
            if let Some(descriptor) = descriptor
                && previous.is_none_or(|instance| instance != descriptor.instance_id)
            {
                return descriptor;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("server publishes a fresh runtime descriptor")
}

async fn kill_server_process(process: &mut Child) {
    process.start_kill().expect("kill server process abruptly");
    timeout(Duration::from_secs(3), process.wait())
        .await
        .expect("killed server process exits")
        .expect("reap killed server process");
}

fn durability_codex(turn_events: &str) -> ScriptedCodex {
    ScriptedCodex::new(&format!(
        "{DURABILITY_CODEX_PREFIX}{turn_events}{DURABILITY_CODEX_SUFFIX}"
    ))
}
