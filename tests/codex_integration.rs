#![cfg(unix)]

use std::{os::unix::fs::PermissionsExt, sync::Arc};

use chidori::{
    managed_client::{ManagedClient, ManagedClientConfig, ManagedEvent, SessionEvent},
    protocol::{
        ActivityKind, AgentId, CreateSessionRequest, InitialPrompt, MessageRole, MessageStatus,
        ModelId, PromptId, PromptStatus, ProviderId, SessionStatus, TurnStatus, Workspace,
    },
    provider::CodexRuntime,
    server::{self, ServerConfig},
};
use serde_json::Value;
use tokio::time::{Duration, timeout};

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

const INITIALIZE_REJECTION: &str = r#"#!/bin/sh
read -r line
printf '%s\n' '{"id":"1","error":{"code":-32000,"message":"fixture rejected initialization"}}'
"#;

const MALFORMED_OUTPUT: &str = r#"#!/bin/sh
read -r line
printf '%s\n' '{this is not JSON'
"#;

const EOF_WITH_PENDING_REQUEST: &str = r#"#!/bin/sh
read -r line
exit 0
"#;

const TURN_REQUEST_ERROR: &str = r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":"2","result":{"thread":{"id":"native-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"error":{"code":-32001,"message":"fixture rejected Turn startup"}}'
      ;;
  esac
done
"#;

const EOF_AFTER_TURN_START: &str = r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"native-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn"}}}'
      exit 0
      ;;
  esac
done
"#;

const NONZERO_AFTER_TURN_START: &str = r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"native-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn"}}}'
      exit 17
      ;;
  esac
done
"#;

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
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Explain the native harness".to_owned(),
            },
        })
        .await
        .expect("create Session without waiting for Codex startup");
    assert_eq!(created.session.agent, None);
    assert_eq!(created.session.status, SessionStatus::Idle);
    assert_eq!(created.prompts[0].status, PromptStatus::Pending);
    assert!(created.turns.is_empty());

    let mut feed = client
        .subscribe_session(created.session.id)
        .await
        .expect("subscribe to Session SSE");
    assert!(matches!(
        timeout(Duration::from_secs(2), feed.next())
            .await
            .expect("Session snapshot arrives")
            .expect("Session feed remains open")
            .expect("Session snapshot is valid"),
        SessionEvent::Snapshot(_)
    ));
    fixture.wait_for_method("turn/start").await;
    fixture.release();

    timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = client
                .read_session(created.session.id)
                .await
                .expect("read Session while Codex runs");
            if snapshot.turns.first().is_some_and(|turn| {
                matches!(
                    turn.status,
                    TurnStatus::Completed | TurnStatus::Failed | TurnStatus::Interrupted
                )
            }) {
                break;
            }
            feed.next()
                .await
                .expect("Session feed remains open")
                .expect("Session update is valid");
        }
    })
    .await
    .expect("Codex Turn reaches a terminal Session state");

    let completed = client
        .read_session(created.session.id)
        .await
        .expect("read completed Session");
    let identity = completed
        .session
        .agent
        .expect("effective Codex Agent is bound");
    assert_eq!(identity.agent, AgentId::new("codex"));
    assert_eq!(identity.provider, ProviderId::new("codex"));
    assert_eq!(identity.model, ModelId::new("gpt-fixture"));
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
    assert!(completed.activities.is_empty());

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
    assert!(requests[3]["params"].get("model").is_none());

    drop(feed);
    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn scripted_codex_projects_failed_and_interrupted_terminal_outcomes() {
    let failed = SCRIPTED_CODEX.replace(
        "\"status\":\"completed\",\"items\":[]",
        "\"status\":\"failed\",\"error\":{\"message\":\"fixture Turn failed\"},\"items\":[]",
    );
    run_terminal_fixture(
        &failed,
        "codex-scripted-failed",
        TurnStatus::Failed,
        Some("fixture Turn failed"),
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
    )
    .await;
}

#[tokio::test]
async fn codex_launch_protocol_and_process_failures_settle_as_error_activities() {
    let missing_directory = tempfile::tempdir().expect("create missing executable directory");
    assert_provider_failure(
        missing_directory.path().join("missing-codex"),
        "codex-missing-executable",
        "could not launch Codex app-server",
    )
    .await;

    let unlaunchable = tempfile::tempdir().expect("create unlaunchable executable directory");
    assert_provider_failure(
        unlaunchable.path(),
        "codex-spawn-failure",
        "could not launch Codex app-server",
    )
    .await;

    for (script, channel, expected) in [
        (
            INITIALIZE_REJECTION,
            "codex-initialize-rejection",
            "fixture rejected initialization",
        ),
        (MALFORMED_OUTPUT, "codex-malformed-output", "malformed JSON"),
        (
            EOF_WITH_PENDING_REQUEST,
            "codex-pending-request-eof",
            "Codex app-server",
        ),
        (
            TURN_REQUEST_ERROR,
            "codex-turn-request-error",
            "fixture rejected Turn startup",
        ),
        (
            EOF_AFTER_TURN_START,
            "codex-unexpected-eof",
            "Codex app-server",
        ),
        (NONZERO_AFTER_TURN_START, "codex-nonzero-exit", "status: 17"),
    ] {
        let fixture = ScriptedCodex::new(script);
        assert_provider_failure(fixture.executable(), channel, expected).await;
    }

    let oversized_message = format!("first\\nsecond {}", "diagnostic".repeat(300));
    let oversized_error =
        TURN_REQUEST_ERROR.replace("fixture rejected Turn startup", &oversized_message);
    let fixture = ScriptedCodex::new(&oversized_error);
    assert_provider_failure(
        fixture.executable(),
        "codex-oversized-request-error",
        "first second",
    )
    .await;
}

