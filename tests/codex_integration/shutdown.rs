//! Server shutdown against cooperative, pending, and unresponsive Codex processes.

use crate::server_support::PROGRESS_DEADLINE;
use crate::{
    server_support::{detached_servers::DetachedServers, request_server_shutdown},
    support::{ScriptedCodex, assert_process_exited, receive_initial_state},
};
use serde_json::Value;
use std::os::unix::fs::FileTypeExt;
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
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::{Duration, timeout},
};

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
        wait_for "$CODEX_FIXTURE_RELEASE"
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
      idle_forever
      ;;
  esac
done
"#;

/// A Codex working on a Turn that leaves a long-lived descendant behind it, so
/// a Server that dies without stopping its Providers leaves something orphaned
/// for the test to find: the descendant outlives the shell, which ends on its
/// own once the Server's end of its stdin closes.
///
/// It starts that descendant only while the test's release stands, and the
/// descendant runs only while the release stands and its test runs too. A test
/// cleaning up withdraws the release, so whatever the Server failed to take
/// down ends on its own — even should the cleanup fail to find Codex's process
/// group. The descendant does not end with the shell, which would hide a
/// Server leaving it running.
const SIGNALLED_SHUTDOWN: &str = r#"#!/bin/sh
printf '%s\n' "$$" > "$CODEX_FIXTURE_PID"
[ -e "$CODEX_FIXTURE_RELEASE" ] || exit 0
( while [ -e "$CODEX_FIXTURE_RELEASE" ] && test_running; do sleep 0.01; done ) &
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
/// test's release stands and the test runs, and ends on its own once the
/// release is withdrawn, so nothing outlives the test, whatever it failed to
/// take down.
const STUBBORN_SHUTDOWN: &str = r#"#!/bin/sh
printf '%s\n' "$$" >> "$CODEX_FIXTURE_PID-all"
printf '%s\n' "$$" > "$CODEX_FIXTURE_PID"
[ -e "$CODEX_FIXTURE_RELEASE" ] || exit 0
( while [ -e "$CODEX_FIXTURE_RELEASE" ] && test_running; do sleep 0.01; done ) &
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
while [ -e "$CODEX_FIXTURE_RELEASE" ] && test_running; do sleep 0.01; done
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
    let roots = ServerRoots::new();
    let (mut server, _) = serve_working_codex(&fixture, channel, &roots, |_| {}).await;

    let server_pid = libc::pid_t::try_from(server.process.id()).expect("server PID fits a pid_t");
    assert_eq!(
        unsafe { libc::kill(server_pid, signal) },
        0,
        "signal the server process"
    );
    let status = server.exit().await;
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

/// A `suru __server` process whose state directory is removed from under it —
/// a test's temporary directory deleted while its Server runs on, say — stops
/// the way a client's stop request stops it, rather than running on for good:
/// it exits successfully, having asked Codex to interrupt its Turn and taken
/// Codex and everything it started down, and makes nothing in the
/// directory's place.
#[tokio::test]
async fn removing_its_state_directory_stops_a_server_process_and_its_codex_gracefully() {
    let fixture = ScriptedCodex::new(SIGNALLED_SHUTDOWN);
    fixture.release();
    let roots = ServerRoots::new();
    let (mut server, _) =
        serve_working_codex(&fixture, "codex-state-removed", &roots, |_| {}).await;

    let state_dir = roots.state.state_dir().to_owned();
    std::fs::remove_dir_all(&state_dir).expect("remove the server's state directory");
    let status = server.exit().await;
    assert!(
        status.success(),
        "a Server whose state directory was removed stops gracefully: {status}"
    );
    assert!(
        fixture
            .methods()
            .iter()
            .any(|method| method == "turn/interrupt"),
        "the stopping Server asks Codex to interrupt its Turn before stopping it"
    );
    assert_process_exited(fixture.pid()).await;
    assert_process_exited(fixture.child_pid()).await;
    assert!(
        !state_dir.exists(),
        "the stopped Server made its state directory again"
    );
}

