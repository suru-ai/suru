//! The Broker handed to Claude (ADR 0034, docs/validation/0408-claude-http-mcp-long-calls.md).
//!
//! A Session's CLI process is pointed with `--mcp-config` at a file naming the Broker's endpoint as
//! the MCP server `suru`, the bearer token its launch was handed as a header, and a per-server
//! timeout above the longest call the Broker serves; `--allowedTools` allowlists the Broker's Tools
//! so calling one never raises an Approval. The token travels in that owner-only file rather than
//! on the command line, the file is gone once the process it served is, and a relaunch after a
//! restart is handed a token of its own. Every such launch also appends a note to the Agent's
//! system prompt saying the Broker is there, naming its Tools by the full names Claude gives them
//! so the Agent can select them (docs/validation/0408-claude-http-mcp-long-calls.md). With the
//! Broker turned off a launch carries none of it.
//!
//! A Subagent Report reaches a Claude Agent the way a Prompt does: as one stream-json user message
//! on its process's stdin (ADR 0035).

use crate::server_support::broker::McpClient;
use crate::support::{
    CLAUDE_MODELS, Launch, LiveTurn, McpConfigFile, ScriptedClaude, conversation_fixture,
    discovery_arms, hosting, opened_session, settled_session,
};
use serde_json::{Value, json};
use suru::{
    protocol::{
        Activity, AdmitPromptRequest, CreateSessionRequest, InitialPrompt, MessageRole,
        PromptDelivery, PromptId, TurnStatus,
    },
    provider::{ClaudeRuntime, SubagentReport, SubagentReportOutcome},
};

/// One agent Message and the terminal result that Settles the Turn, from a loop the message began
/// at rest: the CLI reports it `queued` and at once `started`, and `completed` after that loop's
/// result (docs/validation/0407-claude-folded-steer.md, case B).
const ANSWERED: &str = r#"      lifecycle "$uuid" queued
      lifecycle "$uuid" started
      emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Done"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":5,"num_turns":1,"result":"Done","terminal_reason":"completed","session_id":"prov-session"}'
      lifecycle "$uuid" completed
"#;

/// A parent whose Agent waits on its Subagent in a Broker call, and a Subagent that answers its
/// Delegation — one fixture for both, since both are launches of the same CLI, told apart by what
/// they are handed. The parent's loop calls `wait_subagents` and holds; the Report that the
/// Subagent's settling writes to the parent's stdin arrives while that call runs, and the loop
/// folds it into its next request once the call answers, so one `result` answers the Prompt and
/// the Report (docs/validation/0421-broker-smoke.md, run 5; 0407-claude-folded-steer.md, case A).
fn waiting_parent_and_answering_child() -> String {
    format!(
        r#"    *'"type":"user"'*'Subagent Report from Suru'*)
      lifecycle "$uuid" queued
      emit '{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_wait","content":[{{"type":"text","text":"{{\"settled\":[{{\"status\":\"completed\"}}]}}"}}],"is_error":false}}]}},"parent_tool_use_id":null,"session_id":"prov-session"}}'
      lifecycle "$uuid" started
      emit '{{"type":"stream_event","event":{{"type":"message_start","message":{{"role":"assistant"}}}},"parent_tool_use_id":null,"session_id":"prov-session"}}'
      emit '{{"type":"stream_event","event":{{"type":"content_block_start","index":0,"content_block":{{"type":"text","text":"DONE Done"}}}},"parent_tool_use_id":null,"session_id":"prov-session"}}'
      emit '{{"type":"stream_event","event":{{"type":"content_block_stop","index":0}},"parent_tool_use_id":null,"session_id":"prov-session"}}'
      lifecycle "$uuid" completed
      emit '{{"type":"result","subtype":"success","is_error":false,"duration_ms":335410,"num_turns":6,"result":"DONE Done","terminal_reason":"completed","session_id":"prov-session"}}'
      lifecycle "$prompt" completed
      ;;
    *'"type":"user"'*'Survey the seams'*)
{ANSWERED}      ;;
    *'"type":"user"'*)
      prompt=$uuid
      lifecycle "$prompt" queued
      lifecycle "$prompt" started
      emit '{{"type":"stream_event","event":{{"type":"message_start","message":{{"role":"assistant"}}}},"parent_tool_use_id":null,"session_id":"prov-session"}}'
      emit '{{"type":"stream_event","event":{{"type":"content_block_start","index":0,"content_block":{{"type":"tool_use","id":"toolu_wait","name":"mcp__suru__wait_subagents","input":{{}}}}}},"parent_tool_use_id":null,"session_id":"prov-session"}}'
      emit '{{"type":"stream_event","event":{{"type":"content_block_stop","index":0}},"parent_tool_use_id":null,"session_id":"prov-session"}}'
      emit '{{"type":"stream_event","event":{{"type":"message_stop"}},"parent_tool_use_id":null,"session_id":"prov-session"}}'
      ;;