async fn assert_provider_failure(
    executable: impl AsRef<std::ffi::OsStr>,
    channel: &str,
    expected_error: &str,
) {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), channel).expect("configure server"),
        Arc::new(CodexRuntime::new(executable)),
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
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Surface the Provider failure".to_owned(),
            },
        })
        .await
        .expect("create Session before Provider startup settles");
    let mut feed = client
        .subscribe_session(created.session.id)
        .await
        .expect("subscribe to Session SSE");

    let failed = timeout(Duration::from_secs(2), async {
        loop {
            feed.next()
                .await
                .expect("Session feed remains open")
                .expect("Session event is valid");
            let snapshot = client
                .read_session(created.session.id)
                .await
                .expect("read Session after Provider failure");
            if snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.status == TurnStatus::Failed)
            {
                return snapshot;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{channel} failure reaches a terminal Session state"));

    assert_eq!(failed.session.status, SessionStatus::Idle);
    assert_eq!(failed.prompts[0].status, PromptStatus::Delivered);
    assert_eq!(failed.turns[0].status, TurnStatus::Failed);
    assert_eq!(failed.activities.len(), 1);
    assert_eq!(failed.activities[0].kind, ActivityKind::Error);
    assert!(
        failed.activities[0].text.contains(expected_error),
        "expected {expected_error:?} in {:?}",
        failed.activities[0].text
    );
    assert!(!failed.activities[0].text.contains('\n'));
    assert!(
        failed.activities[0].text.chars().count() <= 512,
        "Provider failure Activity should remain concise: {:?}",
        failed.activities[0].text
    );

    drop(feed);
    drop(client);
    server.shutdown().await.expect("shut down server");
}

async fn run_terminal_fixture(
    script: &str,
    channel: &str,
    expected_status: TurnStatus,
    expected_error: Option<&str>,
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
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Reach the requested terminal state".to_owned(),
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
                .any(|activity| { activity.text.contains(expected_error) })
        ),
        None => assert!(completed.activities.is_empty()),
    }

    drop(feed);
    drop(client);
    server.shutdown().await.expect("shut down server");
}

async fn receive_initial_state(client: &mut ManagedClient) {
    assert!(matches!(
        timeout(Duration::from_secs(1), client.next()).await,
        Ok(Some(ManagedEvent::Connecting))
    ));
    assert!(matches!(
        timeout(Duration::from_secs(1), client.next()).await,
        Ok(Some(ManagedEvent::Connected(_)))
    ));
}

struct ScriptedCodex {
    _directory: tempfile::TempDir,
    executable: std::path::PathBuf,
    log: std::path::PathBuf,
    release: std::path::PathBuf,
}

impl ScriptedCodex {
    fn new(script: &str) -> Self {
        let directory = tempfile::tempdir().expect("create scripted Codex directory");
        let executable = directory.path().join("codex");
        let log = directory.path().join("requests.jsonl");
        let release = directory.path().join("release");
        let script = script
            .replace(
                "$CODEX_FIXTURE_LOG",
                log.to_str().expect("fixture log path is UTF-8"),
            )
            .replace(
                "$CODEX_FIXTURE_RELEASE",
                release.to_str().expect("fixture release path is UTF-8"),
            );
        std::fs::write(&executable, script).expect("write scripted Codex executable");
        let mut permissions = std::fs::metadata(&executable)
            .expect("read scripted Codex metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).expect("make scripted Codex executable");
        Self {
            _directory: directory,
            executable,
            log,
            release,
        }
    }

    fn executable(&self) -> &std::path::Path {
        &self.executable
    }

    async fn wait_for_method(&self, expected: &str) {
        timeout(Duration::from_secs(2), async {
            loop {
                if self
                    .requests()
                    .iter()
                    .any(|request| request.get("method").and_then(Value::as_str) == Some(expected))
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "scripted Codex receives {expected}; captured requests: {:?}",
                self.requests()
            )
        });
    }

    fn release(&self) {
        std::fs::write(&self.release, b"release").expect("release scripted Codex events");
    }

    fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("decode captured Codex request"))
            .collect()
    }
}
