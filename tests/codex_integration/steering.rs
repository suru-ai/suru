//! Queue admission and steering a turn that is already under way.

use crate::server_support::PROGRESS_DEADLINE;
use crate::support::{ScriptedCodex, receive_initial_state};
use std::sync::Arc;
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, SessionEvent, SessionSubscription},
    protocol::{
        Activity, AdmitPromptRequest, CreateSessionRequest, InitialPrompt, MessageRole,
        PromptDelivery, PromptId, PromptStatus, SessionId, SessionSnapshot, SessionStatus, TurnId,
        TurnStatus,
    },
    provider::CodexRuntime,
    server::{self, RunningServer, ServerConfig},
};
use tokio::time::{Duration, timeout};

const START_TURN: &str = r#"      printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn"}}}'"#;

const SEQUENTIAL_QUEUE_TURNS: &str = r#"      turn_index=$((turn_index + 1))
      response_id=$((turn_index + 2))
      printf '{"id":%s,"result":{"turn":{"id":"native-turn-%s"}}}\n' "$response_id" "$turn_index"
      (
        while [ ! -e "$CODEX_FIXTURE_RELEASE-$turn_index" ]; do
          sleep 0.01
        done
        printf '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn-%s","status":"completed","items":[]}}}\n' "$turn_index"
        printf '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn-%s","status":"completed","items":[]}}}\n' "$turn_index"
      ) &"#;

const TERMINAL_BOUNDARY_TURNS: &str = r#"      turn_index=$((turn_index + 1))
      if [ "$turn_index" -eq 1 ]; then
        while [ ! -e "$CODEX_FIXTURE_RELEASE" ]; do
          sleep 0.01
        done
        printf '%s\n' '{"id":3,"result":{"turn":{"id":"native-turn-1"}}}'
        printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn-1","status":"completed","items":[]}}}'
      else
        printf '%s\n' '{"id":4,"result":{"turn":{"id":"native-turn-2"}}}'
        (
          while [ ! -e "$CODEX_FIXTURE_RELEASE-2" ]; do
            sleep 0.01
          done
          printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn-2","status":"completed","items":[]}}}'
        ) &
      fi"#;

const UNEXPECTED_STEER: &str = "      exit 65";

const PROMPT_OPERATION_CODEX: &str = r#"#!/bin/sh
turn_index=0
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
wait
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

fn steering_script(action: &str) -> String {
    prompt_operation_script(START_TURN, action)
}