"#
    )
}

/// A Turn calling each of the Broker's Tools once, as Claude names them, every call answered.
const BROKER_CALLS_TURN: &str = r#"      emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_list","name":"mcp__suru__list_providers","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_spawn","name":"mcp__suru__spawn_subagent","input":{"provider":"codex","model":"gpt-5.5","name":"Scout","description":"Map the crates","prompt":"Map the crates."}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":1},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_read","name":"mcp__suru__read_subagent","input":{"session_id":"child-session"}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":2},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"toolu_send","name":"mcp__suru__send_to_subagent","input":{"session_id":"child-session","prompt":"More."}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":3},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":4,"content_block":{"type":"tool_use","id":"toolu_wait","name":"mcp__suru__wait_subagents","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":4},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":5,"content_block":{"type":"tool_use","id":"toolu_stop","name":"mcp__suru__stop_subagent","input":{"session_id":"child-session"}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":5},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_list","content":[{"type":"text","text":"{\"providers\":[]}"}],"is_error":false},{"type":"tool_result","tool_use_id":"toolu_spawn","content":"spawned","is_error":false},{"type":"tool_result","tool_use_id":"toolu_read","content":"working","is_error":false},{"type":"tool_result","tool_use_id":"toolu_send","content":"sent","is_error":false},{"type":"tool_result","tool_use_id":"toolu_wait","content":"settled","is_error":false},{"type":"tool_result","tool_use_id":"toolu_stop","content":"stopped","is_error":false}]},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":900,"num_turns":1,"result":"Delegated.","session_id":"prov-session"}'
"#;

/// The Broker's entry in an MCP config, which must be the only server the file names: the user's
/// own servers come from their own settings, never from a file Suru wrote.
fn broker_server(config: &McpConfigFile) -> &Value {
    let servers = config.contents["mcpServers"]
        .as_object()
        .unwrap_or_else(|| panic!("the file names MCP servers: {}", config.contents));
    assert_eq!(
        servers.keys().collect::<Vec<_>>(),
        ["suru"],
        "the file names the Broker and nothing else: {}",
        config.contents
    );
    &servers["suru"]
}

/// Checks the note `launch` appends to the Agent's system prompt: it names each of the Broker's
/// Tools by the name Claude gives an MCP server's Tools, says when to prefer them, and leaves the
/// Agent's own subagent tools where they are.
fn assert_carries_the_broker_note(launch: &Launch) {
    let note = launch.value("--append-system-prompt");
    for tool in [
        "mcp__suru__list_providers",
        "mcp__suru__spawn_subagent",
        "mcp__suru__read_subagent",
        "mcp__suru__stop_subagent",
    ] {
        assert!(
            note.contains(tool),
            "the note names {tool} as the Agent would select it: {note:?}"
        );
    }
    assert!(
        note.contains("when the user names another Provider or Model"),
        "the note says when to prefer the Broker: {note:?}"
    );
    assert!(
        note.contains("Suru never re-routes them"),
        "the note leaves the Agent's own subagent tools in place: {note:?}"
    );
}

