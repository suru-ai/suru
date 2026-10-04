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
        CreateSessionRequest, InitialPrompt, MessageRole, PromptId, RuntimeDescriptor, SessionId,
        SessionSnapshot, SessionStatus, ShutdownReason, TurnStatus,
    },
    provider::CodexRuntime,
    server::{self, ServerConfig, ServerTimings},
};
use tokio::time::{Duration, timeout};

const COOPERATIVE_SHUTDOWN: &str = r#"#!/bin/sh
printf '%s\n' "$$" > "$CODEX_FIXTURE_PID"
sleep 30 &
printf '%s\n' "$!" > "$CODEX_FIXTURE_CHILD_PID"
trap 'printf exited > "$CODEX_FIXTURE_EXITED"' EXIT

while IFS= read -r line; do
  append_line "$CODEX_FIXTURE_LOG" "$line"
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
  append_line "$CODEX_FIXTURE_LOG" "$line"
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
  append_line "$CODEX_FIXTURE_LOG" "$line"
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

/// A Codex working on a Turn that leaves a long-lived descendant behind it, so
/// a Server that dies without stopping its Providers leaves something orphaned
/// for the test to find: the descendant outlives the shell, which ends on its
/// own once the Server's end of its stdin closes.
///
/// It starts that descendant only while the test's release stands, and checks
/// only once its PID is recorded. A test cleaning up withdraws the release
/// before it reads the PID, so a Codex launched as the test fails either is
/// found by that read or finds the release gone and ends without starting
/// anything.
const SIGNALLED_SHUTDOWN: &str = r#"#!/bin/sh
printf '%s\n' "$$" > "$CODEX_FIXTURE_PID"
[ -e "$CODEX_FIXTURE_RELEASE" ] || exit 0
sleep 600 &
printf '%s\n' "$!" > "$CODEX_FIXTURE_CHILD_PID"

while IFS= read -r line; do
  append_line "$CODEX_FIXTURE_LOG" "$line"
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
      printf ready > "$CODEX_FIXTURE_READY"
      ;;
    *'"method":"turn/interrupt"'*)
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"interrupted","items":[]}}}'
      printf '%s\n' '{"id":5,"result":{}}'
      ;;
  esac
done
"#;

/// A Codex that heeds nothing a stopping Server says: it never answers the
/// interrupt of its Turn, and once its stdin closes it runs on regardless,
/// beside a descendant it started. Only being killed, with its whole process
/// group, ends either before the test lets them go.
///
/// Every launch records its own PID and its descendant's on lists, so a
/// Codex launched more than once is checked in full. Each runs only while the
/// test's release stands, and ends on its own once the release is withdrawn,
/// so nothing outlives the test, whatever it failed to take down.
const STUBBORN_SHUTDOWN: &str = r#"#!/bin/sh
printf '%s\n' "$$" >> "$CODEX_FIXTURE_PID-all"
printf '%s\n' "$$" > "$CODEX_FIXTURE_PID"
[ -e "$CODEX_FIXTURE_RELEASE" ] || exit 0
( while [ -e "$CODEX_FIXTURE_RELEASE" ]; do sleep 0.01; done ) &
printf '%s\n' "$!" >> "$CODEX_FIXTURE_CHILD_PID-all"

while IFS= read -r line; do
  append_line "$CODEX_FIXTURE_LOG" "$line"
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
      printf ready > "$CODEX_FIXTURE_READY"
      ;;
  esac
done
while [ -e "$CODEX_FIXTURE_RELEASE" ]; do sleep 0.01; done
"#;

const PENDING_INITIALIZE_SHUTDOWN: &str = r#"#!/bin/sh
printf '%s\n' "$$" > "$CODEX_FIXTURE_PID"
trap 'printf exited > "$CODEX_FIXTURE_EXITED"' EXIT

while IFS= read -r line; do
  append_line "$CODEX_FIXTURE_LOG" "$line"
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
            session_id: None,
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
            session_id: None,
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
            session_id: None,
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

