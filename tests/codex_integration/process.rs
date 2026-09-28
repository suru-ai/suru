//! The Codex process lifecycle: launching the real binary, loss, and resume.

use crate::server_support::PROGRESS_DEADLINE;
use crate::support::{ScriptedCodex, receive_initial_state};
use std::sync::Arc;
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig, SessionSubscription},
    protocol::{
        Activity, AdmitPromptRequest, AgentId, CreateSessionRequest, InitialPrompt, MessageRole,
        MessageStatus, PromptDelivery, PromptId, ProviderId, SessionId, SessionSnapshot,
        SessionStatus, TurnStatus,
    },
    provider::CodexRuntime,
    server::{self, RunningServer, ServerConfig},
};
use tokio::time::{Duration, timeout};

const STARTUP_RETRY: &str = r#"
    *'"method":"initialize"'*)
      if [ "$attempt" -eq 1 ]; then
        printf '%s\n' '{"id":1,"error":{"code":-32000,"message":"fixture startup failed once"}}'
        exit 0
      fi
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"config/read"'*)
      printf '%s\n' '{"id":2,"result":{"config":{},"origins":{}}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":3,"result":{"thread":{"id":"retry-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":4,"result":{"turn":{"id":"retry-turn"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"retry-thread","turn":{"id":"retry-turn","status":"completed","items":[]}}}'
      ;;
"#;

const ACTIVE_PROCESS_LOSS_THEN_RESUME: &str = r#"
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"config/read"'*)
      printf '%s\n' '{"id":2,"result":{"config":{},"origins":{}}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":3,"result":{"thread":{"id":"recoverable-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"thread/resume"'*)
      printf '%s\n' '{"id":3,"result":{"thread":{"id":"recoverable-thread","turns":[{"id":"native-turn-1"}]},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      if [ "$attempt" -eq 1 ]; then
        exit 17
      fi
      printf '%s\n' '{"id":4,"result":{"turn":{"id":"native-turn-2"}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"recoverable-thread","turnId":"native-turn-2","item":{"type":"agentMessage","id":"resumed-message","text":""}}}'
      printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"recoverable-thread","turnId":"native-turn-2","itemId":"resumed-message","delta":"Recovered context"}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"recoverable-thread","turnId":"native-turn-2","item":{"type":"agentMessage","id":"resumed-message","text":"Recovered context"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"recoverable-thread","turn":{"id":"native-turn-2","status":"completed","items":[]}}}'
      ;;
"#;

const ACTIVE_PROCESS_LOSS_THEN_RESUME_REJECTION: &str = r#"
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"config/read"'*)
      printf '%s\n' '{"id":2,"result":{"config":{},"origins":{}}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":3,"result":{"thread":{"id":"rejected-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"thread/resume"'*)
      printf '%s\n' '{"id":3,"error":{"code":-32001,"message":"fixture cannot resume this Thread"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":4,"result":{"turn":{"id":"native-turn-1"}}}'
      exit 17
      ;;
"#;

const IDLE_PROCESS_LOSS_THEN_RESUME: &str = r#"
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"config/read"'*)
      printf '%s\n' '{"id":2,"result":{"config":{},"origins":{}}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":3,"result":{"thread":{"id":"idle-loss-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"thread/resume"'*)
      printf '%s\n' '{"id":3,"result":{"thread":{"id":"idle-loss-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      if [ "$attempt" -eq 1 ]; then
        printf '%s\n' '{"id":4,"result":{"turn":{"id":"idle-turn-1"}}}'
        printf '%s\n' '{"method":"turn/completed","params":{"threadId":"idle-loss-thread","turn":{"id":"idle-turn-1","status":"completed","items":[]}}}'
        printf '%s\n' 'exited' > "$CODEX_FIXTURE_EXITED"
        exit 17
      fi
      printf '%s\n' '{"id":4,"result":{"turn":{"id":"idle-turn-2"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"idle-loss-thread","turn":{"id":"idle-turn-2","status":"completed","items":[]}}}'
      ;;
"#;