/// A `suru __server` process killed outright runs no shutdown and drops
/// nothing, yet the Codex it was running goes with it, along with everything
/// Codex started — here a Codex that heeds neither its stdin closing nor
/// anything else short of being killed, beside a descendant that heeds
/// nothing either. Each was in a process group of its own, which no signal
/// sent to the Server reaches.
#[tokio::test]
async fn sigkill_of_a_server_process_takes_down_its_codex_with_everything_it_started() {
    /// How long Codex and its descendant may take to be gone once the Server
    /// is. Generous, since it bounds only a failure: they are gone in
    /// milliseconds, and left running they are never gone.
    const ENDING_DEADLINE: Duration = Duration::from_secs(5);

    let fixture = ScriptedCodex::new(STUBBORN_SHUTDOWN);
    fixture.release();
    let _withdrawn = WithdrawReleaseOnDrop(&fixture);
    let roots = ServerRoots::new();
    let (mut server, _) = serve_working_codex(&fixture, "codex-sigkill", &roots, |_| {}).await;

    let server_pid = libc::pid_t::try_from(server.process.id()).expect("server PID fits a pid_t");
    assert_eq!(
        unsafe { libc::kill(server_pid, libc::SIGKILL) },
        0,
        "kill the server process"
    );
    let status = server.exit().await;
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&status),
        Some(libc::SIGKILL),
        "the Server died at once, running nothing"
    );

    // Read once the Server is dead, so no Codex is launched after.
    let launched = recorded_pids(&fixture.pid_file().with_file_name("pid-all"));
    let started = recorded_pids(&fixture.pid_file().with_file_name("child-pid-all"));
    assert!(!launched.is_empty() && !started.is_empty());

    for pid in launched.into_iter().chain(started) {
        // Orphaned as the Server died, they are reaped by whichever process
        // adopted them.
        let ended = timeout(ENDING_DEADLINE, async {
            while unsafe { libc::kill(pid, 0) } == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            ended.is_ok(),
            "process {pid} of Codex's process tree outlived the Server killed under it"
        );
    }
}

/// A Server whose stop is held up — here by an Attachment upload whose client
/// never sends its body, and a Codex that heeds no request to stop — is cut
/// short at its deadline rather than waiting on either for good: the upload's
/// connection is cut, Codex and everything it started are killed, and the
/// stop finishes, reporting that it was cut short. Neither the drain nor the
/// Provider stop timeout, far longer than the test waits, can be what ends
/// them.
#[tokio::test]
async fn a_stop_held_up_by_an_upload_and_a_heedless_codex_is_cut_short_at_its_deadline() {
    /// Longer than the test waits on anything: only the stop's deadline can
    /// be what ends Codex or the upload.
    const NEVER_WAITED_OUT: Duration = Duration::from_secs(600);
    /// How long a killed process, or a cut connection, may take to be gone.
    /// Generous, since it bounds only a failure.
    const ENDING_DEADLINE: Duration = Duration::from_secs(5);
    const SHUTDOWN_DEADLINE: Duration = Duration::from_millis(300);

    let fixture = ScriptedCodex::new(STUBBORN_SHUTDOWN);
    fixture.release();
    let _withdrawn = WithdrawReleaseOnDrop(&fixture);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider_and_timings(
        ServerConfig::new(state_dir.path(), "codex-stop-deadline").expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable()).with_process_exit_grace(NEVER_WAITED_OUT)),
        ServerTimings::default()
            .with_provider_stop_timeout(NEVER_WAITED_OUT)
            .with_shutdown_deadline(SHUTDOWN_DEADLINE, Duration::from_millis(300)),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-stop-deadline")
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
    let mut upload = begin_stuck_upload(server.descriptor()).await;

    let stopping = tokio::time::Instant::now();
    let stopped = timeout(PROGRESS_DEADLINE, server.shutdown())
        .await
        .expect("the stop's deadline bounds a stop held up by an upload and a heedless Codex");
    assert!(
        stopping.elapsed() >= SHUTDOWN_DEADLINE,
        "the stop was given its deadline before it was cut short"
    );
    let error = stopped.expect_err("a stop cut short at its deadline reports it");
    assert!(
        format!("{error:#}").contains("deadline"),
        "the stop reports reaching its deadline: {error:#}"
    );

    let mut answer = [0_u8; 64];
    let cut = timeout(ENDING_DEADLINE, upload.read(&mut answer))
        .await
        .expect("the upload's connection is cut as the stop is cut short");
    assert!(
        matches!(cut, Ok(0) | Err(_)),
        "the upload was cut off rather than answered: {cut:?}"
    );
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
            "process {pid} of Codex's group outlived the stop cut short at its deadline"
        );
    }
}