/// The Server waits on a stopping Provider Session, and then on its runtime,
/// only as long as its Provider stop timeout — here far shorter than the
/// exit grace Codex is given. A Codex that ignores both the interrupt and its
/// stdin closing is still within that grace when the waits end, and is taken
/// down then, with everything it started, before the Server's shutdown
/// returns: nothing is left for its grace to end later, or never, should the
/// process end first.
#[tokio::test]
async fn server_shutdown_takes_down_a_codex_that_ignores_being_stopped_with_everything_it_started()
{
    /// Longer than the test waits on anything: only the Server's own stop
    /// timeout can be what ends Codex.
    const NEVER_WAITED_OUT: Duration = Duration::from_secs(600);
    /// How long a killed process may take to be gone. Generous, since it
    /// bounds only a failure: a killed process is gone in milliseconds.
    const ENDING_DEADLINE: Duration = Duration::from_secs(5);

    let fixture = ScriptedCodex::new(STUBBORN_SHUTDOWN);
    fixture.release();
    let _withdrawn = WithdrawReleaseOnDrop(&fixture);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider_and_timings(
        ServerConfig::new(state_dir.path(), "codex-stubborn-shutdown").expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable()).with_process_exit_grace(NEVER_WAITED_OUT)),
        ServerTimings::default().with_provider_stop_timeout(Duration::from_millis(50)),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-stubborn-shutdown")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    client
        .create_session(CreateSessionRequest {
            session_id: None,
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Ignore every request to stop".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create Session");
    fixture.wait_until_ready().await;
    drop(client);

    timeout(PROGRESS_DEADLINE, server.shutdown())
        .await
        .expect("the stop timeout bounds server shutdown")
        .expect("shut down server");

    let launched = recorded_pids(&fixture.pid_file().with_file_name("pid-all"));
    let started = recorded_pids(&fixture.pid_file().with_file_name("child-pid-all"));
    assert!(!launched.is_empty() && !started.is_empty());
    for pid in launched.into_iter().chain(started) {
        let ended = timeout(ENDING_DEADLINE, async {
            while unsafe { libc::kill(pid, 0) } == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            ended.is_ok(),
            "process {pid} of Codex's group outlived the Server's shutdown"
        );
    }
}

/// The PIDs a scripted Codex appended to the list at `path`.
fn recorded_pids(path: &std::path::Path) -> Vec<libc::pid_t> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|pid| pid.trim().parse().expect("recorded PID is numeric"))
        .collect()
}

/// Withdraws a scripted Codex's release as the test ends, however it ends,
/// so a Codex that reads its release as leave to run on stops.
struct WithdrawReleaseOnDrop<'a>(&'a ScriptedCodex);

impl Drop for WithdrawReleaseOnDrop<'_> {
    fn drop(&mut self) {
        self.0.withdraw_release();
    }
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
            session_id: None,
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

#[tokio::test]
async fn sigterm_stops_a_server_process_and_its_codex_gracefully() {
    signal_stops_a_server_process_and_its_codex_gracefully(libc::SIGTERM, "codex-sigterm").await;
}

#[tokio::test]
async fn sighup_stops_a_server_process_and_its_codex_gracefully() {
    signal_stops_a_server_process_and_its_codex_gracefully(libc::SIGHUP, "codex-sighup").await;
}

#[tokio::test]
async fn sigint_stops_a_server_process_and_its_codex_gracefully() {
    signal_stops_a_server_process_and_its_codex_gracefully(libc::SIGINT, "codex-sigint").await;
}

