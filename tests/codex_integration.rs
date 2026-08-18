#![cfg(unix)]

use std::{os::unix::fs::PermissionsExt, sync::Arc};

use chidori::{
    managed_client::{
        ManagedClient, ManagedClientConfig, ManagedEvent, SessionEvent, SessionSubscription,
    },
    protocol::{
        Activity, ActivityId, ActivityStatus, AdmitPromptRequest, AgentId, CreateSessionRequest,
        InitialPrompt, MessageRole, MessageStatus, ModelId, PromptDelivery, PromptId, PromptStatus,
        ProviderId, SessionChange, SessionId, SessionSnapshot, SessionStatus, TranscriptItem,
        TurnId, TurnStatus, Workspace,
    },
    provider::CodexRuntime,
    server::{self, RunningServer, ServerConfig},
    tui::{Application, ApplicationEvent},
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
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"commandExecution","id":"native-command","command":"cargo test --test codex_integration","cwd":"/fixture/work","status":"inProgress","futureField":true},"futureField":true}}'
      printf '%s\n' '{"method":"item/commandExecution/outputDelta","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"other-command","delta":"wrong command output"}}'
      printf '%s\n' '{"method":"item/commandExecution/outputDelta","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"native-command","delta":"running ","futureField":true}}'
      printf '%s\n' '{"method":"item/commandExecution/outputDelta","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"native-command","delta":"tests\n"}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"commandExecution","id":"native-command","command":"cargo test --test codex_integration","cwd":"/fixture/work","status":"completed","aggregatedOutput":"running tests\nall green\n","exitCode":0,"futureField":true},"futureField":true}}'
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

const START_TURN: &str = r#"      printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn"}}}'"#;

const COMPLETE_BEFORE_STEER: &str = r#"      while [ ! -e "$CODEX_FIXTURE_RELEASE" ]; do
        sleep 0.01
      done
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"completed","items":[]}}}'"#;

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

const STEERING_CODEX: &str = r#"#!/bin/sh
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
__TURN_START_ACTION__
      ;;
    *'"method":"turn/steer"'*)
__STEER_ACTION__
      ;;
  esac
done
"#;

const ACCEPT_STEER: &str = r#"      while [ ! -e "$CODEX_FIXTURE_RELEASE" ]; do
        sleep 0.01
      done
      printf '%s\n' '{"id":4,"result":{"turnId":"native-turn"}}'"#;

const REJECT_STEER: &str = r#"      printf '%s\n' '{"id":4,"error":{"code":-32600,"message":"fixture rejected steering"}}'"#;

const COMPLETE_THEN_ACCEPT_STEER: &str = r#"      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"completed","items":[]}}}'
      printf '%s\n' '{"id":4,"result":{"turnId":"native-turn"}}'"#;

const COMPLETE_THEN_REJECT_STEER: &str = r#"      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"completed","items":[]}}}'
      printf '%s\n' '{"id":4,"error":{"code":-32600,"message":"no active turn to steer"}}'"#;

const LOSE_PROCESS_DURING_STEER: &str = "      exit 29";

fn interruption_script(action: &str) -> String {
    INTERRUPTION_CODEX.replace("__INTERRUPT_ACTION__", action)
}

fn steering_script(action: &str) -> String {
    steering_script_with_start(START_TURN, action)
}

fn steering_script_with_start(start_action: &str, steer_action: &str) -> String {
    STEERING_CODEX
        .replace("__TURN_START_ACTION__", start_action)
        .replace("__STEER_ACTION__", steer_action)
}

struct SteeringFixture {
    codex: ScriptedCodex,
    _state_dir: tempfile::TempDir,
    _workspace: tempfile::TempDir,
    server: RunningServer,
    client: ManagedClient,
    feed: SessionSubscription,
    session_id: SessionId,
    turn_id: TurnId,
}