/// A second signal arriving while a `suru __server` process is stopping —
/// held up by an Attachment upload that never finishes, under a deadline
/// longer than the test waits — ends the process at once, with the status
/// a shell reports for the second signal, rather than being absorbed while
/// the stop waits on the upload. Its Codex and what Codex started are gone,
/// and the election it held is let go, so a successor is elected in its
/// place.
#[tokio::test]
async fn a_second_signal_cuts_a_held_up_stop_short_and_ends_the_server_process() {
    let fixture = ScriptedCodex::new(SIGNALLED_SHUTDOWN);
    fixture.release();
    let roots = ServerRoots::new();
    let channel = "codex-second-signal";
    let (mut server, descriptor) = serve_working_codex(&fixture, channel, &roots, |command| {
        command.args(["--shutdown-deadline-ms", "600000"]);
    })
    .await;
    let _upload = begin_stuck_upload(&descriptor).await;

    let server_pid = libc::pid_t::try_from(server.process.id()).expect("server PID fits a pid_t");
    assert_eq!(
        unsafe { libc::kill(server_pid, libc::SIGTERM) },
        0,
        "signal the server process"
    );
    fixture.wait_for_method("turn/interrupt").await;
    assert!(
        matches!(server.process.try_wait(), Ok(None)),
        "the upload holds the stop the first signal began up"
    );
    assert_eq!(
        unsafe { libc::kill(server_pid, libc::SIGINT) },
        0,
        "signal the server process again"
    );
    let status = server.exit().await;
    assert_eq!(
        status.code(),
        Some(128 + libc::SIGINT),
        "a second signal ends the stopping Server with the status a shell reports for it"
    );
    assert_process_exited(fixture.pid()).await;
    assert_process_exited(fixture.child_pid()).await;

    let config = ServerConfig::new(roots.state.state_dir(), channel)
        .expect("configure the successor")
        .with_data_dir(roots.data.path());
    let successor = timeout(
        PROGRESS_DEADLINE,
        server::spawn_with_provider(config, Arc::new(CodexRuntime::new(fixture.executable()))),
    )
    .await
    .expect("the successor's election ends")
    .expect("a successor is elected once the cut-short Server is gone");
    successor.shutdown().await.expect("stop the successor");
}

/// A stop's deadline, overrun and cutoff margin, short enough that a test
/// sees a process cut off without waiting out the defaults.
const CUT_OFF_SOON: [&str; 6] = [
    "--shutdown-deadline-ms",
    "200",
    "--shutdown-overrun-ms",
    "200",
    "--shutdown-cutoff-margin-ms",
    "100",
];

/// A `suru __server` process whose runtime has one worker, held — a Turn
/// settling as the stop begins waits on storage that does not answer, its
/// database held locked by another connection, holding the worker and the
/// Session store with it — is still ended at its cutoff: the stop's deadline
/// and overrun, kept by tasks no worker is free to run, could never end it,
/// but the process's cutoff, kept apart from the runtime, does. Its Codex and
/// everything Codex started go with it.
#[tokio::test]
async fn a_server_process_whose_runtime_is_held_is_ended_at_its_cutoff() {
    held_server_process_is_ended_at_its_cutoff("codex-held-runtime", StopBy::Signal).await;
}

/// As [`a_server_process_whose_runtime_is_held_is_ended_at_its_cutoff`], but
/// stopped by a client's Manual stop alone, with no signal sent: the stop's
/// beginning arms the cutoff, as it is accepted and before any worker is
/// held, so the process is ended at its cutoff all the same.
#[tokio::test]
async fn a_server_process_stopped_by_a_client_while_its_runtime_is_held_is_ended_at_its_cutoff() {
    held_server_process_is_ended_at_its_cutoff("codex-held-runtime-client", StopBy::Client).await;
}

/// How a test stops a `suru __server` process.
enum StopBy {
    /// SIGTERM, which the process's cutoff hears for itself.
    Signal,
    /// A client's Manual stop request, which only the Server hears.
    Client,
}

