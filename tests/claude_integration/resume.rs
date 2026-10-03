//! A Claude conversation outliving the child that opened it.
//!
//! The provider-session identifier Suru mints persists as the Session's Resume State, and every
//! child after the first asks the CLI to resume the conversation filed under it: the child a
//! replacement server spawns after a restart, and the child an Agent Selection change respawns —
//! the Model and its Options being spawn-time flags, a change between Turns is a new process on
//! the same conversation. A resume the CLI cannot honor fails the Turn rather than quietly opening
//! an empty conversation under the same identifier.

use crate::support::{
    CLAUDE_MODELS, ScriptedClaude, agent_messages, connect, conversation_arms,
    conversation_fixture, discovery_arms, flag_value, hosting, rejecting_resume_preamble,
    settled_session,
};
use suru::{
    managed_client::ManagedClient,
    protocol::{
        Activity, ActivityStatus, AdmitPromptRequest, AgentSelection, AgentSelectionOperationId,
        ApprovalPosture, ApprovalPostureApplication, ClaudePermissionMode, CreateSessionRequest,
        InitialPrompt, ModelId, ModelOptionChoiceId, ModelOptionId, ModelOptionSelection,
        ModelOptionValue, PromptDelivery, PromptId, ProviderId, SessionId, SessionSnapshot,
        SettingMutation, TurnId, TurnStatus, UpdateAgentSelectionRequest,
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
        let execution_directory = suru::paths::canonical(execution_directory).unwrap();
        let (server, client) = hosting(claude, channel, state_dir.path()).await;
        let created = client
            .create_session(CreateSessionRequest {
                session_id: None,
                preparation_id: None,
                agent_selection: None,
                execution_directory: suru::protocol::ExecutionDirectory {
                    path: execution_directory.clone(),
                },
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Open a durable Claude conversation".to_owned(),
                    skill_invocations: Vec::new(),
                    attachments: Vec::new(),
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
                        attachments: Vec::new(),
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
    let expected = suru::paths::canonical(
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

/// The durable Session's first Turn spawns two background agents, each of which says hello and
/// settles before the loop's result, as the live 2.1.280 CLI reports a spawn.
const SPAWNS_TWO_AGENTS: &str = r#"      emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"agent_1","name":"Agent","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"description\":\"Say hello\",\"prompt\":\"Say HELLO.\",\"run_in_background\":true}"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"agent_2","name":"Agent","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"description\":\"Say hi\",\"prompt\":\"Say HI.\",\"subagent_type\":\"Explore\",\"run_in_background\":true}"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":1},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"a2046dbbe8ecd4a5c","tool_use_id":"agent_1","description":"Say hello","task_type":"local_agent","subagent_type":"general-purpose","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"b3157ecc9f9de5b6d","tool_use_id":"agent_2","description":"Say hi","task_type":"local_agent","subagent_type":"Explore","session_id":"prov-session"}'
      emit '{"type":"assistant","message":{"role":"assistant","model":"claude-haiku-child","content":[{"type":"text","text":"HELLO"}]},"parent_tool_use_id":"agent_1","session_id":"prov-session"}'
      emit '{"type":"assistant","message":{"role":"assistant","model":"claude-haiku-child","content":[{"type":"text","text":"HI"}]},"parent_tool_use_id":"agent_2","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_notification","task_id":"a2046dbbe8ecd4a5c","tool_use_id":"agent_1","status":"completed","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_notification","task_id":"b3157ecc9f9de5b6d","tool_use_id":"agent_2","status":"completed","session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":300,"num_turns":1,"result":"Launched.","session_id":"prov-session"}'
"#;

/// After the restart, the resumed conversation's loop sends both agents more through SendMessage
/// at once, and their answers interleave. The CLI starts each agent's task again naming its
/// SendMessage, and each agent's conversation rides under the Agent tool use that spawned it
/// before the restart — the shape the live CLI reports within one process, assumed to hold for
/// agents a `--resume`d CLI reloads. Nothing in the resumed stretch says which spawn is whose, so
/// only what the Session recorded before the restart can route them.
const RESUMES_BOTH_AGENTS: &str = r#"      emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"send_1","name":"SendMessage","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"to\":\"a2046dbbe8ecd4a5c\",\"message\":\"Now say GOODBYE.\",\"summary\":\"Say goodbye\"}"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"send_2","name":"SendMessage","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"to\":\"b3157ecc9f9de5b6d\",\"message\":\"Now say BYE.\",\"summary\":\"Say bye\"}"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":1},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"a2046dbbe8ecd4a5c","tool_use_id":"send_1","description":"Say hello","task_type":"local_agent","subagent_type":"general-purpose","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"b3157ecc9f9de5b6d","tool_use_id":"send_2","description":"Say hi","task_type":"local_agent","subagent_type":"Explore","session_id":"prov-session"}'
      emit '{"type":"assistant","message":{"role":"assistant","model":"claude-opus-resumed","content":[{"type":"text","text":"BYE"}]},"parent_tool_use_id":"agent_2","session_id":"prov-session"}'
      emit '{"type":"assistant","message":{"role":"assistant","model":"claude-sonnet-resumed","content":[{"type":"text","text":"GOODBYE"}]},"parent_tool_use_id":"agent_1","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_notification","task_id":"b3157ecc9f9de5b6d","tool_use_id":"send_2","status":"completed","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_notification","task_id":"a2046dbbe8ecd4a5c","tool_use_id":"send_1","status":"completed","session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"DONE"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":900,"num_turns":1,"result":"DONE","session_id":"prov-session"}'
"#;

/// A user-message arm that plays `timeline` only for the Prompt reading `prompt`, so one fixture
/// can answer the Turns on either side of a restart differently.
fn prompt_arm(prompt: &str, timeline: &str) -> String {
    format!(
        r#"    *'"type":"user"'*'{prompt}'*)
{timeline}      ;;
"#
    )
}

/// The Subagent rows in `snapshot`, in Transcript order, as the Turn each stands in, the
/// description it reads, and the Session it leads into.
fn subagent_rows(snapshot: &SessionSnapshot) -> Vec<(TurnId, &str, SessionId)> {
    snapshot
        .activities
        .iter()
        .filter_map(|activity| match activity {
            Activity::Subagent {
                turn_id,
                status,
                description,
                session_id,
                ..
            } => {
                assert_eq!(*status, ActivityStatus::Completed, "{description} settles");
                Some((*turn_id, description.as_str(), *session_id))
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn agents_spawned_before_a_restart_resume_in_their_own_sessions_after_it() {
    let claude = ScriptedClaude::new(&format!(
        "{}{}{}",
        discovery_arms(CLAUDE_MODELS),
        prompt_arm("Open a durable Claude conversation", SPAWNS_TWO_AGENTS),
        prompt_arm("Send the agents back in", RESUMES_BOTH_AGENTS),
    ));
    let restored = DurableSession::start(&claude, "claude-restart-subagent-resume")
        .await
        .restarted(&claude)
        .await;

    let parent = restored.continue_with("Send the agents back in", 1).await;

    assert_second_child_resumed_the_first(&claude, "the restart");
    assert_eq!(parent.turns[1].status, TurnStatus::Completed);
    let rows = subagent_rows(&parent);
    let [
        (hello_turn, "Say hello", hello_child),
        (hi_turn, "Say hi", hi_child),
        (goodbye_turn, "Say goodbye", goodbye_child),
        (bye_turn, "Say bye", bye_child),
    ] = rows[..]
    else {
        panic!("each spawn and each resume stands as a row, got {rows:?}");
    };
    assert_eq!([hello_turn, hi_turn], [parent.turns[0].id; 2]);
    assert_eq!(
        [goodbye_turn, bye_turn],
        [parent.turns[1].id; 2],
        "the resume rows stand in the Turn after the restart that delegated them"
    );
    assert_ne!(hello_child, hi_child);
    assert_eq!(
        (goodbye_child, bye_child),
        (hello_child, hi_child),
        "each resume leads into the Session its agent spawned into before the restart"
    );

    for (child, (spawned, spawn_delegation), (resumed, resume_delegation), model) in [
        (
            hello_child,
            ("HELLO", "Say HELLO."),
            ("GOODBYE", "Now say GOODBYE."),
            "claude-sonnet-resumed",
        ),
        (
            hi_child,
            ("HI", "Say HI."),
            ("BYE", "Now say BYE."),
            "claude-opus-resumed",
        ),
    ] {
        let child = settled_session(&restored.client, child, 1).await;
        let [first, second] = child.turns.as_slice() else {
            panic!(
                "the resume begins a second Turn in the agent's own Session, got {:?}",
                child.turns
            );
        };
        assert_eq!(first.status, TurnStatus::Completed);
        assert_eq!(second.status, TurnStatus::Completed);
        assert_eq!(
            second
                .agent
                .as_ref()
                .map(|agent| agent.selection.model.as_str()),
            Some(model),
            "the resumed stretch's Model evidence reaches the Turn it began"
        );
        let messages = agent_messages(&child)
            .into_iter()
            .map(|message| (message.content.as_str(), message.turn_id))
            .collect::<Vec<_>>();
        assert_eq!(
            messages,
            [(spawned, first.id), (resumed, second.id)],
            "the conversation the agent spawned under before the restart still reaches its Session"
        );
        let delegations = child
            .messages
            .iter()
            .filter(|message| message.role.delegator().is_some())
            .map(|message| (message.content.as_str(), message.turn_id))
            .collect::<Vec<_>>();
        assert_eq!(
            delegations,
            [(spawn_delegation, first.id), (resume_delegation, second.id)],
            "the resume after the restart opens its Turn with SendMessage's message"
        );
    }

    restored.shutdown().await;
}

/// A Session nothing has used for a while is evicted from memory with its Claude stopped, as a
/// restart stops it: nothing that Claude could still say has a history left to land on, and nothing
/// told the Session about its posture could reach it. The next Turn resumes the conversation from
/// its Resume State in a fresh child launched under the Settings in force by then, which the
/// Session reads as applied because it was.
#[tokio::test]
async fn an_evicted_session_stops_its_claude_and_resumes_under_the_settings_in_force() {
    let claude = conversation_fixture(STREAMED_MESSAGE);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let channel = "claude-eviction-resume";
    let server = suru::server::spawn_with_providers_and_timings(
        suru::server::ServerConfig::new(state_dir.path(), channel)
            .expect("configure server")
            .with_config_dir(config_dir.path()),
        vec![std::sync::Arc::new(suru::provider::ClaudeRuntime::new(
            claude.executable(),
        ))],
        suru::server::ServerTimings::default().with_session_eviction(
            std::time::Duration::from_millis(200),
            std::time::Duration::from_millis(20),
        ),
    )
    .await
    .expect("spawn server");
    let client = connect(state_dir.path(), channel).await;
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
                text: "Open a conversation to leave idle".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create Session");
    let session_id = created.session.id;
    settled_session(&client, session_id, 0).await;

    // Left unused, the Session is evicted and the Claude serving it stops.
    claude.wait_for_exits(claude.launches()).await;
    client
        .mutate_setting(SettingMutation::ProviderClaudePermissionMode {
            value: Some(ClaudePermissionMode::AcceptEdits),
        })
        .await
        .expect("change the permission mode Setting");

    client
        .admit_prompt(
            session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Carry on after the eviction".to_owned(),
                    skill_invocations: Vec::new(),
                    attachments: Vec::new(),
                },
                delivery: PromptDelivery::Queue,
            },
        )
        .await
        .expect("admit a Prompt to the evicted Session");
    let settled = settled_session(&client, session_id, 1).await;

    assert_eq!(settled.turns[1].status, TurnStatus::Completed);
    assert_eq!(
        agent_messages(&settled).len(),
        2,
        "the history read back keeps the first Turn's Message beside the second's"
    );
    assert_second_child_resumed_the_first(&claude, "the eviction");
    assert_eq!(
        claude
            .launch_carrying("--resume")
            .value("--permission-mode"),
        "acceptEdits",
        "the resumed child runs under the Settings in force when it started"
    );
    let posture = settled
        .session
        .approval_posture
        .expect("the Session has a posture");
    assert_eq!(
        (posture.value, posture.application),
        (
            ApprovalPosture::Claude {
                permission_mode: ClaudePermissionMode::AcceptEdits
            },
            ApprovalPostureApplication::Applied
        )
    );
    drop(client);
    server.shutdown().await.expect("shut the server down");
}