impl SteeringFixture {
    async fn start(script: &str, channel: &str) -> Self {
        let codex = ScriptedCodex::new(script);
        let state_dir = tempfile::tempdir().expect("create isolated state directory");
        let workspace = tempfile::tempdir().expect("create valid Workspace");
        let server = server::spawn_with_provider(
            ServerConfig::new(state_dir.path(), channel).expect("configure server"),
            Arc::new(CodexRuntime::new(codex.executable())),
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
                    text: "Begin the steering fixture".to_owned(),
                },
            })
            .await
            .expect("create Session");
        codex.wait_for_method("turn/start").await;
        let active = timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = client
                    .read_session(created.session.id)
                    .await
                    .expect("read Session");
                if snapshot
                    .turns
                    .first()
                    .is_some_and(|turn| turn.status == TurnStatus::Active)
                {
                    return snapshot;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("initial Codex Turn becomes active");
        let mut feed = client
            .subscribe_session(created.session.id)
            .await
            .expect("subscribe to Session SSE");
        assert!(matches!(
            feed.next()
                .await
                .expect("Session feed remains open")
                .expect("Session snapshot is valid"),
            SessionEvent::Snapshot(_)
        ));
        Self {
            codex,
            _state_dir: state_dir,
            _workspace: workspace,
            server,
            client,
            feed,
            session_id: created.session.id,
            turn_id: active.turns[0].id,
        }
    }

    async fn wait_for(
        &mut self,
        description: &str,
        predicate: impl Fn(&SessionSnapshot) -> bool,
    ) -> SessionSnapshot {
        timeout(Duration::from_secs(2), async {
            loop {
                self.feed
                    .next()
                    .await
                    .expect("Session feed remains open")
                    .expect("Session update is valid");
                let snapshot = self
                    .client
                    .read_session(self.session_id)
                    .await
                    .expect("read steering fixture Session");
                if predicate(&snapshot) {
                    return snapshot;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{description}"))
    }

    async fn shutdown(self) {
        let Self {
            codex,
            _state_dir: state_dir,
            _workspace: workspace,
            server,
            client,
            feed,
            ..
        } = self;
        drop(feed);
        drop(client);
        server.shutdown().await.expect("shut down server");
        drop(codex);
        drop(state_dir);
        drop(workspace);
    }
}

enum TerminalSteerOutcome {
    Accepted,
    Rejected { error: &'static str },
}

#[tokio::test]
async fn scripted_codex_accepts_one_idempotent_steer_before_delivering_its_prompt() {
    let mut fixture =
        SteeringFixture::start(&steering_script(ACCEPT_STEER), "codex-scripted-steering").await;
    let active_turn_id = fixture.turn_id;
    let prompt_id = PromptId::new();
    let steer = AdmitPromptRequest {
        prompt: InitialPrompt {
            id: prompt_id,
            text: "Change course".to_owned(),
        },
        delivery: PromptDelivery::Steer,
    };

    let admitted = fixture
        .client
        .admit_prompt(fixture.session_id, steer.clone())
        .await
        .expect("admit steer Prompt");
    assert_eq!(admitted.status, PromptStatus::Pending);
    fixture.codex.wait_for_method("turn/steer").await;

    let retried = fixture
        .client
        .admit_prompt(fixture.session_id, steer.clone())
        .await
        .expect("retry identical steer admission");
    assert_eq!(retried.status, PromptStatus::Pending);
    let before_acknowledgement = fixture
        .client
        .read_session(fixture.session_id)
        .await
        .expect("read Session before steering acknowledgement");
    assert_eq!(
        before_acknowledgement.prompts[1].status,
        PromptStatus::Pending
    );
    assert_eq!(before_acknowledgement.turns.len(), 1);
    assert_eq!(before_acknowledgement.messages.len(), 1);

    fixture.codex.release();
    let delivered = fixture
        .wait_for("accepted steer becomes delivered", |snapshot| {
            snapshot
                .prompts
                .iter()
                .find(|prompt| prompt.id == prompt_id)
                .is_some_and(|prompt| prompt.status == PromptStatus::Delivered)
        })
        .await;
    assert_eq!(delivered.turns.len(), 1);
    assert_eq!(delivered.turns[0].id, active_turn_id);
    assert_eq!(delivered.turns[0].status, TurnStatus::Active);
    assert_eq!(delivered.messages.len(), 2);
    assert_eq!(delivered.messages[1].role, MessageRole::User);
    assert_eq!(delivered.messages[1].turn_id, active_turn_id);
    assert_eq!(delivered.messages[1].content, "Change course");

    let after_delivery_retry = fixture
        .client
        .admit_prompt(fixture.session_id, steer)
        .await
        .expect("retry delivered steer admission");
    assert_eq!(after_delivery_retry.status, PromptStatus::Delivered);
    let steer_requests = fixture
        .codex
        .requests()
        .into_iter()
        .filter(|request| request["method"] == "turn/steer")
        .collect::<Vec<_>>();
    assert_eq!(steer_requests.len(), 1);
    assert_eq!(steer_requests[0]["params"]["threadId"], "native-thread");
    assert_eq!(steer_requests[0]["params"]["expectedTurnId"], "native-turn");
    assert_eq!(
        steer_requests[0]["params"]["input"],
        serde_json::json!([{ "type": "text", "text": "Change course" }])
    );

    fixture.shutdown().await;
}

#[tokio::test]
async fn scripted_codex_rejection_keeps_the_steer_pending_and_reports_the_failure() {
    let mut fixture = SteeringFixture::start(
        &steering_script(REJECT_STEER),
        "codex-scripted-steer-rejection",
    )
    .await;
    let prompt_id = PromptId::new();
    let admitted = fixture
        .client
        .admit_prompt(
            fixture.session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: prompt_id,
                    text: "Try a rejected course correction".to_owned(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("admit steer Prompt");
    assert_eq!(admitted.status, PromptStatus::Pending);

    let rejected = fixture
        .wait_for("steering rejection reaches Session SSE", |snapshot| {
            snapshot.activities.iter().any(|activity| {
                matches!(activity,
                    Activity::Error { text, .. } if text.contains("fixture rejected steering"))
            })
        })
        .await;
    assert_eq!(
        rejected
            .prompts
            .iter()
            .find(|prompt| prompt.id == prompt_id)
            .expect("steer Prompt remains authoritative")
            .status,
        PromptStatus::Pending
    );
    assert_eq!(rejected.turns.len(), 1);
    assert_eq!(rejected.turns[0].status, TurnStatus::Active);
    assert_eq!(rejected.messages.len(), 1);
    assert_eq!(rejected.activities.len(), 1);
    assert!(matches!(rejected.activities[0], Activity::Error { .. }));

    fixture.shutdown().await;
}

#[tokio::test]
async fn scripted_codex_transport_loss_during_steering_keeps_the_prompt_pending() {
    let mut fixture = SteeringFixture::start(
        &steering_script(LOSE_PROCESS_DURING_STEER),
        "codex-scripted-steer-process-loss",
    )
    .await;
    let prompt_id = PromptId::new();
    fixture
        .client
        .admit_prompt(
            fixture.session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: prompt_id,
                    text: "Steer across a broken transport".to_owned(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("admit steer Prompt");

    let failed = fixture
        .wait_for("steering transport loss reaches Session SSE", |snapshot| {
            snapshot.turns[0].status == TurnStatus::Failed
        })
        .await;
    assert_eq!(
        failed
            .prompts
            .iter()
            .find(|prompt| prompt.id == prompt_id)
            .expect("steer Prompt remains authoritative")
            .status,
        PromptStatus::Pending
    );
    assert_eq!(failed.messages.len(), 1);
    assert!(
        failed.activities.iter().any(|activity| matches!(activity,
                Activity::Error { text, .. } if text.contains("status: 29"))),
        "transport failure is visible: {:?}",
        failed.activities
    );

    fixture.shutdown().await;
}

#[tokio::test]
async fn scripted_codex_terminal_completion_during_steering_is_ordered_after_its_response() {
    assert_terminal_steering_race(
        COMPLETE_THEN_ACCEPT_STEER,
        "codex-steer-terminal-acceptance",
        TerminalSteerOutcome::Accepted,
    )
    .await;
    assert_terminal_steering_race(
        COMPLETE_THEN_REJECT_STEER,
        "codex-steer-terminal-rejection",
        TerminalSteerOutcome::Rejected {
            error: "no active turn to steer",
        },
    )
    .await;
}

#[tokio::test]
async fn scripted_codex_terminal_event_wins_when_it_precedes_the_queued_steer() {
    let mut fixture = SteeringFixture::start(
        &steering_script_with_start(COMPLETE_BEFORE_STEER, ACCEPT_STEER),
        "codex-terminal-before-steer",
    )
    .await;
    let prompt_id = PromptId::new();
    fixture
        .client
        .admit_prompt(
            fixture.session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: prompt_id,
                    text: "Stay pending after the terminal boundary".to_owned(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("admit boundary steer Prompt");
    fixture.codex.release();

    let completed = fixture
        .wait_for("native completion reaches Session SSE", |snapshot| {
            snapshot.turns[0].status == TurnStatus::Completed
        })
        .await;
    assert_eq!(completed.turns.len(), 1);
    assert_eq!(completed.messages.len(), 1);
    assert_eq!(
        completed
            .prompts
            .iter()
            .find(|prompt| prompt.id == prompt_id)
            .expect("boundary Prompt remains authoritative")
            .status,
        PromptStatus::Pending
    );
    assert_eq!(
        fixture
            .codex
            .requests()
            .iter()
            .filter(|request| request["method"] == "turn/steer")
            .count(),
        0
    );

    fixture.shutdown().await;
}

async fn assert_terminal_steering_race(
    steer_action: &str,
    channel: &str,
    outcome: TerminalSteerOutcome,
) {
    let mut fixture = SteeringFixture::start(&steering_script(steer_action), channel).await;
    let active_turn_id = fixture.turn_id;
    let prompt_id = PromptId::new();
    fixture
        .client
        .admit_prompt(
            fixture.session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: prompt_id,
                    text: "Race the terminal boundary".to_owned(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("admit boundary steer Prompt");

    let completed = fixture
        .wait_for(
            "terminal steering race settles through Session SSE",
            |snapshot| snapshot.turns[0].status == TurnStatus::Completed,
        )
        .await;
    let (expected_prompt_status, expected_message_count, expected_error) = match outcome {
        TerminalSteerOutcome::Accepted => (PromptStatus::Delivered, 2, None),
        TerminalSteerOutcome::Rejected { error } => (PromptStatus::Pending, 1, Some(error)),
    };
    assert_eq!(completed.session.status, SessionStatus::Idle);
    assert_eq!(completed.turns.len(), 1);
    assert_eq!(completed.turns[0].id, active_turn_id);
    assert_eq!(
        completed
            .prompts
            .iter()
            .find(|prompt| prompt.id == prompt_id)
            .expect("boundary steer Prompt remains authoritative")
            .status,
        expected_prompt_status
    );
    assert_eq!(completed.messages.len(), expected_message_count);
    assert_eq!(
        completed
            .messages
            .iter()
            .filter(|message| message.content == "Race the terminal boundary")
            .count(),
        usize::from(expected_prompt_status == PromptStatus::Delivered)
    );
    match expected_error {
        Some(expected_error) => assert!(
            completed
                .activities
                .iter()
                .any(|activity| matches!(activity,
                    Activity::Error { text, .. } if text.contains(expected_error)))
        ),
        None => assert!(completed.activities.is_empty()),
    }

    fixture.shutdown().await;
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
    let initial_event = timeout(Duration::from_secs(2), feed.next())
        .await
        .expect("Session snapshot arrives")
        .expect("Session feed remains open")
        .expect("Session snapshot is valid");
    assert!(matches!(initial_event, SessionEvent::Snapshot(_)));
    let mut application = Application::new(workspace.path());
    application
        .handle_event(ApplicationEvent::Session(initial_event))
        .expect("initial SSE snapshot hydrates the client projection");
    fixture.wait_for_method("turn/start").await;
    fixture.release();

    let mut streamed_command_id: Option<ActivityId> = None;
    let mut streamed_command_output = String::new();
    let mut saw_command_completion = false;
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
    assert_eq!(completed.activities.len(), 1);
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
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Keep working until interrupted".to_owned(),
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

    let acknowledged = client
        .interrupt_turn(created.session.id, turn_id)
        .await
        .expect("Codex acknowledges interruption");
    assert_eq!(acknowledged.status, TurnStatus::Active);
    let retried = client
        .interrupt_turn(created.session.id, turn_id)
        .await
        .expect("retry acknowledged interruption");
    assert_eq!(retried.status, TurnStatus::Active);

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
                            chidori::protocol::SessionChange::TurnStatusChanged {
                                turn_id: changed_turn_id,
                                status: TurnStatus::Interrupted,
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
    let Activity::Error { text, .. } = &failed.activities[0] else {
        panic!("Provider failure must be an Error Activity");
    };
    assert!(
        text.contains(expected_error),
        "expected {expected_error:?} in {text:?}"
    );
    assert!(!text.contains('\n'));
    assert!(
        text.chars().count() <= 512,
        "Provider failure Activity should remain concise: {text:?}"
    );

    drop(feed);
    drop(client);
    server.shutdown().await.expect("shut down server");
}

async fn assert_interruption_failure(script: &str, channel: &str, expected_error: &str) {
    let fixture = ScriptedCodex::new(&interruption_script(script));
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
                text: "Interrupt this work".to_owned(),
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

    let error = timeout(
        Duration::from_secs(7),
        client.interrupt_turn(created.session.id, active.turns[0].id),
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
