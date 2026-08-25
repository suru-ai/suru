//! Claude Turns: a delivered Prompt streaming an agent Message into the Transcript, the Turn
//! settling on the CLI's terminal result, the spawn that carries the Agent Selection, and the
//! Session shutting its child down cleanly.

use std::sync::Arc;

use crate::support::{
    ScriptedClaude, agent_messages, connect, conversation_fixture, probe_arms, settled_session,
    silent_user_turn_arm,
};
use serde_json::Value;
use suru::{
    protocol::{
        Activity, AgentSelection, CreateSessionRequest, InitialPrompt, MessageStatus, ModelId,
        ModelOptionChoiceId, ModelOptionId, ModelOptionSelection, ModelOptionValue, PromptId,
        PromptStatus, ProviderId, TurnStatus, Workspace,
    },
    provider::ClaudeRuntime,
    server::{self, ServerConfig},
};

/// One agent Message arriving as the CLI streams it in partial-message chunks, then the terminal
/// result settling the Turn as completed. The surrounding traffic mirrors a live 2.1.237 CLI:
/// system messages and a `rate_limit_event` around the stream, a full `assistant` snapshot of the
/// Message beside the chunks, and a `message_delta` whose delta object carries no `type` at all.
const STREAMED_MESSAGE: &str = r#"      emit '{"type":"system","subtype":"init","session_id":"prov-session","model":"claude-fixture-1"}'
      emit '{"type":"system","subtype":"status","status":null,"session_id":"prov-session"}'
      emit '{"type":"rate_limit_event","rate_limit":{"status":"allowed"},"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" from Claude"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Hello from Claude"}]},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":4}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":1200,"num_turns":1,"result":"Hello from Claude","session_id":"prov-session"}'
"#;

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

#[tokio::test]
async fn a_prompt_streams_a_claude_message_into_the_transcript_and_settles_the_turn() {
    let claude = conversation_fixture(STREAMED_MESSAGE);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "claude-streaming-turn").expect("configure server"),
        Arc::new(ClaudeRuntime::new(claude.executable())),
    )
    .await
    .expect("spawn server");
    let client = connect(state_dir.path(), "claude-streaming-turn").await;

    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Say hello".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session without waiting for Claude startup");

    let settled = settled_session(&client, created.session.id, 0).await;

    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    assert!(
        settled.turns[0].started_at.is_some() && settled.turns[0].settled_at.is_some(),
        "the Turn records its start and Settle so Turn Folds and timings work"
    );
    assert_eq!(settled.prompts[0].status, PromptStatus::Delivered);
    assert!(
        settled.activities.is_empty(),
        "a Turn that ran cleanly leaves no Error Activity: {:?}",
        settled.activities
    );
    let agent = agent_messages(&settled);
    let [message] = agent.as_slice() else {
        panic!(
            "the Turn produced one agent Message, got {:?}",
            settled.messages
        );
    };
    assert_eq!(message.status, MessageStatus::Completed);
    assert_eq!(message.content, "Hello from Claude");
    assert_eq!(
        settled.session.agent_selection,
        Some(selection("default", "high")),
        "with nothing chosen, the Session runs under the catalog default"
    );

    let arguments = claude.arguments();
    assert_eq!(
        arguments[..8],
        [
            "--print",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--setting-sources",
            // The empty settings-sources value is invisible to the fixture's argv capture.
            "--include-partial-messages",
        ],
        "the Session child runs in stream-json mode with partial messages enabled"
    );
    assert!(
        arguments.contains(&"--dangerously-skip-permissions".to_owned()),
        "the child is launched full-auto, got: {arguments:?}"
    );
    let flag_value = |flag: &str| {
        let position = arguments
            .iter()
            .position(|argument| argument == flag)
            .unwrap_or_else(|| panic!("the child is launched with {flag}, got: {arguments:?}"));
        arguments[position + 1].clone()
    };
    uuid::Uuid::parse_str(&flag_value("--session-id"))
        .expect("the provider-session identifier is the UUID Suru minted");
    assert_eq!(
        flag_value("--model"),
        "default",
        "the first Turn spawns under its Agent Selection's Model"
    );
    assert_eq!(
        flag_value("--effort"),
        "high",
        "the first Turn spawns under its Agent Selection's reasoning effort"
    );

    let prompt = claude
        .requests()
        .into_iter()
        .find(|request| request.get("type").and_then(Value::as_str) == Some("user"))
        .expect("the Prompt reaches the CLI as a stream-json user message");
    assert_eq!(
        prompt
            .pointer("/message/content/0/text")
            .and_then(Value::as_str),
        Some("Say hello"),
        "the user message carries the Prompt text: {prompt}"
    );

    server.shutdown().await.expect("shut the server down");
    claude.wait_for_exit().await;
}