/// Sends `signal` to a `suru __server` process whose Codex is working on a
/// Turn, and holds it to the shutdown a client's stop request starts: the
/// process exits successfully rather than by the signal, having asked Codex to
/// interrupt its Turn and then taken Codex and everything it started down.
/// Left to the signal's default action the Server would die at once, and the
/// Codex descendant, in a process group of its own, would live on as an
/// orphan.
async fn signal_stops_a_server_process_and_its_codex_gracefully(
    signal: libc::c_int,
    channel: &str,
) {
    let fixture = ScriptedCodex::new(SIGNALLED_SHUTDOWN);
    fixture.release();
    let state_root = tempfile::tempdir().expect("create isolated state root");
    let data_root = tempfile::tempdir().expect("create isolated data root");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let absent_provider = state_root.path().join("absent-provider");
    let config = ServerConfig::new(state_root.path(), channel)
        .expect("configure isolated server")
        .with_data_dir(data_root.path());
    let mut server = SignalledServer {
        process: std::process::Command::new(env!("CARGO_BIN_EXE_suru"))
            .arg("__server")
            .arg("--state-dir")
            .arg(state_root.path())
            .arg("--data-dir")
            .arg(data_root.path())
            .arg("--channel")
            .arg(channel)
            .env("SURU_CODEX_PATH", fixture.executable())
            // The other Providers are pointed at nothing, so the Server never
            // launches a real Copilot or Claude installed on this machine.
            .env("SURU_COPILOT_PATH", &absent_provider)
            .env("SURU_CLAUDE_PATH", &absent_provider)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn isolated server process"),
        codex: &fixture,
    };
    let descriptor = timeout(PROGRESS_DEADLINE, async {
        loop {
            if let Some(descriptor) = std::fs::File::open(config.descriptor_path())
                .ok()
                .and_then(|file| serde_json::from_reader::<_, RuntimeDescriptor>(file).ok())
            {
                return descriptor;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("server process publishes its runtime descriptor");

    // The Session is begun over plain HTTP against this one Server rather
    // than through a managed client, which would launch a Server of its own —
    // with none of the environment above, so with real Providers, and beyond
    // this test's cleanup — should this one go away first.
    reqwest::Client::new()
        .post(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .json(&CreateSessionRequest {
            session_id: None,
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Keep working until the Server is signalled".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        // Bounded as a managed client's readiness was, so a Server that
        // published its descriptor but never serves fails the test, and the
        // guard above cleans up, rather than hanging it.
        .timeout(PROGRESS_DEADLINE)
        .send()
        .await
        .expect("request a Session")
        .error_for_status()
        .expect("create Session");
    fixture.wait_until_ready().await;

    let server_pid = libc::pid_t::try_from(server.process.id()).expect("server PID fits a pid_t");
    assert_eq!(
        unsafe { libc::kill(server_pid, signal) },
        0,
        "signal the server process"
    );
    let status = timeout(PROGRESS_DEADLINE, async {
        loop {
            if let Some(status) = server.process.try_wait().expect("poll server process") {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("signalled server process exits");
    assert!(
        status.success(),
        "a signalled Server shuts down gracefully rather than dying by the signal: {status}"
    );
    assert!(
        fixture
            .methods()
            .iter()
            .any(|method| method == "turn/interrupt"),
        "the signalled Server asks Codex to interrupt its Turn before stopping it"
    );
    assert_process_exited(fixture.pid()).await;
    assert_process_exited(fixture.child_pid()).await;
}

/// A `suru __server` process a test launched, and the Codex it may have
/// started. Dropped before the Server could take Codex down — the test failed
/// — it kills both, Codex by its process group so the descendants it started
/// go with it, rather than leaving them running past the test. The Codex
/// release is withdrawn before its PID is read, so a Codex that has not yet
/// recorded one ends on its own rather than outliving the cleanup.
struct SignalledServer<'a> {
    process: std::process::Child,
    codex: &'a ScriptedCodex,
}

impl Drop for SignalledServer<'_> {
    fn drop(&mut self) {
        if matches!(self.process.try_wait(), Ok(None)) {
            let _ = self.process.kill();
            let _ = self.process.wait();
        }
        self.codex.withdraw_release();
        let codex_group = std::fs::read_to_string(self.codex.pid_file())
            .ok()
            .and_then(|pid| pid.trim().parse::<libc::pid_t>().ok())
            .filter(|pid| *pid > 0);
        if let Some(group) = codex_group
            && unsafe { libc::killpg(group, 0) } == 0
        {
            unsafe { libc::killpg(group, libc::SIGKILL) };
        }
    }
}