/// Launches a `suru __server` process on a runtime of one worker, its Codex
/// working on a Turn, holds its database locked, stops it `by` a signal or a
/// client, and holds it to ending at its cutoff, well before storage would
/// answer, with Codex and everything Codex started gone.
async fn held_server_process_is_ended_at_its_cutoff(channel: &str, by: StopBy) {
    /// How long storage waits on a locked database before giving up — its
    /// busy timeout — and so the least the settling Turn holds the worker.
    const STORAGE_STALL: Duration = Duration::from_secs(5);

    let fixture = ScriptedCodex::new(SIGNALLED_SHUTDOWN);
    fixture.release();
    let roots = ServerRoots::new();
    let (mut server, descriptor) = serve_working_codex(&fixture, channel, &roots, |command| {
        command.args(CUT_OFF_SOON).env("TOKIO_WORKER_THREADS", "1");
    })
    .await;
    let database_path = ServerConfig::new(roots.state.state_dir(), channel)
        .expect("configure the server")
        .with_data_dir(roots.data.path())
        .data_dir()
        .join("suru.db");
    let mut database = {
        use diesel::{Connection, SqliteConnection, connection::SimpleConnection};
        let mut database = SqliteConnection::establish(
            database_path
                .to_str()
                .expect("the database path is valid UTF-8"),
        )
        .expect("open the server's database");
        database
            .batch_execute("PRAGMA busy_timeout = 5000; BEGIN EXCLUSIVE;")
            .expect("hold the database locked");
        database
    };

    let stopping = tokio::time::Instant::now();
    // The client's request is never answered once the runtime is held, so
    // it is not waited on: only the process's end is.
    let request = match by {
        StopBy::Signal => {
            let server_pid =
                libc::pid_t::try_from(server.process.id()).expect("server PID fits a pid_t");
            assert_eq!(
                unsafe { libc::kill(server_pid, libc::SIGTERM) },
                0,
                "signal the server process"
            );
            None
        }
        StopBy::Client => Some(tokio::spawn(
            reqwest::Client::new()
                .post(format!("{}/v1/server/stop", descriptor.base_url))
                .bearer_auth(&descriptor.token)
                .json(&suru::protocol::ServerShutdown {
                    instance_id: descriptor.identity.instance_id,
                    reason: ShutdownReason::Manual,
                })
                .send(),
        )),
    };
    let status = server.exit().await;
    let took = stopping.elapsed();
    if let Some(request) = request {
        request.abort();
    }
    diesel::connection::SimpleConnection::batch_execute(&mut database, "ROLLBACK;")
        .expect("let the database go");
    drop(database);
    assert_eq!(
        status.code(),
        Some(suru::server::CUT_OFF_EXIT_STATUS),
        "the held server process was ended at its cutoff: {status}"
    );
    assert!(
        took < STORAGE_STALL - Duration::from_secs(1),
        "the cutoff ended the process before storage answered: it took {took:?}"
    );
    assert_process_exited(fixture.pid()).await;
    assert_process_exited(fixture.child_pid()).await;
}

/// A `suru __server` process whose stderr does not take what it writes — a
/// pipe left full, as a Log on a mount that does not answer leaves it —
/// still ends once its stop is over: reporting that the stop was cut short
/// waits on stderr for good, and the process's cutoff ends it.
#[tokio::test]
async fn a_server_process_whose_stderr_does_not_answer_still_ends() {
    use std::os::fd::{FromRawFd, OwnedFd};

    let fixture = ScriptedCodex::new(SIGNALLED_SHUTDOWN);
    fixture.release();
    let roots = ServerRoots::new();
    let mut ends = [0; 2];
    assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0, "make a pipe");
    let (unread, stderr) =
        unsafe { (OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])) };
    fill_pipe(&stderr);
    let (mut server, descriptor) =
        serve_working_codex(&fixture, "codex-stalled-stderr", &roots, |command| {
            command
                .args(CUT_OFF_SOON)
                .stderr(std::process::Stdio::from(stderr));
        })
        .await;
    // Holds the stop up past its deadline, so it is reported as cut short.
    let _upload = begin_stuck_upload(&descriptor).await;

    let server_pid = libc::pid_t::try_from(server.process.id()).expect("server PID fits a pid_t");
    assert_eq!(
        unsafe { libc::kill(server_pid, libc::SIGTERM) },
        0,
        "signal the server process"
    );
    let status = server.exit().await;
    drop(unread);
    assert_eq!(
        status.code(),
        Some(suru::server::CUT_OFF_EXIT_STATUS),
        "the server process stuck reporting its stop was ended at its cutoff: {status}"
    );
    assert_process_exited(fixture.pid()).await;
    assert_process_exited(fixture.child_pid()).await;
}