#[tokio::test]
async fn a_turn_under_a_chosen_selection_spawns_the_child_with_its_model_and_effort() {
    let claude = conversation_fixture(STREAMED_MESSAGE);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "claude-chosen-selection").expect("configure server"),
        Arc::new(ClaudeRuntime::new(claude.executable())),
    )
    .await
    .expect("spawn server");
    let client = connect(state_dir.path(), "claude-chosen-selection").await;
    // The Agent Selection is normalized against the catalog, so discover it before naming one.
    client.list_models().await.expect("discover Claude Models");

    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: Some(selection("middling", "low")),
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Think less".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session under a chosen Agent Selection");

    let settled = settled_session(&client, created.session.id, 0).await;
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    assert_eq!(
        settled.session.agent_selection,
        Some(selection("middling", "low")),
        "the Session keeps the Selection its Turn ran under"
    );

    // The conversation child is read by the conversation it was given, because it is not the only
    // process this Session launched: a Title Errand runs its own one-shot beside it, at a Model of
    // the Provider's own choosing.
    let child = claude.launch_carrying("--session-id");
    assert_eq!(child.value("--model"), "middling");
    assert_eq!(child.value("--effort"), "low");

    server.shutdown().await.expect("shut the server down");
    claude.wait_for_exit().await;
}

/// The CLI fails the Turn after part of the Message has streamed, with an internal diagnostic
/// entry ahead of the user-facing error.
const FAILED_TURN: &str = r#"      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Working"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"result","subtype":"error_during_execution","is_error":true,"duration_ms":42,"num_turns":1,"errors":["[ede_diagnostic] internal telemetry","Usage limit reached for this account."],"session_id":"prov-session"}'
"#;

#[tokio::test]
async fn a_failed_result_settles_the_turn_as_failed_with_the_clis_own_reason() {
    let claude = conversation_fixture(FAILED_TURN);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "claude-failed-result").expect("configure server"),
        Arc::new(ClaudeRuntime::new(claude.executable())),
    )
    .await
    .expect("spawn server");
    let client = connect(state_dir.path(), "claude-failed-result").await;

    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Spend the quota".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session");

    let settled = settled_session(&client, created.session.id, 0).await;

    assert_eq!(settled.turns[0].status, TurnStatus::Failed);
    let [Activity::Error { text, .. }] = settled.activities.as_slice() else {
        panic!(
            "a failed result settles the Turn with an Error Activity, got {:?}",
            settled.activities
        );
    };
    assert_eq!(
        text, "Claude Turn failed: Usage limit reached for this account.",
        "the failure carries the CLI's user-facing error, never its internal diagnostics"
    );
    assert_eq!(
        agent_messages(&settled)[0].content,
        "Working",
        "what the CLI did stream before failing stays in the Transcript"
    );

    server.shutdown().await.expect("shut the server down");
    claude.wait_for_exit().await;
}

/// An errored `success` result: the loop finished, but what it finished with is a failure whose
/// account is the result text itself.
const ERRORED_SUCCESS: &str = r#"      emit '{"type":"result","subtype":"success","is_error":true,"duration_ms":42,"num_turns":1,"result":"API Error: 401 invalid api key","session_id":"prov-session"}'
"#;

#[tokio::test]
async fn an_errored_success_result_settles_the_turn_as_failed_with_its_text() {
    let claude = conversation_fixture(ERRORED_SUCCESS);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "claude-errored-success").expect("configure server"),
        Arc::new(ClaudeRuntime::new(claude.executable())),
    )
    .await
    .expect("spawn server");
    let client = connect(state_dir.path(), "claude-errored-success").await;

    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Fail quietly".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session");

    let settled = settled_session(&client, created.session.id, 0).await;

    assert_eq!(settled.turns[0].status, TurnStatus::Failed);
    let [Activity::Error { text, .. }] = settled.activities.as_slice() else {
        panic!(
            "an errored success settles the Turn with an Error Activity, got {:?}",
            settled.activities
        );
    };
    assert_eq!(text, "Claude Turn failed: API Error: 401 invalid api key");

    server.shutdown().await.expect("shut the server down");
    claude.wait_for_exit().await;
}

/// A conversation whose second Prompt is answered like its first, for a Session running more than
/// one Turn on the same child process.
const TWO_TURNS: &str = r#"      turns=$(( ${turns:-0} + 1 ))
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Answer '"$turns"'"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":7,"num_turns":1,"result":"","session_id":"prov-session"}'
"#;

