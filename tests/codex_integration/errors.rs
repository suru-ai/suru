//! Protocol violations and launch failures surfaced as error activities.

use crate::server_support::PROGRESS_DEADLINE;
use crate::support::{ScriptedCodex, receive_initial_state};
use serde_json::Value;
use std::sync::Arc;
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig},
    protocol::{
        Activity, CreateSessionRequest, InitialPrompt, PromptId, PromptStatus, SessionStatus,
        TurnStatus,
    },
    provider::CodexRuntime,
    server::{self, ServerConfig, ServerTimings},
};
use tokio::time::{Duration, timeout};

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
  append_line "$CODEX_FIXTURE_LOG" "$line"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"config/read"'*)
      printf '%s\n' '{"id":2,"result":{"config":{},"origins":{}}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":"3","result":{"thread":{"id":"native-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":4,"error":{"code":-32001,"message":"fixture rejected Turn startup"}}'
      ;;
  esac
done
"#;

/// An app-server that cannot read the user's configuration, though it would start a thread if
/// asked to.
const CONFIG_READ_REJECTION: &str = r#"#!/bin/sh
while IFS= read -r line; do
  append_line "$CODEX_FIXTURE_LOG" "$line"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"config/read"'*)
      printf '%s\n' '{"id":2,"error":{"code":-32603,"message":"fixture cannot read config.toml"}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":3,"result":{"thread":{"id":"native-thread"},"model":"gpt-fixture"}}'
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
    *'"method":"config/read"'*)
      printf '%s\n' '{"id":2,"result":{"config":{},"origins":{}}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":3,"result":{"thread":{"id":"native-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":4,"result":{"turn":{"id":"native-turn"}}}'
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
    *'"method":"config/read"'*)
      printf '%s\n' '{"id":2,"result":{"config":{},"origins":{}}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":3,"result":{"thread":{"id":"native-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":4,"result":{"turn":{"id":"native-turn"}}}'
      exit 17
      ;;
  esac
done
"#;

const UNKNOWN_SERVER_REQUEST: &str = r#"#!/bin/sh
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
      printf '%s\n' '{"id":"unknown-correlation","method":"future/request","params":{"ignored":true}}'
      read -r response
      append_line "$CODEX_FIXTURE_LOG" "$response"
      printf '%s\n' '{"id":4,"result":{"turn":{"id":"native-turn"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"completed","items":[]}}}'
      ;;
  esac
done
"#;

const UNSUPPORTED_SERVER_REQUEST: &str = r#"#!/bin/sh
request_pipe="$CODEX_FIXTURE_LOG.pipe"
mkfifo "$request_pipe"
exec 3<&0
while IFS= read -r captured; do
  append_line "$CODEX_FIXTURE_LOG" "$captured"
  printf '%s\n' "$captured"
done <&3 > "$request_pipe" &
while IFS= read -r line; do
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
      printf '%s\n' '{"id":"unsupported-correlation","method":"$CODEX_FIXTURE_METHOD","params":{"fixture":true}}'
      read -r response
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"completed","items":[]}}}'
      ;;
  esac
done < "$request_pipe"
"#;

const PROCESS_LOSS_WITH_UNSUPPORTED_REQUEST: &str = r#"#!/bin/sh
while IFS= read -r line; do
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
      printf '%s\n' '{"id":"abandoned-interaction","method":"item/commandExecution/requestApproval","params":{"fixture":true}}'
      exit 17
      ;;
  esac
done
"#;

