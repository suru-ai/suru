//! Interrupting an active turn and settling what it left in flight.

use crate::support::{ScriptedCodex, receive_initial_state};
use std::sync::Arc;
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, SessionEvent},
    protocol::{
        Activity, CreateSessionRequest, InitialPrompt, MessageRole, MessageStatus, PromptId,
        SessionStatus, TurnStatus, Workspace,
    },
    provider::CodexRuntime,
    server::{self, ServerConfig, ServerTimings},
};
use tokio::time::{Duration, timeout};

const INTERRUPTION_CODEX: &str = r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"native-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn"}}}'
      ;;
    *'"method":"turn/interrupt"'*)
__INTERRUPT_ACTION__
      ;;
  esac
done
"#;

const ACKNOWLEDGE_AND_COMPLETE_INTERRUPTION: &str = r#"      printf '%s\n' '{"id":4,"result":{}}'
      while [ ! -e "$CODEX_FIXTURE_RELEASE" ]; do
        sleep 0.01
      done
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"agentMessage","id":"trailing-message","text":""}}}'
      printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"trailing-message","delta":"Trailing output"}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"agentMessage","id":"trailing-message","text":"Trailing output"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"interrupted","items":[]}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"interrupted","items":[]}}}'"#;

const REJECT_INTERRUPTION: &str = r#"      printf '%s\n' '{"id":4,"error":{"code":-32600,"message":"fixture rejected interruption"}}'"#;

const TIME_OUT_INTERRUPTION: &str = "      sleep 10";

const LOSE_PROCESS_DURING_INTERRUPT: &str = "      exit 23";

fn interruption_script(action: &str) -> String {
    INTERRUPTION_CODEX.replace("__INTERRUPT_ACTION__", action)
}

#[tokio::test]
async fn scripted_codex_interrupt_acknowledges_before_trailing_output_and_terminal_event() {
    let fixture = ScriptedCodex::new(&interruption_script(ACKNOWLEDGE_AND_COMPLETE_INTERRUPTION));
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-scripted-interruption")
            .expect("configure server"),
        Arc::new(CodexRuntime::new(fixture.executable())),
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-scripted-interruption")
            .expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Keep working until interrupted".to_owned(),
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
    let active = client
        .read_session(created.session.id)
        .await
        .expect("read active Session");
    let turn_id = active.turns[0].id;
    assert_eq!(active.turns[0].status, TurnStatus::Active);

    client
        .interrupt_session(created.session.id)
        .await
        .expect("Codex acknowledges interruption");
    client
        .interrupt_session(created.session.id)
        .await
        .expect("retry acknowledged interruption");

    let after_acknowledgement = client
        .read_session(created.session.id)
        .await
        .expect("read Session after interruption acknowledgement");
    assert_eq!(after_acknowledgement.session.status, SessionStatus::Active);
    assert_eq!(after_acknowledgement.turns[0].status, TurnStatus::Active);
    let interrupt_requests = fixture
        .requests()
        .into_iter()
        .filter(|request| request["method"] == "turn/interrupt")
        .collect::<Vec<_>>();
    assert_eq!(interrupt_requests.len(), 1);
    assert_eq!(interrupt_requests[0]["params"]["threadId"], "native-thread");
    assert_eq!(interrupt_requests[0]["params"]["turnId"], "native-turn");

    fixture.release();
    let mut interrupted_transitions = 0;
    let interrupted = timeout(Duration::from_secs(2), async {
        loop {
            let event = feed
                .next()
                .await
                .expect("Session feed remains open")
                .expect("Session update is valid");
            if let SessionEvent::Updated(update) = event {
                interrupted_transitions += update
                    .changes
                    .iter()
                    .filter(|change| {
                        matches!(
                            change,
                            suru::protocol::SessionChange::TurnStatusChanged {
                                turn_id: changed_turn_id,
                                status: TurnStatus::Interrupted,
                                ..
                            } if *changed_turn_id == turn_id
                        )
                    })
                    .count();
            }
            let snapshot = client
                .read_session(created.session.id)
                .await
                .expect("read interrupting Session");
            if interrupted_transitions == 1 {
                return snapshot;
            }
        }
    })
    .await
    .expect("Codex terminal interruption reaches Session SSE");

    assert_eq!(interrupted_transitions, 1);
    assert_eq!(interrupted.session.status, SessionStatus::Idle);
    assert_eq!(interrupted.turns[0].status, TurnStatus::Interrupted);
    let trailing = interrupted
        .messages
        .iter()
        .find(|message| message.role == MessageRole::Agent)
        .expect("trailing Agent Message is accepted");
    assert_eq!(trailing.status, MessageStatus::Completed);
    assert_eq!(trailing.content, "Trailing output");
    assert!(
        timeout(Duration::from_millis(100), feed.next())
            .await
            .is_err(),
        "duplicate native completion must not publish a second Session transition"
    );

    drop(feed);
    drop(client);
    server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn scripted_codex_interruption_failures_settle_the_command_and_session() {
    for (script, channel, expected_error) in [
        (
            REJECT_INTERRUPTION,
            "codex-interrupt-rejection",
            "fixture rejected interruption",
        ),
        (
            TIME_OUT_INTERRUPTION,
            "codex-interrupt-timeout",
            "timed out handling `turn/interrupt`",
        ),
        (
            LOSE_PROCESS_DURING_INTERRUPT,
            "codex-interrupt-process-loss",
            "status: 23",
        ),
    ] {
        assert_interruption_failure(script, channel, expected_error).await;
    }
}

async fn assert_interruption_failure(script: &str, channel: &str, expected_error: &str) {
    let fixture = ScriptedCodex::new(&interruption_script(script));
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider_and_timings(
        ServerConfig::new(state_dir.path(), channel).expect("configure server"),
        Arc::new(
            CodexRuntime::new(fixture.executable())
                .with_interrupt_request_timeout(Duration::from_millis(300))
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
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Interrupt this work".to_owned(),
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
    let active = client
        .read_session(created.session.id)
        .await
        .expect("read active Session");
    assert_eq!(active.turns[0].status, TurnStatus::Active);

    let error = timeout(
        Duration::from_secs(2),
        client.interrupt_session(created.session.id),
    )
    .await
    .unwrap_or_else(|_| panic!("{channel} interruption command must not hang"))
    .expect_err("Provider interruption failure reaches the client");
    assert!(
        error.to_string().contains(expected_error),
        "expected {expected_error:?} in {error:#}"
    );

    let failed = timeout(Duration::from_secs(2), async {
        loop {
            feed.next()
                .await
                .expect("Session feed remains open")
                .expect("Session update is valid");
            let snapshot = client
                .read_session(created.session.id)
                .await
                .expect("read Session after interruption failure");
            if snapshot.turns[0].status == TurnStatus::Failed {
                return snapshot;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{channel} failure reaches a terminal Session state"));
    assert_eq!(failed.session.status, SessionStatus::Idle);
    assert_eq!(failed.turns[0].status, TurnStatus::Failed);
    assert_eq!(failed.activities.len(), 1);
    let Activity::Error { text, .. } = &failed.activities[0] else {
        panic!("interruption failure must project as an error Activity");
    };
    assert!(
        text.contains(expected_error),
        "expected {expected_error:?} in {text:?}"
    );

    drop(feed);
    drop(client);
    server.shutdown().await.expect("shut down server");
}