#[tokio::test]
async fn a_second_prompt_runs_its_turn_on_the_same_long_lived_child() {
    let claude = conversation_fixture(TWO_TURNS);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "claude-second-turn").expect("configure server"),
        Arc::new(ClaudeRuntime::new(claude.executable())),
    )
    .await
    .expect("spawn server");
    let client = connect(state_dir.path(), "claude-second-turn").await;

    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "First".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session");
    settled_session(&client, created.session.id, 0).await;

    client
        .admit_prompt(
            created.session.id,
            suru::protocol::AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Second".to_owned(),
                    skill_invocations: Vec::new(),
                },
                delivery: suru::protocol::PromptDelivery::Queue,
            },
        )
        .await
        .expect("admit a second Prompt");
    let settled = settled_session(&client, created.session.id, 1).await;

    assert_eq!(settled.turns[1].status, TurnStatus::Completed);
    assert_eq!(
        agent_messages(&settled)
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        ["Answer 1", "Answer 2"],
        "both Turns stream their own Message"
    );
    assert_eq!(
        claude.launches(),
        3,
        "one probe and one discovery process at startup and one long-lived conversation child, \
         not a spawn per Turn"
    );

    server.shutdown().await.expect("shut the server down");
    claude.wait_for_exit().await;
}

#[tokio::test]
async fn deleting_the_session_terminates_the_child_and_shutdown_stays_clean() {
    let claude = conversation_fixture(STREAMED_MESSAGE);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "claude-session-shutdown").expect("configure server"),
        Arc::new(ClaudeRuntime::new(claude.executable())),
    )
    .await
    .expect("spawn server");
    let client = connect(state_dir.path(), "claude-session-shutdown").await;

    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Say hello".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session");
    settled_session(&client, created.session.id, 0).await;

    client
        .delete_session(created.session.id)
        .await
        .expect("delete the Session");
    // The startup probe's and discovery's processes already exited; the third exit is the Session
    // child's.
    claude.wait_for_exits(3).await;

    // The Session's child is already down, so the server shutdown finds nothing left to stop and
    // stays clean — the Session shutdown path is idempotent.
    server.shutdown().await.expect("shut the server down");
}

/// A conversation child that never answers the Prompt: the process dies mid-Turn instead.
const CRASH_MID_TURN: &str = r#"      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Half a"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      exit 9
"#;

#[tokio::test]
async fn a_child_crash_mid_turn_fails_the_turn_and_keeps_what_streamed() {
    let claude = conversation_fixture(CRASH_MID_TURN);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "claude-crash-mid-turn").expect("configure server"),
        Arc::new(ClaudeRuntime::new(claude.executable())),
    )
    .await
    .expect("spawn server");
    let client = connect(state_dir.path(), "claude-crash-mid-turn").await;

    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Start something the child will not finish".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session");

    let settled = settled_session(&client, created.session.id, 0).await;

    assert_eq!(settled.turns[0].status, TurnStatus::Failed);
    let [Activity::Error { text, .. }] = settled.activities.as_slice() else {
        panic!(
            "a lost child settles the Turn with an Error Activity, got {:?}",
            settled.activities
        );
    };
    assert!(
        text.contains("Claude Code CLI exited unexpectedly"),
        "the failure says the child process is what went, got: {text}"
    );
    assert_eq!(
        agent_messages(&settled)[0].content,
        "Half a",
        "what the lost Turn had already streamed survives it"
    );

    server.shutdown().await.expect("shut the server down");
}

#[tokio::test]
async fn a_prompt_to_a_session_whose_discovery_fails_settles_its_turn_as_failed() {
    // A fixture the probe finds usable but with no list_models arm never answers the startup
    // discovery; the injected control-request timeout is what bounds the wait.
    let claude = ScriptedClaude::new(&format!("{}{}", probe_arms(), silent_user_turn_arm()));
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let runtime = ClaudeRuntime::new(claude.executable())
        .with_control_request_timeout(tokio::time::Duration::from_millis(100))
        .with_process_exit_grace(tokio::time::Duration::from_millis(50));
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "claude-startup-discovery-failure")
            .expect("configure server"),
        Arc::new(runtime),
    )
    .await
    .expect("spawn server");
    let client = connect(state_dir.path(), "claude-startup-discovery-failure").await;

    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Say hello".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session");

    let settled = settled_session(&client, created.session.id, 0).await;
    assert_eq!(settled.turns[0].status, TurnStatus::Failed);
    let [Activity::Error { text, .. }] = settled.activities.as_slice() else {
        panic!(
            "a failed startup settles the Turn with an Error Activity, got {:?}",
            settled.activities
        );
    };
    assert!(
        text.contains("Claude Session startup failed"),
        "the failure names the operation that could not complete, got: {text}"
    );

    server.shutdown().await.expect("shut the server down");
}
