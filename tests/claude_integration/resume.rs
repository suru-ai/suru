//! A Claude conversation outliving the child that opened it.
//!
//! The provider-session identifier Suru mints persists as the Session's Resume State, and every
//! child after the first asks the CLI to resume the conversation filed under it: the child a
//! replacement server spawns after a restart, and the child an Agent Selection change respawns —
//! the Model and its Options being spawn-time flags, a change between Turns is a new process on
//! the same conversation. A resume the CLI cannot honor fails the Turn rather than quietly opening
//! an empty conversation under the same identifier.

use crate::support::{
    ScriptedClaude, agent_messages, conversation_arms, conversation_fixture, flag_value, hosting,
    rejecting_resume_preamble, settled_session,
};
use suru::{
    managed_client::ManagedClient,
    protocol::{
        AdmitPromptRequest, AgentSelection, AgentSelectionOperationId, CreateSessionRequest,
        InitialPrompt, ModelId, ModelOptionChoiceId, ModelOptionId, ModelOptionSelection,
        ModelOptionValue, PromptDelivery, PromptId, ProviderId, SessionId, SessionSnapshot,
        TurnStatus, UpdateAgentSelectionRequest,
    },
    server::RunningServer,
};

/// One agent Message streamed and the terminal result that Settles the Turn — enough for a Turn to
/// run on either side of a restart or a respawn.
const STREAMED_MESSAGE: &str = r#"      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Still here"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":5,"num_turns":1,"result":"Still here","terminal_reason":"completed","session_id":"prov-session"}'
"#;

/// A fixture that answers the Session's discoveries and Turns, but refuses every resume — a CLI
/// that has lost the conversation Suru comes back for.
fn forgetful_fixture() -> ScriptedClaude {
    ScriptedClaude::with_preamble(
        &rejecting_resume_preamble(),
        &conversation_arms(STREAMED_MESSAGE),
    )
}

fn selection(model: &str, effort: &str) -> AgentSelection {
    AgentSelection {
        provider: ProviderId::new("claude"),
        model: ModelId::new(model),
        options: vec![ModelOptionSelection {
            id: ModelOptionId::new("reasoning_effort"),
            value: ModelOptionValue::Select {
                choice: ModelOptionChoiceId::new(effort),
            },
        }],
    }
}

/// A Session past its first Turn, holding everything it runs on for as long as the test does —
/// the state directory and the Workspace included, which are only alive while this is.
struct DurableSession {
    state_dir: tempfile::TempDir,
    _workspace: tempfile::TempDir,
    channel: &'static str,
    server: RunningServer,
    client: ManagedClient,
    session_id: SessionId,
}

