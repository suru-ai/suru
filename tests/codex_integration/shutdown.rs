//! Server shutdown against cooperative, pending, and unresponsive Codex processes.

use crate::server_support::PROGRESS_DEADLINE;
use crate::{
    server_support::request_server_shutdown,
    support::{ScriptedCodex, assert_process_exited, receive_initial_state},
};
use serde_json::Value;
use std::sync::Arc;
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig},
    protocol::{
        CreateSessionRequest, InitialPrompt, MessageRole, PromptId, SessionId, SessionSnapshot,
        SessionStatus, ShutdownReason, TurnStatus,
    },
    provider::CodexRuntime,
    server::{self, ServerConfig},
};
use tokio::time::{Duration, timeout};

const COOPERATIVE_SHUTDOWN: &str = r#"#!/bin/sh
printf '%s\n' "$$" > "$CODEX_FIXTURE_PID"
sleep 30 &
printf '%s\n' "$!" > "$CODEX_FIXTURE_CHILD_PID"
trap 'printf exited > "$CODEX_FIXTURE_EXITED"' EXIT

while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"config/read"'*)
      printf '%s\n' '{"id":2,"result":{"config":{},"origins":{}}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":3,"result":{"thread":{"id":"native-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":4,"result":{"turn":{"id":"native-turn"}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"agentMessage","id":"working-message","text":""}}}'
      printf ready > "$CODEX_FIXTURE_READY"
      ;;
    *'"method":"turn/interrupt"'*)
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"agentMessage","id":"late-message","text":""}}}'
      printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"late-message","delta":"late shutdown output"}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"agentMessage","id":"late-message","text":"late shutdown output"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"interrupted","items":[]}}}'
      printf '%s\n' '{"id":5,"result":{}}'
      ;;
  esac
done
"#;

const PENDING_TURN_START_SHUTDOWN: &str = r#"#!/bin/sh
printf '%s\n' "$$" > "$CODEX_FIXTURE_PID"
trap 'printf exited > "$CODEX_FIXTURE_EXITED"' EXIT

while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"config/read"'*)
      printf '%s\n' '{"id":2,"result":{"config":{},"origins":{}}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":3,"result":{"thread":{"id":"native-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf ready > "$CODEX_FIXTURE_READY"
      (
        while [ ! -e "$CODEX_FIXTURE_RELEASE" ]; do
          sleep 0.01
        done
        printf '%s\n' '{"id":4,"result":{"turn":{"id":"native-turn"}}}'
      ) &
      ;;
    *'"method":"turn/interrupt"'*)
      printf '%s\n' '{"id":5,"result":{}}'
      ;;
  esac
done
"#;

const UNRESPONSIVE_SHUTDOWN: &str = r#"#!/bin/sh
printf '%s\n' "$$" > "$CODEX_FIXTURE_PID"
sleep 30 &
printf '%s\n' "$!" > "$CODEX_FIXTURE_CHILD_PID"

while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"config/read"'*)
      printf '%s\n' '{"id":2,"result":{"config":{},"origins":{}}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":3,"result":{"thread":{"id":"native-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":4,"result":{"turn":{"id":"native-turn"}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"agentMessage","id":"working-message","text":""}}}'
      printf ready > "$CODEX_FIXTURE_READY"
      ;;
    *'"method":"turn/interrupt"'*)
      while :; do :; done
      ;;
  esac
done
"#;

const PENDING_INITIALIZE_SHUTDOWN: &str = r#"#!/bin/sh
printf '%s\n' "$$" > "$CODEX_FIXTURE_PID"
trap 'printf exited > "$CODEX_FIXTURE_EXITED"' EXIT

while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
done
"#;