#[tokio::test]
async fn unknown_server_request_gets_method_not_found_without_corrupting_response_routing() {
    let fixture = ScriptedCodex::new(UNKNOWN_SERVER_REQUEST);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-unknown-server-request")
            .expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-unknown-server-request")
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
                text: "Keep routing the Codex Turn".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create Session");
    let mut feed = client
        .subscribe_session(created.session.id)
        .await
        .expect("subscribe to Session SSE");

    let completed = timeout(PROGRESS_DEADLINE, async {
        loop {
            feed.next()
                .await
                .expect("Session feed remains open")
                .expect("Session event is valid");
            let snapshot = client
                .read_session(created.session.id)
                .await
                .expect("read Session after unknown server request");
            if snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.status != TurnStatus::Active)
            {
                return snapshot;
            }
        }
    })
    .await
    .expect("Codex Turn settles after unknown server request");

    assert_eq!(completed.turns[0].status, TurnStatus::Completed);
    assert!(completed.activities.is_empty());
    let response = fixture
        .requests()
        .into_iter()
        .find(|message| message.get("id") == Some(&Value::String("unknown-correlation".to_owned())))
        .expect("unknown server request receives a response");
    assert_eq!(response["error"]["code"], -32601);
    assert!(response.get("result").is_none());

    drop(feed);
    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn unsupported_elicitation_and_dynamic_tools_are_rejected_without_ending_the_turn() {
    for (method, channel) in [
        (
            "mcpServer/elicitation/request",
            "codex-unsupported-elicitation",
        ),
        ("item/tool/call", "codex-unsupported-host-tool"),
    ] {
        let script = UNSUPPORTED_SERVER_REQUEST.replace("$CODEX_FIXTURE_METHOD", method);
        let fixture = ScriptedCodex::new(&script);
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let server = server::spawn_with_provider(
            ServerConfig::new(state.path(), channel).unwrap(),
            Arc::new(CodexRuntime::new(fixture.executable())),
        )
        .await
        .unwrap();
        let mut client =
            ManagedClient::connect(ManagedClientConfig::new(state.path(), channel).unwrap())
                .await
                .unwrap();
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
                    text: "Continue after unsupported callback".into(),
                    skill_invocations: Vec::new(),
                    attachments: Vec::new(),
                },
            })
            .await
            .unwrap();
        timeout(PROGRESS_DEADLINE, async {
            loop {
                if client
                    .read_session(created.session.id)
                    .await
                    .unwrap()
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
        .expect("unsupported request is nonfatal");

        let response = fixture
            .requests()
            .into_iter()
            .find(|message| {
                message.get("id") == Some(&Value::String("unsupported-correlation".to_owned()))
            })
            .unwrap_or_else(|| panic!("{method} receives a correlated response"));
        assert_eq!(response["error"]["code"], -32000);
        assert!(
            response["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains(method))
        );
        assert!(response.get("result").is_none());
        drop(client);
        server.shutdown().await.unwrap();
    }
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
        (
            PROCESS_LOSS_WITH_UNSUPPORTED_REQUEST,
            "codex-process-loss-with-unsupported-request",
            "Codex app-server",
        ),
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

/// The Broker's note takes the place of the developer instructions the user's configuration sets
/// unless it follows them, so a launch that cannot read them starts no thread at all.
#[tokio::test]
async fn a_codex_launch_that_cannot_read_the_users_configuration_starts_no_thread() {
    let fixture = ScriptedCodex::new(CONFIG_READ_REJECTION);
    assert_provider_failure(
        fixture.executable(),
        "codex-config-read-rejection",
        "fixture cannot read config.toml",
    )
    .await;
    assert!(
        !fixture
            .methods()
            .iter()
            .any(|method| method == "thread/start"),
        "no thread starts without the user's own developer instructions: {:?}",
        fixture.methods()
    );
}

async fn assert_provider_failure(
    executable: impl AsRef<std::ffi::OsStr>,
    channel: &str,
    expected_error: &str,
) {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider_and_timings(
        ServerConfig::new(state_dir.path(), channel).expect("configure server"),
        Arc::new(
            CodexRuntime::new(executable)
                .with_shutdown_interrupt_timeout(Duration::from_millis(25))
                .with_process_exit_grace(Duration::from_millis(50)),
        ),
        ServerTimings {
            shutdown_grace: Duration::from_millis(10),
            ..ServerTimings::default()
        },
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
                text: "Surface the Provider failure".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create Session before Provider startup settles");
    let mut feed = client
        .subscribe_session(created.session.id)
        .await
        .expect("subscribe to Session SSE");

    let failed = timeout(PROGRESS_DEADLINE, async {
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
    let Activity::Error { text, .. } = &failed.activities[0] else {
        panic!("Provider failure must be an Error Activity");
    };
    assert!(
        text.contains(expected_error),
        "expected {expected_error:?} in {text:?}"
    );
    assert!(!text.contains('\n'));
    assert!(
        text.chars().count() <= 1_152,
        "Provider failure Activity should remain concise: {text:?}"
    );

    drop(feed);
    drop(client);
    server.shutdown().await.expect("shut down server");
}