impl DurableSession {
    /// Opens a Session on `claude` under `channel` and runs its first Turn to completion.
    /// `channel` is the client channel, so each test needs its own.
    async fn start(claude: &ScriptedClaude, channel: &'static str) -> Self {
        let state_dir = tempfile::tempdir().expect("create isolated state directory");
        let workspace = tempfile::tempdir().expect("create valid Workspace");
        let execution_directory = workspace.path().join("packages/nested agent directory");
        std::fs::create_dir_all(&execution_directory).unwrap();
        let execution_directory = std::fs::canonicalize(execution_directory).unwrap();
        let (server, client) = hosting(claude, channel, state_dir.path()).await;
        let created = client
            .create_session(CreateSessionRequest {
                agent_selection: None,
                execution_directory: suru::protocol::ExecutionDirectory {
                    path: execution_directory.clone(),
                },
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Open a durable Claude conversation".to_owned(),
                    skill_invocations: Vec::new(),
                },
            })
            .await
            .expect("create Session");
        let session_id = created.session.id;
        let settled = settled_session(&client, session_id, 0).await;
        assert_eq!(
            settled.turns[0].status,
            TurnStatus::Completed,
            "the Session's first Turn runs before anything asks it to carry on: {:?}",
            settled.activities
        );
        Self {
            state_dir,
            _workspace: workspace,
            channel,
            server,
            client,
            session_id,
        }
    }

    /// The same Suru Session on a replacement server, reached the way a user restarting Suru
    /// reaches it. Every process the original server launched is gone before the replacement asks
    /// for the conversation back, so only a child the replacement spawned itself can serve it.
    async fn restarted(self, claude: &ScriptedClaude) -> Self {
        let Self {
            state_dir,
            _workspace,
            channel,
            server,
            client,
            session_id,
        } = self;
        drop(client);
        server.shutdown().await.expect("stop the original server");
        claude.wait_for_exits(claude.launches()).await;

        let (server, client) = hosting(claude, channel, state_dir.path()).await;
        Self {
            state_dir,
            _workspace,
            channel,
            server,
            client,
            session_id,
        }
    }

    /// Puts `selection` in force on the Session, for a Turn to run under next.
    async fn select(&self, selection: AgentSelection) {
        // The Agent Selection is normalized against the catalog, so discover it before naming one.
        self.client
            .list_models()
            .await
            .expect("discover Claude Models");
        self.client
            .update_agent_selection(
                self.session_id,
                UpdateAgentSelectionRequest {
                    operation_id: AgentSelectionOperationId::new(),
                    selection: selection.clone(),
                },
            )
            .await
            .expect("choose another Agent Selection between Turns");
    }

    /// Delivers `prompt` to the idle Session and comes back once the Turn it began settles.
    async fn continue_with(&self, prompt: &str, turn_index: usize) -> SessionSnapshot {
        self.client
            .admit_prompt(
                self.session_id,
                AdmitPromptRequest {
                    prompt: InitialPrompt {
                        id: PromptId::new(),
                        text: prompt.to_owned(),
                        skill_invocations: Vec::new(),
                    },
                    delivery: PromptDelivery::Steer,
                },
            )
            .await
            .expect("admit a Prompt to the Session");
        settled_session(&self.client, self.session_id, turn_index).await
    }

    async fn shutdown(self) {
        let Self { server, client, .. } = self;
        drop(client);
        server.shutdown().await.expect("shut the server down");
    }
}

/// How each Session child the fixture launched addressed the provider session: the flag naming the
/// conversation and the identifier it named. A Model discovery names no conversation and so is not
/// one of these.
fn conversation_launches(claude: &ScriptedClaude) -> Vec<(String, String)> {
    claude
        .launch_arguments()
        .into_iter()
        .filter_map(|arguments| {
            let flag = ["--session-id", "--resume"]
                .into_iter()
                .find(|flag| arguments.iter().any(|argument| argument == flag))?;
            Some((flag.to_owned(), flag_value(&arguments, flag)))
        })
        .collect()
}

/// Asserts the fixture ran exactly two Session children, the second continuing the conversation
/// the first opened — what both a restart and a Selection change come to. `why` names which.
fn assert_second_child_resumed_the_first(claude: &ScriptedClaude, why: &str) {
    let launches = conversation_launches(claude);
    let [(minted, opened), (resumed, continued)] = launches.as_slice() else {
        panic!("{why} runs a second child beside the first: {launches:?}");
    };
    assert_eq!(minted, "--session-id");
    assert_eq!(resumed, "--resume");
    assert_eq!(
        opened, continued,
        "the child {why} spawned continues the conversation the first one opened"
    );
}

#[tokio::test]
async fn a_session_reopened_after_a_restart_resumes_the_conversation_it_opened() {
    let claude = conversation_fixture(STREAMED_MESSAGE);
    let restored = DurableSession::start(&claude, "claude-restart-resume")
        .await
        .restarted(&claude)
        .await;

    let settled = restored.continue_with("Carry on where we stopped", 1).await;

    assert_eq!(
        settled.turns[1].status,
        TurnStatus::Completed,
        "the Turn after the restart runs on the resumed conversation: {:?}",
        settled.activities
    );
    assert_eq!(
        agent_messages(&settled).len(),
        2,
        "the restored Transcript keeps its first agent Message and gains the second"
    );

    assert_second_child_resumed_the_first(&claude, "the restart");
    let expected = std::fs::canonicalize(
        restored
            ._workspace
            .path()
            .join("packages/nested agent directory"),
    )
    .unwrap();
    for flag in ["--session-id", "--resume"] {
        assert_eq!(claude.launch_carrying(flag).working_directory, expected);
    }

    restored.shutdown().await;
}