#[tokio::test]
async fn server_shutdown_interrupts_active_codex_and_allows_cooperative_exit() {
    let fixture = ScriptedCodex::new(COOPERATIVE_SHUTDOWN);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-cooperative-shutdown")
            .expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-cooperative-shutdown")
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
                text: "Keep working until Suru shuts down".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create Session");
    fixture.wait_for_method("turn/start").await;
    fixture.wait_until_ready().await;
    let before_shutdown = wait_for_agent_output(&client, created.session.id).await;
    assert_eq!(before_shutdown.turns[0].status, TurnStatus::Active);

    let response = request_server_shutdown(&descriptor, ShutdownReason::Manual).await;
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    fixture.wait_for_exit().await;

    let after_shutdown = reqwest::Client::new()
        .get(format!(
            "{}/v1/sessions/{}",
            descriptor.base_url, created.session.id
        ))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("read Session during graceful HTTP shutdown")
        .error_for_status()
        .expect("Session remains readable during graceful HTTP shutdown")
        .json::<SessionSnapshot>()
        .await
        .expect("decode Session after Provider shutdown");
    // The stop settles the Turn it cut off before the storage writer closes,
    // and the settlement is readable through the graceful HTTP window
    // (ADR 0029).
    assert!(
        after_shutdown.revision > before_shutdown.revision,
        "the stop's settlement is the one change after shutdown begins"
    );
    assert!(
        after_shutdown.turns[0].status.is_terminal(),
        "the Turn Suru interrupted for shutdown is settled, not left running"
    );
    assert!(after_shutdown.turns[0].settled_at.is_some());
    assert_eq!(after_shutdown.session.status, SessionStatus::Idle);

    let methods = fixture
        .requests()
        .into_iter()
        .filter_map(|request| {
            request
                .get("method")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        methods,
        [
            "initialize",
            "initialized",
            "config/read",
            "thread/start",
            "turn/start",
            "turn/interrupt"
        ]
    );
    assert_process_exited(fixture.pid()).await;
    assert_process_exited(fixture.child_pid()).await;

    drop(client);
    timeout(PROGRESS_DEADLINE, server.shutdown())
        .await
        .expect("repeated shutdown request remains bounded")
        .expect("shut down server");
}

#[tokio::test]
async fn server_shutdown_interrupts_a_turn_whose_start_response_is_pending() {
    let fixture = ScriptedCodex::new(PENDING_TURN_START_SHUTDOWN);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-pending-turn-shutdown")
            .expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let descriptor = server.descriptor().clone();
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-pending-turn-shutdown")
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
                text: "Shut down while Codex accepts this Turn".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create Session");
    fixture.wait_until_ready().await;

    let response = request_server_shutdown(&descriptor, ShutdownReason::Manual).await;
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    fixture.release();
    timeout(PROGRESS_DEADLINE, server.shutdown())
        .await
        .expect("pending Turn startup keeps shutdown bounded")
        .expect("shut down server");

    assert!(
        fixture
            .requests()
            .iter()
            .any(|request| request.get("method").and_then(Value::as_str) == Some("turn/interrupt")),
        "shutdown waits for the accepted native Turn ID and interrupts it before closing transport"
    );
    assert_process_exited(fixture.pid()).await;
}

#[tokio::test]
async fn server_shutdown_releases_pending_rpc_and_forces_an_unresponsive_codex_to_exit() {
    let fixture = ScriptedCodex::new(UNRESPONSIVE_SHUTDOWN);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-forced-shutdown").expect("configure server"),
        Arc::new(
            CodexRuntime::new(fixture.executable())
                .with_process_exit_grace(Duration::from_millis(50)),
        ),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-forced-shutdown")
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
                text: "Remain unresponsive during shutdown".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create Session");
    fixture.wait_for_method("turn/start").await;
    fixture.wait_until_ready().await;
    wait_for_agent_output(&client, created.session.id).await;

    timeout(PROGRESS_DEADLINE, server.shutdown())
        .await
        .expect("forced Provider termination bounds server shutdown")
        .expect("shut down server");
    assert!(
        fixture
            .requests()
            .iter()
            .any(|request| request.get("method").and_then(Value::as_str) == Some("turn/interrupt")),
        "shutdown asks active Codex work to interrupt before forcing termination"
    );
    assert_process_exited(fixture.pid()).await;
    assert_process_exited(fixture.child_pid()).await;
}

#[tokio::test]
async fn server_shutdown_closes_transport_with_a_startup_request_pending() {
    let fixture = ScriptedCodex::new(PENDING_INITIALIZE_SHUTDOWN);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-pending-startup-shutdown")
            .expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-pending-startup-shutdown")
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
                text: "Shut down during Codex initialization".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create Session while Provider starts");
    fixture.wait_for_method("initialize").await;

    timeout(PROGRESS_DEADLINE, server.shutdown())
        .await
        .expect("pending startup RPC does not delay server shutdown")
        .expect("shut down server");
    assert!(
        fixture.exited.exists(),
        "server shutdown waits for cooperative Codex startup exit"
    );
    assert_process_exited(fixture.pid()).await;
}

async fn wait_for_agent_output(client: &ManagedClient, session_id: SessionId) -> SessionSnapshot {
    timeout(PROGRESS_DEADLINE, async {
        loop {
            let snapshot = client
                .read_session(session_id)
                .await
                .expect("read Session while scripted Codex starts");
            if snapshot
                .messages
                .iter()
                .any(|message| message.role == MessageRole::Agent)
            {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("scripted Codex Agent output reaches the Session")
}