/// The bearer token the Broker's entry presents.
fn token(server: &Value) -> &str {
    server["headers"]["Authorization"]
        .as_str()
        .and_then(|header| header.strip_prefix("Bearer "))
        .filter(|token| !token.is_empty())
        .unwrap_or_else(|| panic!("the Broker's entry presents a bearer token: {server}"))
}

#[tokio::test]
async fn a_session_launch_carries_the_broker_config_and_allowlist_with_a_token() {
    let claude = conversation_fixture("      :\n");
    let live = LiveTurn::start(
        ClaudeRuntime::new(claude.executable()),
        "claude-broker-launch",
        "Which Providers could you delegate to?",
    )
    .await;
    let launch = claude.wait_for_launch_carrying("--session-id").await;

    assert_eq!(
        launch.value("--allowedTools"),
        "mcp__suru__*",
        "every Broker Tool is allowlisted, so none raises an Approval: {:?}",
        launch.arguments
    );
    let config = claude.mcp_config_of(&launch);
    assert!(
        config.path.is_absolute(),
        "the CLI is pointed at a file rather than handed JSON: {:?}",
        config.path
    );
    assert_eq!(
        config.permissions, "-rw-------",
        "only the user Suru runs as may read the token in the file"
    );
    let server = broker_server(&config);
    assert_eq!(server["type"], "http");
    assert_eq!(server["url"], live.broker_url().as_str());
    assert_eq!(
        server["timeout"], 900_000,
        "the per-call limit stands above the Broker's longest call"
    );
    let token = token(server);
    assert!(
        launch
            .arguments
            .iter()
            .all(|argument| !argument.contains(token)),
        "the token never rides the command line, which other local users can read: {:?}",
        launch.arguments
    );
    assert!(
        !launch.carries("--strict-mcp-config"),
        "the user's own MCP servers still load beside the Broker: {:?}",
        launch.arguments
    );
    assert_carries_the_broker_note(&launch);

    live.shutdown().await;
    assert!(
        !config.path.exists(),
        "the file is removed once the process it served has stopped"
    );
}