/// Fills the pipe whose writing end is `pipe` until it takes no more, leaving
/// that end blocking, as it was, so the next write to it waits for good.
fn fill_pipe(pipe: &std::os::fd::OwnedFd) {
    use std::os::fd::AsRawFd;
    let fd = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    assert!(flags >= 0, "read the pipe's flags");
    assert_eq!(
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0,
        "stop the pipe blocking"
    );
    for chunk in [4096, 1] {
        let bytes = vec![b'x'; chunk];
        while unsafe { libc::write(fd, bytes.as_ptr().cast(), chunk) } > 0 {}
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EAGAIN),
            "the pipe fills"
        );
    }
    assert_eq!(
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags) },
        0,
        "leave the pipe blocking again"
    );
}

/// A FIFO put in place of a Server's runtime descriptor — which an ordinary
/// open waits on until something writes to it, as nothing will — holds up
/// neither the Server's stop nor the election it lets go: the stop finishes,
/// leaving what is not its descriptor alone, and a successor is elected and
/// publishes its own descriptor in the FIFO's place.
#[tokio::test(flavor = "multi_thread")]
async fn a_fifo_in_place_of_its_descriptor_holds_up_neither_a_stop_nor_its_successor() {
    let fixture = ScriptedCodex::new(PENDING_INITIALIZE_SHUTDOWN);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config =
        ServerConfig::new(state_dir.path(), "codex-fifo-descriptor").expect("configure server");
    let server = server::spawn_with_provider(
        config.clone(),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");

    let descriptor_path = config.descriptor_path();
    std::fs::remove_file(&descriptor_path).expect("remove the published descriptor");
    let fifo_path = std::ffi::CString::new(descriptor_path.as_os_str().as_encoded_bytes())
        .expect("the descriptor path has no NUL");
    assert_eq!(
        unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) },
        0,
        "put a FIFO in the descriptor's place"
    );
    let _fifo = ReleaseFifoReadersOnDrop(fifo_path);

    timeout(PROGRESS_DEADLINE, server.shutdown())
        .await
        .expect("a FIFO in the descriptor's place does not hold the stop up")
        .expect("shut down server");
    assert!(
        std::fs::symlink_metadata(&descriptor_path)
            .expect("the FIFO is left in place")
            .file_type()
            .is_fifo(),
        "the stopping Server left alone what was not its descriptor"
    );

    let successor = timeout(
        PROGRESS_DEADLINE,
        server::spawn_with_provider(
            config.clone(),
            Arc::new(CodexRuntime::new(fixture.executable())),
        ),
    )
    .await
    .expect("the successor's election ends")
    .expect("a successor is elected once the stopped Server let the election go");
    let published: RuntimeDescriptor = serde_json::from_slice(
        &std::fs::read(&descriptor_path).expect("read the successor's descriptor"),
    )
    .expect("decode the successor's descriptor");
    assert_eq!(
        published.identity.instance_id,
        successor.descriptor().identity.instance_id,
        "the successor publishes its descriptor in the FIFO's place"
    );
    successor.shutdown().await.expect("stop the successor");
}

/// Opens the FIFO at its path for writing as the test ends, however it ends,
/// so whatever is still waiting to open it for reading — a stop that failed
/// the test by waiting on it — is let go rather than holding the test's
/// runtime up as it shuts down.
struct ReleaseFifoReadersOnDrop(std::ffi::CString);

impl Drop for ReleaseFifoReadersOnDrop {
    fn drop(&mut self) {
        let writer = unsafe { libc::open(self.0.as_ptr(), libc::O_WRONLY | libc::O_NONBLOCK) };
        if writer >= 0 {
            unsafe { libc::close(writer) };
        }
    }
}

/// Begins an Attachment upload on the Server `descriptor` names that never
/// finishes, returning once the Server is reading its body, of which nothing
/// ever comes. Asking to continue before sending the body, as a client may,
/// the upload is answered as soon as the Server begins reading it, so it is
/// in flight — not merely connecting — once this returns. The connection is
/// held open for as long as the returned stream lives.
async fn begin_stuck_upload(descriptor: &RuntimeDescriptor) -> tokio::net::TcpStream {
    let address = descriptor
        .base_url
        .strip_prefix("http://")
        .expect("the local API is served over plain HTTP");
    let mut upload = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect to the server");
    upload
        .write_all(
            format!(
                "POST /v1/attachments HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {}\r\n\
                 Content-Type: image/png\r\nContent-Length: 1024\r\nExpect: 100-continue\r\n\r\n",
                descriptor.token
            )
            .as_bytes(),
        )
        .await
        .expect("send the upload's headers");
    let mut answered = Vec::new();
    timeout(PROGRESS_DEADLINE, async {
        while !String::from_utf8_lossy(&answered).contains("100 Continue") {
            let mut chunk = [0_u8; 256];
            let read = upload
                .read(&mut chunk)
                .await
                .expect("read the server's answer");
            assert_ne!(
                read, 0,
                "the server closed the upload before reading its body"
            );
            answered.extend_from_slice(&chunk[..read]);
        }
    })
    .await
    .expect("the server begins reading the upload's body");
    upload
}