#[tokio::test]
#[ignore = "set SURU_CODEX_SMOKE=1 to use the installed authenticated Codex binary"]
async fn installed_codex_launches_runs_one_text_turn_and_shuts_down() {
    if std::env::var_os("SURU_CODEX_SMOKE").as_deref() != Some(std::ffi::OsStr::new("1")) {
        eprintln!("skipping: set SURU_CODEX_SMOKE=1 to opt in");
        return;
    }

    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "codex-installed-smoke").expect("configure server"),
        Arc::new(CodexRuntime::from_environment()),
    )
    .await
    .expect("launch server with installed Codex");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), "codex-installed-smoke")
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
                text: "Reply with a short confirmation that the smoke test completed.".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create live Codex Session");
    let mut feed = client
        .subscribe_session(created.session.id)
        .await
        .expect("subscribe to live Codex Session SSE");

    let completed = timeout(Duration::from_secs(120), async {
        loop {
            let snapshot = client
                .read_session(created.session.id)
                .await
                .expect("read live Codex Session");
            if snapshot.turns.first().is_some_and(|turn| {
                matches!(
                    turn.status,
                    TurnStatus::Completed | TurnStatus::Failed | TurnStatus::Interrupted
                )
            }) {
                return snapshot;
            }
            feed.next()
                .await
                .expect("live Codex Session feed remains open")
                .expect("live Codex Session event is valid");
        }
    })
    .await
    .expect("installed Codex completes one text Turn");

    assert_eq!(completed.turns[0].status, TurnStatus::Completed);
    let selection = completed
        .session
        .agent_selection
        .expect("Codex publishes its effective Model");
    let identity = completed.turns[0]
        .agent
        .as_ref()
        .expect("Codex Turn captures its effective Agent");
    assert_eq!(identity.agent, AgentId::new("codex"));
    assert_eq!(identity.selection, selection);
    assert_eq!(selection.provider, ProviderId::new("codex"));
    assert!(completed.messages.iter().any(|message| {
        message.role == MessageRole::Agent
            && message.status == MessageStatus::Completed
            && !message.content.trim().is_empty()
    }));

    drop(feed);
    drop(client);
    timeout(PROGRESS_DEADLINE, server.shutdown())
        .await
        .expect("live Codex shutdown remains bounded")
        .expect("shut down live Codex server");
}

#[tokio::test]
async fn codex_retries_startup_with_a_fresh_thread_when_no_thread_id_exists() {
    let mut fixture = RecoveryFixture::start(
        STARTUP_RETRY,
        "codex-startup-retry",
        "Fail before a native Thread exists",
    )
    .await;
    fixture.wait_for_turn(0, TurnStatus::Failed).await;
    let recovered = fixture
        .admit_and_wait("Retry startup", 1, TurnStatus::Completed)
        .await;

    assert_eq!(recovered.session.id, fixture.session_id);
    assert_eq!(recovered.session.status, SessionStatus::Idle);
    assert_eq!(recovered.turns.len(), 2);
    assert_eq!(recovered.turns[0].status, TurnStatus::Failed);
    assert_eq!(recovered.turns[1].status, TurnStatus::Completed);
    let methods = fixture.codex.methods();
    assert_eq!(
        methods,
        [
            "initialize",
            "initialize",
            "initialized",
            "config/read",
            "thread/start",
            "turn/start"
        ]
    );
    assert!(!methods.iter().any(|method| method == "thread/resume"));

    fixture.shutdown().await;
}