#[tokio::test]
async fn a_resume_the_cli_rejects_fails_the_turn_rather_than_opening_a_new_conversation() {
    let claude = forgetful_fixture();
    let restored = DurableSession::start(&claude, "claude-rejected-resume")
        .await
        .restarted(&claude)
        .await;

    let settled = restored
        .continue_with("Carry on a conversation Claude has lost", 1)
        .await;

    assert_eq!(
        settled.turns[1].status,
        TurnStatus::Failed,
        "a conversation the CLI cannot resume fails the Turn rather than starting over"
    );
    assert_eq!(
        agent_messages(&settled).len(),
        1,
        "the Transcript the Session was restored with stays readable"
    );
    let failure = settled
        .activities
        .iter()
        .map(|activity| format!("{activity:?}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        failure.contains("No conversation found with session ID"),
        "the failure carries the CLI's own account of the refused resume: {failure}"
    );
    assert_eq!(
        conversation_launches(&claude)
            .iter()
            .filter(|(flag, _)| flag == "--session-id")
            .count(),
        1,
        "no second conversation is minted for a Session that already has one"
    );

    restored.shutdown().await;
}

#[tokio::test]
async fn changing_the_model_between_turns_respawns_the_child_on_the_same_conversation() {
    let claude = conversation_fixture(STREAMED_MESSAGE);
    let session = DurableSession::start(&claude, "claude-model-respawn").await;

    session.select(selection("middling", "low")).await;
    let settled = session.continue_with("Think less this time", 1).await;

    assert_eq!(
        settled.turns[1].status,
        TurnStatus::Completed,
        "the Turn after the change runs on the respawned child: {:?}",
        settled.activities
    );
    assert_eq!(
        settled.session.agent_selection,
        Some(selection("middling", "low")),
        "the Session keeps the Selection its latest Turn ran under"
    );

    assert_second_child_resumed_the_first(&claude, "the Model change");
    assert_eq!(
        claude.argument_value("--model"),
        "middling",
        "the next Turn runs under the Model that was chosen"
    );
    assert_eq!(
        claude.argument_value("--effort"),
        "low",
        "the next Turn runs under the reasoning effort that was chosen"
    );
    // The flags are the process's, so the child that carried the old ones is gone rather than
    // left running beside its replacement.
    claude.wait_for_exits(claude.launches() - 1).await;

    session.shutdown().await;
}

#[tokio::test]
async fn changing_only_a_model_option_between_turns_respawns_the_child_just_the_same() {
    let claude = conversation_fixture(STREAMED_MESSAGE);
    let session = DurableSession::start(&claude, "claude-option-respawn").await;

    // The Session's first Turn ran under the catalog default's own effort, so lowering it alone
    // leaves the Model where it was — and a Model Option is a spawn-time flag just the same.
    session.select(selection("default", "low")).await;
    let settled = session.continue_with("Same Model, less thinking", 1).await;

    assert_eq!(
        settled.turns[1].status,
        TurnStatus::Completed,
        "the Turn after the change runs on the respawned child: {:?}",
        settled.activities
    );
    assert_second_child_resumed_the_first(&claude, "the Model Option change");
    assert_eq!(
        claude.argument_value("--model"),
        "default",
        "the Model the change left alone spawns the replacement too"
    );
    assert_eq!(
        claude.argument_value("--effort"),
        "low",
        "the next Turn runs under the reasoning effort that was chosen"
    );

    session.shutdown().await;
}

#[tokio::test]
async fn an_unchanged_selection_between_turns_keeps_the_child_that_is_running() {
    let claude = conversation_fixture(STREAMED_MESSAGE);
    let session = DurableSession::start(&claude, "claude-selection-unchanged").await;

    let settled = session.continue_with("Same Model, please", 1).await;

    assert_eq!(settled.turns[1].status, TurnStatus::Completed);
    assert_eq!(
        conversation_launches(&claude).len(),
        1,
        "a Turn selected the way the running child was spawned respawns nothing"
    );

    session.shutdown().await;
}