/// The state, data, and Workspace directories of a `suru __server` process a
/// test launches, removed as the test ends — the state directory owned by a
/// [`DetachedServers`], so a server launched into it is gone however the test
/// ends.
struct ServerRoots {
    state: DetachedServers,
    data: tempfile::TempDir,
    workspace: tempfile::TempDir,
}

impl ServerRoots {
    fn new() -> Self {
        Self {
            state: DetachedServers::new(),
            data: tempfile::tempdir().expect("create isolated data root"),
            workspace: tempfile::tempdir().expect("create valid Workspace"),
        }
    }
}

/// Launches a `suru __server` process that runs `codex` as its Codex, its
/// command as `launch` leaves it, and begins a Session on it, returning once
/// Codex is working on its Turn, with the descriptor the Server published.
async fn serve_working_codex<'a>(
    codex: &'a ScriptedCodex,
    channel: &str,
    roots: &ServerRoots,
    launch: impl FnOnce(&mut std::process::Command),
) -> (SignalledServer<'a>, RuntimeDescriptor) {
    let config = ServerConfig::new(roots.state.state_dir(), channel)
        .expect("configure isolated server")
        .with_data_dir(roots.data.path());
    // Made first, as a managed client makes them before it launches a Server.
    config
        .create_private_runtime_dir()
        .expect("make the server's directories");
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_suru"));
    command
        .arg("__server")
        .arg("--state-dir")
        .arg(roots.state.state_dir())
        .arg("--data-dir")
        .arg(roots.data.path())
        .arg("--channel")
        .arg(channel)
        // Looking this often to see that it still stands for its Channel,
        // a Server whose state directory a test removes stops at once.
        .arg("--state-dir-check-interval-ms")
        .arg("10")
        // The other Providers are pointed at nothing, so the Server never
        // launches a real Copilot or Claude installed on this machine.
        .envs(roots.state.isolated_environment())
        .env("SURU_CODEX_PATH", codex.executable())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    launch(&mut command);
    let server = SignalledServer {
        process: command.spawn().expect("spawn isolated server process"),
        codex,
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
                path: roots.workspace.path().to_owned(),
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
    codex.wait_until_ready().await;
    (server, descriptor)
}

/// A `suru __server` process a test launched, and the Codex it may have
/// started. Dropped before the Server could take Codex down — the test failed
/// — it kills both, Codex by its process group so the descendants it started
/// go with it, rather than leaving them running past the test. It withdraws
/// the Codex release as well: every Codex launched through it runs on past
/// the Server, and keeps its descendants running, only while the release
/// stands, so whatever the kills miss — a Codex launched too late to be
/// found, say — ends on its own rather than outliving the cleanup.
struct SignalledServer<'a> {
    process: std::process::Child,
    codex: &'a ScriptedCodex,
}

impl SignalledServer<'_> {
    /// Waits for the server process to exit, and reaps it.
    async fn exit(&mut self) -> std::process::ExitStatus {
        timeout(PROGRESS_DEADLINE, async {
            loop {
                if let Some(status) = self.process.try_wait().expect("poll server process") {
                    return status;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("signalled server process exits")
    }
}

impl Drop for SignalledServer<'_> {
    fn drop(&mut self) {
        // Codex shares its group with the anchor that leads it, so the group
        // is looked up through Codex — before the Server is killed, which
        // may take Codex down with it.
        let codex_group = std::fs::read_to_string(self.codex.pid_file())
            .ok()
            .and_then(|pid| pid.trim().parse::<libc::pid_t>().ok())
            .filter(|pid| *pid > 0)
            .map(|pid| unsafe { libc::getpgid(pid) })
            .filter(|group| *group > 0 && *group != unsafe { libc::getpgrp() });
        if matches!(self.process.try_wait(), Ok(None)) {
            let _ = self.process.kill();
            let _ = self.process.wait();
        }
        self.codex.withdraw_release();
        if let Some(group) = codex_group
            && unsafe { libc::killpg(group, 0) } == 0
        {
            unsafe { libc::killpg(group, libc::SIGKILL) };
        }
    }
}