#[tokio::test]
async fn a_resume_relaunch_after_a_restart_carries_a_fresh_token() {
    let claude = conversation_fixture(ANSWERED);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let (server, client) = hosting(&claude, "claude-broker-resume", state_dir.path()).await;
    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Open a conversation to come back to".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .await
        .expect("create Session");
    let session_id = created.session.id;
    assert_eq!(
        settled_session(&client, session_id, 0).await.turns[0].status,
        TurnStatus::Completed
    );
    let opened = claude.mcp_config_of(&claude.launch_carrying("--session-id"));
    drop(client);
    server.shutdown().await.expect("stop the original server");
    claude.wait_for_exits(claude.launches()).await;
    assert!(
        !opened.path.exists(),
        "the first launch's file went with the server that wrote it"
    );

    let (server, client) = hosting(&claude, "claude-broker-resume", state_dir.path()).await;
    client
        .admit_prompt(
            session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Carry on".to_owned(),
                    skill_invocations: Vec::new(),
                    attachments: Vec::new(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("admit a Prompt to the reopened Session");
    assert_eq!(
        settled_session(&client, session_id, 1).await.turns[1].status,
        TurnStatus::Completed
    );
    let resume = claude.launch_carrying("--resume");
    assert_eq!(resume.value("--allowedTools"), "mcp__suru__*");
    // A system prompt lives only as long as the process it was given to, so the relaunch appends
    // the note again.
    assert_carries_the_broker_note(&resume);
    let resumed = claude.mcp_config_of(&resume);

    let (opened_server, resumed_server) = (broker_server(&opened), broker_server(&resumed));
    assert_ne!(
        token(opened_server),
        token(resumed_server),
        "the relaunch is handed a token of its own rather than the retired one"
    );
    assert_eq!(resumed_server["type"], "http");
    assert_eq!(
        resumed_server["url"]
            .as_str()
            .map(|url| url.ends_with("/broker")),
        Some(true),
        "the relaunch names the replacement server's Broker: {resumed_server}"
    );
    assert_eq!(resumed_server["timeout"], 900_000);
    assert_ne!(
        opened.path, resumed.path,
        "each launch is handed a file of its own"
    );

    drop(client);
    server.shutdown().await.expect("shut the server down");
}

#[tokio::test]
async fn with_the_broker_off_a_session_launch_carries_no_mcp_config_allowlist_or_note() {
    let claude = conversation_fixture("      :\n");
    let live = LiveTurn::start_configured(
        ClaudeRuntime::new(claude.executable()),
        "claude-broker-off",
        "Work without the Broker",
        r#"{"broker":{"enabled":false}}"#,
    )
    .await;
    let launch = claude.wait_for_launch_carrying("--session-id").await;

    assert!(
        !launch.carries("--mcp-config") && !launch.carries("--allowedTools"),
        "a launch made while the Broker is off is handed no server: {:?}",
        launch.arguments
    );
    assert!(
        !launch.carries("--append-system-prompt"),
        "nor told of a Broker it cannot reach: {:?}",
        launch.arguments
    );
    assert!(
        claude.mcp_configs().is_empty(),
        "no launch was pointed at an MCP config"
    );

    live.shutdown().await;
}

#[tokio::test]
async fn a_report_leaves_as_a_stdin_user_message_waking_the_idle_parent() {
    let claude = conversation_fixture(ANSWERED);
    let opened = opened_session(&claude, "claude-broker-report", "Delegate the survey").await;
    let parent_id = opened.session_id;
    settled_session(&opened.client, parent_id, 0).await;
    let parent_launch = claude.wait_for_launch_carrying("--session-id").await;
    let server = broker_server(&claude.mcp_config_of(&parent_launch)).clone();
    let mut broker = McpClient::presenting(
        server["url"].as_str().expect("the Broker's URL"),
        server["headers"]["Authorization"]
            .as_str()
            .map(str::to_owned),
    );
    broker.initialize().await;

    // The idle parent's Agent spawns a Claude Subagent, whose own process answers its Delegation
    // and settles.
    let child_id = broker
        .spawn_subagent(json!({
            "provider": "claude",
            "model": "haiku",
            "options": {},
            "name": "Researcher",
            "description": "Survey the seams",
            "prompt": "Survey the seams.",
        }))
        .await;

    // Its Report wakes the parent into a Continuation — the parent's third Turn, after its first
    // and the Continuation that holds the Subagent's row — which the parent's process settles.
    let woken = settled_session(&opened.client, parent_id, 2).await;
    assert_eq!(woken.turns[2].status, TurnStatus::Completed);
    assert_eq!(woken.turns[2].prompt_id, None, "a Continuation");
    let Some(Activity::Subagent { duration_ms, .. }) = woken.activities.iter().find(
        |activity| matches!(activity, Activity::Subagent { session_id, .. } if *session_id == child_id),
    ) else {
        panic!("the parent's Transcript holds the Subagent's row");
    };
    let report = SubagentReport::new(
        child_id,
        "Researcher",
        SubagentReportOutcome::Completed,
        *duration_ms,
        Some("Done"),
        None,
    )
    .to_string();
    let user_messages = claude
        .requests()
        .into_iter()
        .filter(|request| request.get("type").and_then(Value::as_str) == Some("user"))
        .collect::<Vec<_>>();
    let delivered = user_messages
        .iter()
        .filter(|request| request.pointer("/message/content/0/text") == Some(&json!(report)))
        .collect::<Vec<_>>();
    assert_eq!(
        delivered.len(),
        1,
        "the Report leaves once, as a stdin user message: {user_messages:?}"
    );
    assert_eq!(
        delivered[0]["message"]["content"].as_array().map(Vec::len),
        Some(1),
        "whose one text block is the Report as Suru words it"
    );
    assert!(
        woken
            .messages
            .iter()
            .filter(|message| message.turn_id == woken.turns[2].id)
            .all(|message| message.role == MessageRole::Agent),
        "the Report stands nowhere in the parent's Transcript"
    );

    let opened_server = opened.server;
    drop(opened.client);
    opened_server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn a_report_folded_into_a_turn_waiting_in_a_broker_call_settles_it_on_the_one_result() {
    let claude = ScriptedClaude::new(&format!(
        "{}{}",
        discovery_arms(CLAUDE_MODELS),
        waiting_parent_and_answering_child()
    ));
    let live = LiveTurn::start(
        ClaudeRuntime::new(claude.executable()),
        "claude-broker-report-steer",
        "Delegate the work and wait for it",
    )
    .await;
    let parent_id = live.session_id;
    let parent_launch = claude.wait_for_launch_carrying("--session-id").await;
    let server = broker_server(&claude.mcp_config_of(&parent_launch)).clone();
    let mut broker = McpClient::presenting(
        server["url"].as_str().expect("the Broker's URL"),
        server["headers"]["Authorization"]
            .as_str()
            .map(str::to_owned),
    );
    broker.initialize().await;

    // The working parent's Agent spawns a Claude Subagent, whose own process answers its
    // Delegation and settles while the parent's loop is still inside its wait.
    let child_id = broker
        .spawn_subagent(json!({
            "provider": "claude",
            "model": "haiku",
            "options": {},
            "name": "Researcher",
            "description": "Survey the seams",
            "prompt": "Survey the seams.",
        }))
        .await;

    let settled = settled_session(&live.client, parent_id, 0).await;
    assert_eq!(
        settled.turns[0].status,
        TurnStatus::Completed,
        "the one result the loop answered the Prompt and the folded Report with Settles the Turn"
    );
    assert_eq!(
        settled.turns.len(),
        1,
        "the Report steered the working Turn rather than waking a Continuation"
    );
    let Some(Activity::Subagent { turn_id, .. }) = settled.activities.iter().find(
        |activity| matches!(activity, Activity::Subagent { session_id, .. } if *session_id == child_id),
    ) else {
        panic!("the parent's Transcript holds the Subagent's row");
    };
    assert_eq!(*turn_id, settled.turns[0].id, "in the Turn that spawned it");
    assert_eq!(
        settled
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::Agent)
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        ["DONE Done"]
    );

    let parent_messages = claude
        .requests()
        .into_iter()
        .filter(|request| request.get("type").and_then(Value::as_str) == Some("user"))
        .filter(|request| {
            request
                .pointer("/message/content/0/text")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.contains("Survey the seams"))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        parent_messages.len(),
        2,
        "the parent's process was handed its Prompt and then the Report: {parent_messages:?}"
    );
    assert!(
        parent_messages[1]["message"]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.starts_with("Subagent Report from Suru")),
        "the Report steered the running loop as a stdin user message: {parent_messages:?}"
    );
    assert!(
        parent_messages[1]["uuid"].is_string(),
        "under a uuid the CLI reports its lifecycle by: {parent_messages:?}"
    );

    live.shutdown().await;
}

/// The Broker's calls that spawn, send to, or stop a Subagent stand only in the Subagent row they
/// affect, which the Broker adds itself; its calls that read — listing Providers, reading a
/// Subagent, waiting on Subagents — are Tool Calls on the Broker's own server.
#[tokio::test]
async fn the_brokers_reading_calls_are_tool_calls_and_its_subagent_calls_make_none() {
    let claude = conversation_fixture(BROKER_CALLS_TURN);
    let opened = opened_session(&claude, "claude-broker-tool-calls", "Delegate").await;
    let settled = settled_session(&opened.client, opened.session_id, 0).await;

    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    let recorded = settled
        .activities
        .iter()
        .map(|activity| match activity {
            Activity::ToolCall {
                server,
                name,
                output,
                ..
            } => (server.as_deref(), name.as_str(), output.as_str()),
            activity => panic!("every Activity here is a Tool Call, got {activity:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        recorded,
        [
            (Some("suru"), "list_providers", r#"{"providers":[]}"#),
            (Some("suru"), "read_subagent", "working"),
            (Some("suru"), "wait_subagents", "settled"),
        ]
    );
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}