#[tokio::test]
async fn codex_resumes_the_known_thread_after_active_process_loss() {
    let mut fixture = RecoveryFixture::start(
        ACTIVE_PROCESS_LOSS_THEN_RESUME,
        "codex-process-recovery",
        "Lose the first app-server",
    )
    .await;
    let failed = fixture.wait_for_turn(0, TurnStatus::Failed).await;
    assert_eq!(
        failed
            .activities
            .iter()
            .filter(|activity| matches!(activity, Activity::Error { .. }))
            .count(),
        1
    );
    assert!(failed.activities.iter().any(
        |activity| matches!(activity, Activity::Error { text, .. } if text.contains("exited unexpectedly"))
    ));

    let recovered = fixture
        .admit_and_wait(
            "Continue in the known Codex Thread",
            1,
            TurnStatus::Completed,
        )
        .await;
    assert_eq!(recovered.session.id, fixture.session_id);
    assert_eq!(
        recovered.session.agent_selection,
        failed.session.agent_selection
    );
    assert_eq!(recovered.turns.len(), 2);
    assert_eq!(recovered.messages.len(), 3);
    assert_eq!(
        recovered
            .messages
            .iter()
            .filter(|message| message.content == "Lose the first app-server")
            .count(),
        1,
        "resume must not project native history into the Suru transcript"
    );
    assert_eq!(
        recovered
            .messages
            .last()
            .map(|message| message.content.as_str()),
        Some("Recovered context")
    );
    assert_eq!(recovered.activities.len(), 1);

    let requests = fixture.codex.requests();
    assert_eq!(
        fixture.codex.methods(),
        [
            "initialize",
            "initialized",
            "config/read",
            "thread/start",
            "turn/start",
            "initialize",
            "initialized",
            "config/read",
            "thread/resume",
            "turn/start"
        ]
    );
    let resume = requests
        .iter()
        .find(|request| request["method"] == "thread/resume")
        .expect("second app-server resumes the known Thread");
    assert_eq!(resume["params"]["threadId"], "recoverable-thread");
    assert_eq!(
        resume["params"]["cwd"],
        suru::paths::canonical(fixture.workspace.path())
            .expect("canonicalize Workspace")
            .to_string_lossy()
            .as_ref()
    );
    assert_eq!(resume["params"]["approvalPolicy"], "on-request");
    assert_eq!(resume["params"]["sandbox"], "workspace-write");
    assert_eq!(
        requests.last().expect("second Turn request")["params"]["threadId"],
        "recoverable-thread"
    );

    fixture.shutdown().await;
}

#[tokio::test]
async fn codex_surfaces_resume_rejection_without_starting_an_unrelated_thread() {
    let mut fixture = RecoveryFixture::start(
        ACTIVE_PROCESS_LOSS_THEN_RESUME_REJECTION,
        "codex-resume-rejection",
        "Lose the app-server",
    )
    .await;
    fixture.wait_for_turn(0, TurnStatus::Failed).await;
    let rejected = fixture
        .admit_and_wait("Attempt the required resume", 1, TurnStatus::Failed)
        .await;

    assert_eq!(rejected.session.id, fixture.session_id);
    assert_eq!(rejected.session.status, SessionStatus::Idle);
    assert_eq!(rejected.activities.len(), 2);
    assert!(matches!(
        &rejected.activities[1],
        Activity::Error { text, .. } if text.contains("fixture cannot resume this Thread")
    ));
    let methods = fixture.codex.methods();
    assert_eq!(
        methods,
        [
            "initialize",
            "initialized",
            "config/read",
            "thread/start",
            "turn/start",
            "initialize",
            "initialized",
            "config/read",
            "thread/resume"
        ]
    );
    assert_eq!(
        methods
            .iter()
            .filter(|method| **method == "thread/start")
            .count(),
        1,
        "resume rejection must not fall back to a new native Thread"
    );

    fixture.shutdown().await;
}

#[tokio::test]
async fn codex_resumes_on_the_first_prompt_after_process_loss_while_idle() {
    let mut fixture = RecoveryFixture::start(
        IDLE_PROCESS_LOSS_THEN_RESUME,
        "codex-idle-process-recovery",
        "Complete before the app-server exits",
    )
    .await;
    fixture.wait_for_turn(0, TurnStatus::Completed).await;
    fixture.codex.wait_for_exit().await;
    let resumed = fixture
        .admit_and_wait(
            "Resume immediately after idle loss",
            1,
            TurnStatus::Completed,
        )
        .await;

    assert_eq!(resumed.turns.len(), 2);
    assert_eq!(
        fixture.codex.methods(),
        [
            "initialize",
            "initialized",
            "config/read",
            "thread/start",
            "turn/start",
            "initialize",
            "initialized",
            "config/read",
            "thread/resume",
            "turn/start"
        ]
    );

    fixture.shutdown().await;
}

struct RecoveryFixture {
    codex: ScriptedCodex,
    _state_dir: tempfile::TempDir,
    workspace: tempfile::TempDir,
    server: RunningServer,
    client: ManagedClient,
    feed: SessionSubscription,
    session_id: SessionId,
}