fn prompt_operation_script(start_action: &str, steer_action: &str) -> String {
    PROMPT_OPERATION_CODEX
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
                preparation_id: None,
                agent_selection: None,
                execution_directory: suru::protocol::ExecutionDirectory {
                    path: workspace.path().to_owned(),
                },
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Begin the steering fixture".to_owned(),
                    skill_invocations: Vec::new(),
                },
            })
            .await
            .expect("create Session");
        codex.wait_for_method("turn/start").await;
        let active = timeout(PROGRESS_DEADLINE, async {
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
        timeout(PROGRESS_DEADLINE, async {
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
async fn scripted_codex_delivers_the_authoritative_queue_once_in_admission_order() {
    let codex = ScriptedCodex::new(&prompt_operation_script(
        SEQUENTIAL_QUEUE_TURNS,
        UNEXPECTED_STEER,
    ));
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-scripted-queueing").expect("configure server"),
        Arc::new(CodexRuntime::new(codex.executable())),
    )
    .await
    .expect("spawn server");
    let mut author = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-scripted-queueing")
            .expect("configure author client"),
    )
    .await
    .expect("connect author client");
    let mut observer = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-scripted-queueing")
            .expect("configure observer client"),
    )
    .await
    .expect("connect observer client");
    receive_initial_state(&mut author).await;
    receive_initial_state(&mut observer).await;

    let created = author
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Run the initial Turn".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create queued-delivery Session");
    let session_id = created.session.id;
    let mut feed = observer
        .subscribe_session(session_id)
        .await
        .expect("observe Session through SSE");
    codex.wait_for_method_count("turn/start", 1).await;

    let first_request = AdmitPromptRequest {
        prompt: InitialPrompt {
            id: PromptId::new(),
            text: "Run the first queued Turn".to_owned(),
            skill_invocations: Vec::new(),
        },
        delivery: PromptDelivery::Queue,
    };
    let cancelled_request = AdmitPromptRequest {
        prompt: InitialPrompt {
            id: PromptId::new(),
            text: "Cancel this queued Turn".to_owned(),
            skill_invocations: Vec::new(),
        },
        delivery: PromptDelivery::Queue,
    };
    let second_request = AdmitPromptRequest {
        prompt: InitialPrompt {
            id: PromptId::new(),
            text: "Run the second queued Turn".to_owned(),
            skill_invocations: Vec::new(),
        },
        delivery: PromptDelivery::Queue,
    };
    let first = author
        .admit_prompt(session_id, first_request.clone())
        .await
        .expect("admit first queued Prompt");
    let cancelled = author
        .admit_prompt(session_id, cancelled_request)
        .await
        .expect("admit cancellable queued Prompt");
    let second = author
        .admit_prompt(session_id, second_request)
        .await
        .expect("admit second queued Prompt");
    assert_eq!(first.status, PromptStatus::Pending);
    assert_eq!(cancelled.status, PromptStatus::Pending);
    assert_eq!(second.status, PromptStatus::Pending);
    assert!(first.admission_order < cancelled.admission_order);
    assert!(cancelled.admission_order < second.admission_order);

    let cancelled = observer
        .cancel_prompt(session_id, cancelled.id)
        .await
        .expect("cancel queued Prompt from another client");
    assert_eq!(cancelled.status, PromptStatus::Cancelled);
    let retried = author
        .admit_prompt(session_id, first_request.clone())
        .await
        .expect("retry identical queued admission");
    assert_eq!(retried.status, PromptStatus::Pending);

    let pending = observer
        .read_session(session_id)
        .await
        .expect("read pending queue from observer");
    assert_eq!(pending.session.status, SessionStatus::Active);
    assert_eq!(pending.turns.len(), 1);
    assert_eq!(pending.messages.len(), 1);
    assert_eq!(
        pending
            .prompts
            .iter()
            .map(|prompt| prompt.status)
            .collect::<Vec<_>>(),
        [
            PromptStatus::Delivered,
            PromptStatus::Pending,
            PromptStatus::Cancelled,
            PromptStatus::Pending,
        ]
    );
    assert_eq!(
        codex
            .requests()
            .iter()
            .filter(|request| request["method"] == "turn/start")
            .count(),
        1
    );

    codex.release_turn(1);
    codex.wait_for_method_count("turn/start", 2).await;
    let first_queued_turn = wait_for_session_snapshot(
        &observer,
        &mut feed,
        session_id,
        "first queued Prompt begins after the first terminal boundary",
        |snapshot| snapshot.turns.len() == 2 && snapshot.turns[1].status == TurnStatus::Active,
    )
    .await;
    assert_eq!(first_queued_turn.session.status, SessionStatus::Active);
    assert_eq!(first_queued_turn.turns[0].status, TurnStatus::Completed);
    assert_eq!(first_queued_turn.turns[1].prompt_id, Some(first.id));
    assert_eq!(first_queued_turn.messages.len(), 2);
    assert_eq!(first_queued_turn.messages[1].content, first.text);
    assert_eq!(
        first_queued_turn
            .prompts
            .iter()
            .find(|prompt| prompt.id == second.id)
            .expect("second queued Prompt remains authoritative")
            .status,
        PromptStatus::Pending
    );

    let delivered_retry = author
        .admit_prompt(session_id, first_request)
        .await
        .expect("retry delivered queued admission");
    assert_eq!(delivered_retry.status, PromptStatus::Delivered);

    codex.release_turn(2);
    codex.wait_for_method_count("turn/start", 3).await;
    let second_queued_turn = wait_for_session_snapshot(
        &observer,
        &mut feed,
        session_id,
        "second queued Prompt begins after the second terminal boundary",
        |snapshot| snapshot.turns.len() == 3 && snapshot.turns[2].status == TurnStatus::Active,
    )
    .await;
    assert_eq!(second_queued_turn.session.status, SessionStatus::Active);
    assert_eq!(second_queued_turn.turns[1].status, TurnStatus::Completed);
    assert_eq!(second_queued_turn.turns[2].prompt_id, Some(second.id));
    assert_eq!(second_queued_turn.messages.len(), 3);
    assert_eq!(second_queued_turn.messages[2].content, second.text);

    codex.release_turn(3);
    let completed = wait_for_session_snapshot(
        &observer,
        &mut feed,
        session_id,
        "the final queued Turn reaches idle",
        |snapshot| snapshot.turns[2].status == TurnStatus::Completed,
    )
    .await;
    assert_eq!(completed.session.status, SessionStatus::Idle);
    assert_eq!(completed.turns.len(), 3);
    assert_eq!(completed.messages.len(), 3);
    assert_eq!(
        completed
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::User)
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        [
            "Run the initial Turn",
            "Run the first queued Turn",
            "Run the second queued Turn",
        ]
    );
    let requests = codex.requests();
    let turn_starts = requests
        .iter()
        .filter(|request| request["method"] == "turn/start")
        .collect::<Vec<_>>();
    assert_eq!(turn_starts.len(), 3);
    assert_eq!(
        turn_starts
            .iter()
            .map(|request| request["params"]["input"][0]["text"]
                .as_str()
                .expect("turn/start contains text input"))
            .collect::<Vec<_>>(),
        [
            "Run the initial Turn",
            "Run the first queued Turn",
            "Run the second queued Turn",
        ]
    );
    assert!(
        requests
            .iter()
            .all(|request| request["method"] != "turn/queue"),
        "Suru must not submit Prompts to Codex's queue API"
    );

    drop(feed);
    drop(observer);
    drop(author);
    server.shutdown().await.expect("shut down server");
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
            skill_invocations: Vec::new(),
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
                    skill_invocations: Vec::new(),
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
                    skill_invocations: Vec::new(),
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
async fn scripted_codex_handles_pending_steers_before_starting_the_queued_turn() {
    let mut fixture = SteeringFixture::start(
        &prompt_operation_script(TERMINAL_BOUNDARY_TURNS, UNEXPECTED_STEER),
        "codex-terminal-steer-priority",
    )
    .await;
    let queued = fixture
        .client
        .admit_prompt(
            fixture.session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Run after the terminal boundary".to_owned(),
                    skill_invocations: Vec::new(),
                },
                delivery: PromptDelivery::Queue,
            },
        )
        .await
        .expect("admit queued Prompt before the boundary steer");
    let steer = fixture
        .client
        .admit_prompt(
            fixture.session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Apply this steer before continuing".to_owned(),
                    skill_invocations: Vec::new(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("admit steer while native completion is pending");
    assert!(queued.admission_order < steer.admission_order);
    assert_eq!(queued.status, PromptStatus::Pending);
    assert_eq!(steer.status, PromptStatus::Pending);

    fixture.codex.release();
    fixture.codex.wait_for_method_count("turn/start", 2).await;

    let continued = fixture
        .wait_for(
            "steer is reconciled before the queued Turn begins",
            |snapshot| snapshot.turns.len() == 2 && snapshot.turns[1].status == TurnStatus::Active,
        )
        .await;
    assert_eq!(continued.session.status, SessionStatus::Active);
    assert_eq!(continued.turns[0].status, TurnStatus::Completed);
    assert_eq!(continued.turns[1].prompt_id, Some(queued.id));
    assert_eq!(continued.messages.len(), 3);
    assert_eq!(continued.messages[0].content, "Begin the steering fixture");
    assert_eq!(continued.messages[1].content, steer.text);
    assert_eq!(continued.messages[1].turn_id, continued.turns[0].id);
    assert_eq!(continued.messages[2].content, queued.text);
    assert_eq!(continued.messages[2].turn_id, continued.turns[1].id);
    assert_eq!(
        continued
            .prompts
            .iter()
            .find(|prompt| prompt.id == steer.id)
            .expect("boundary steer remains authoritative")
            .status,
        PromptStatus::Delivered
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
    let turn_starts = fixture
        .codex
        .requests()
        .into_iter()
        .filter(|request| request["method"] == "turn/start")
        .collect::<Vec<_>>();
    assert_eq!(turn_starts.len(), 2);
    assert_eq!(
        turn_starts[1]["params"]["input"],
        serde_json::json!([{ "type": "text", "text": "Run after the terminal boundary" }])
    );

    fixture.codex.release_turn(2);
    let completed = fixture
        .wait_for("queued Turn reaches its terminal boundary", |snapshot| {
            snapshot.turns[1].status == TurnStatus::Completed
        })
        .await;
    assert_eq!(completed.session.status, SessionStatus::Idle);

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
                    skill_invocations: Vec::new(),
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
    let expected_error = match outcome {
        TerminalSteerOutcome::Accepted => None,
        TerminalSteerOutcome::Rejected { error } => Some(error),
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
        PromptStatus::Delivered
    );
    assert_eq!(completed.messages.len(), 2);
    assert_eq!(
        completed
            .messages
            .iter()
            .filter(|message| message.content == "Race the terminal boundary")
            .count(),
        1
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

async fn wait_for_session_snapshot(
    client: &ManagedClient,
    feed: &mut SessionSubscription,
    session_id: SessionId,
    description: &str,
    predicate: impl Fn(&SessionSnapshot) -> bool,
) -> SessionSnapshot {
    timeout(PROGRESS_DEADLINE, async {
        let mut observed_revision = 0;
        loop {
            let snapshot = client
                .read_session(session_id)
                .await
                .expect("read observed Session");
            if predicate(&snapshot) && observed_revision >= snapshot.revision.0 {
                return snapshot;
            }
            observed_revision = match feed
                .next()
                .await
                .expect("Session feed remains open")
                .expect("Session update is valid")
            {
                SessionEvent::Snapshot(snapshot) => snapshot.revision.0,
                SessionEvent::Updated(update) => update.revision.0,
            };
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{description}"))
}
