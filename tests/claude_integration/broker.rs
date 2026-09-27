//! The Broker handed to Claude (ADR 0034, docs/validation/0408-claude-http-mcp-long-calls.md).
//!
//! A Session's CLI process is pointed with `--mcp-config` at a file naming the Broker's endpoint as
//! the MCP server `suru`, the bearer token its launch was handed as a header, and a per-server
//! timeout above the longest call the Broker serves; `--allowedTools` allowlists the Broker's Tools
//! so calling one never raises an Approval. The token travels in that owner-only file rather than
//! on the command line, the file is gone once the process it served is, and a relaunch after a
//! restart is handed a token of its own. With the Broker turned off a launch carries none of it.

use crate::support::{LiveTurn, McpConfigFile, conversation_fixture, hosting, settled_session};
use serde_json::Value;
use suru::{
    protocol::{
        AdmitPromptRequest, CreateSessionRequest, InitialPrompt, PromptDelivery, PromptId,
        TurnStatus,
    },
    provider::ClaudeRuntime,
};

/// One agent Message and the terminal result that Settles the Turn.
const ANSWERED: &str = r#"      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Done"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":5,"num_turns":1,"result":"Done","terminal_reason":"completed","session_id":"prov-session"}'
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
async fn with_the_broker_off_a_session_launch_carries_no_mcp_config_or_allowlist() {
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
        claude.mcp_configs().is_empty(),
        "no launch was pointed at an MCP config"
    );

    live.shutdown().await;
}