impl RecoveryFixture {
    async fn start(script: &str, channel: &str, initial_prompt: &str) -> Self {
        let codex = ScriptedCodex::new_multiprocess(script);
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
                    text: initial_prompt.to_owned(),
                    skill_invocations: Vec::new(),
                    attachments: Vec::new(),
                },
            })
            .await
            .expect("create Session");
        let feed = client
            .subscribe_session(created.session.id)
            .await
            .expect("subscribe to Session SSE");
        Self {
            codex,
            _state_dir: state_dir,
            workspace,
            server,
            client,
            feed,
            session_id: created.session.id,
        }
    }

    async fn wait_for_turn(&mut self, turn_index: usize, expected: TurnStatus) -> SessionSnapshot {
        timeout(PROGRESS_DEADLINE, async {
            loop {
                let snapshot = self
                    .client
                    .read_session(self.session_id)
                    .await
                    .expect("read Session while waiting for Provider outcome");
                if snapshot
                    .turns
                    .get(turn_index)
                    .is_some_and(|turn| turn.status == expected)
                {
                    return snapshot;
                }
                self.feed
                    .next()
                    .await
                    .expect("Session feed remains open")
                    .expect("Session event is valid");
            }
        })
        .await
        .unwrap_or_else(|_| panic!("Turn {turn_index} reaches {expected:?}"))
    }

    async fn admit_and_wait(
        &mut self,
        prompt: &str,
        turn_index: usize,
        expected: TurnStatus,
    ) -> SessionSnapshot {
        self.client
            .admit_prompt(
                self.session_id,
                AdmitPromptRequest {
                    prompt: InitialPrompt {
                        id: PromptId::new(),
                        text: prompt.to_owned(),
                        skill_invocations: Vec::new(),
                        attachments: Vec::new(),
                    },
                    delivery: PromptDelivery::Steer,
                },
            )
            .await
            .expect("admit recovery Prompt");
        self.wait_for_turn(turn_index, expected).await
    }

    async fn shutdown(self) {
        drop(self.feed);
        drop(self.client);
        self.server.shutdown().await.expect("shut down server");
    }
}

#[tokio::test]
async fn managed_worktree_native_codex_starts_at_prepared_root() {
    let codex = ScriptedCodex::new_multiprocess(
        r#"
    *'"method":"initialize"'*) printf '%s\n' '{"id":1,"result":{}}' ;;
    *'"method":"config/read"'*) printf '%s\n' '{"id":2,"result":{"config":{},"origins":{}}}' ;;
    *'"method":"skills/list"'*)
      cwd=$(printf '%s' "$line" | sed 's/.*"cwds":\[\(.*\)\],"forceReload".*/\1/')
      printf '%s\n' '{"id":2,"result":{"data":[{"cwd":'"$cwd"',"skills":[],"errors":[]}]}}'
      ;;
    *'"method":"thread/start"'*|*'"method":"thread/resume"'*) printf '%s\n' '{"id":3,"result":{"thread":{"id":"prepared"},"model":"gpt-fixture"}}' ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":4,"result":{"turn":{"id":"prepared-turn"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"prepared","turn":{"id":"prepared-turn","status":"completed","items":[]}}}'
      ;;
    "#,
    );
    let state = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    let server = server::spawn_with_provider_and_timings(
        ServerConfig::new(state.path(), "prepared-codex").unwrap(),
        Arc::new(CodexRuntime::new(codex.executable())),
        server::ServerTimings::default().with_checkout_skill_timeout(Duration::from_secs(2)),
    )
    .await
    .unwrap();
    let client =
        ManagedClient::connect(ManagedClientConfig::new(state.path(), "prepared-codex").unwrap())
            .await
            .unwrap();
    let prepared = crate::managed_worktree::prepare(&client, source.path(), "codex").await;
    assert!(
        !codex
            .requests()
            .iter()
            .any(|r| r["method"] == "thread/start")
    );
    let created = client
        .create_session(crate::managed_worktree::creation(&prepared))
        .await
        .unwrap();
    codex.wait_for_method_count("turn/start", 1).await;
    let request = codex
        .requests()
        .into_iter()
        .find(|r| r["method"] == "thread/start")
        .unwrap();
    assert_eq!(
        request["params"]["cwd"].as_str(),
        prepared.destination.path.to_str()
    );
    crate::managed_worktree::recover(&client, created.session.id, &prepared.destination.path).await;
    let resumed = codex
        .requests()
        .into_iter()
        .find(|r| r["method"] == "thread/resume")
        .expect("recovered native connection resumes");
    assert_eq!(
        resumed["params"]["cwd"].as_str(),
        prepared.destination.path.to_str()
    );
    assert_eq!(resumed["params"]["threadId"], "prepared");
    server.shutdown().await.unwrap();
}
