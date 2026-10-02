//! A Context Breakdown read on request from the CLI's native `get_context_usage`, through the
//! runtime, server and client contract.

use std::time::Duration;

use suru::{
    managed_client::ManagedClient,
    protocol::{
        ContextBreakdown, ContextFill, ContextItem, ContextPart, ContextSource,
        CreateSessionRequest, ExecutionDirectory, InitialPrompt, PromptId, SessionError,
        SessionErrorCode, SessionId,
    },
    provider::ClaudeRuntime,
    server::RunningServer,
};

use crate::support::{
    CLAUDE_MODELS, ScriptedClaude, discovery_arms, hosting_runtime, settled_session, user_turn_arm,
};

const TURN: &str = r#"emit '{"type":"system","subtype":"init","model":"claude-fixture-1"}'
emit '{"type":"result","subtype":"success","is_error":false,"usage":{"input_tokens":10,"output_tokens":1}}'"#;

/// The reply the installed CLI gave on 2026-10-02, trimmed to what a breakdown reads and
/// shaped to show each rule: a deferred category and free space occupy nothing, and the
/// buffer is held back rather than occupied.
const USAGE: &str = r#"{"categories":[
{"name":"System prompt","tokens":2805,"color":"promptBorder","kind":"used"},
{"name":"System tools","tokens":3947,"color":"inactive","kind":"used"},
{"name":"System tools (deferred)","tokens":13088,"color":"inactive","isDeferred":true,"kind":"deferred"},
{"name":"MCP tools","tokens":700,"color":"cyan","kind":"used"},
{"name":"Memory files","tokens":1283,"color":"claude","kind":"used"},
{"name":"Skills","tokens":239,"color":"warning","kind":"used"},
{"name":"Custom agents","tokens":90,"color":"permission","kind":"used"},
{"name":"Hook output","tokens":12,"color":"inactive","kind":"used"},
{"name":"Messages","tokens":410,"color":"purple","kind":"used"},
{"name":"Autocompact buffer","tokens":33000,"color":"inactive","kind":"buffer"},
{"name":"Free space","tokens":944514,"color":"promptBorder","kind":"free"}],
"totalTokens":9486,"maxTokens":967000,"rawMaxTokens":1000000,"model":"claude-fixture-1",
"memoryFiles":[{"path":"/repo/CLAUDE.md","type":"Project","tokens":1154},{"path":"<auto-memory-index>","type":"AutoMem","tokens":129}],
"mcpTools":[{"name":"mcp__docs__search","serverName":"docs","tokens":700}],
"agents":[{"agentType":"reviewer","source":"projectSettings","tokens":90}],
"skills":{"totalSkills":2,"includedSkills":2,"tokens":239,"skillFrontmatter":[{"name":"tdd","source":"userSettings","tokens":144},{"name":"grilling","source":"userSettings","tokens":95}]},
"messageBreakdown":{"toolCallTokens":100,"toolResultTokens":200,"attachmentTokens":0,"assistantMessageTokens":60,"userMessageTokens":50,"redirectedContextTokens":0,"unattributedTokens":0}}"#;

fn response(value: &str) -> String {
    let value = value.replace('\n', "");
    format!(
        r#"emit '{{"type":"control_response","response":{{"subtype":"success","request_id":"'"$request_id"'","response":{value}}}}}'"#
    )
}

fn fixture(query: &str) -> ScriptedClaude {
    ScriptedClaude::new(&format!(
        "{}{}\n *'\"subtype\":\"get_context_usage\"'*)\n{query}\n;;\n",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(TURN)
    ))
}

struct Session {
    state: tempfile::TempDir,
    _workspace: tempfile::TempDir,
    server: RunningServer,
    client: ManagedClient,
    id: SessionId,
}

impl Session {
    /// A Session whose first Turn has settled, so its CLI is running.
    async fn settled(claude: &ScriptedClaude) -> Self {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (server, client) = hosting_runtime(
            ClaudeRuntime::new(claude.executable())
                .with_context_request_timeout(Duration::from_millis(200)),
            "claude-context-breakdown",
            state.path(),
        )
        .await;
        let created = client
            .create_session(CreateSessionRequest {
                preparation_id: None,
                agent_selection: None,
                execution_directory: ExecutionDirectory {
                    path: workspace.path().to_owned(),
                },
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Measure context".to_owned(),
                    skill_invocations: vec![],
                    attachments: Vec::new(),
                },
            })
            .await
            .unwrap();
        settled_session(&client, created.session.id, 0).await;
        Self {
            state,
            _workspace: workspace,
            server,
            client,
            id: created.session.id,
        }
    }
}

fn item(label: &str, tokens: u64) -> ContextItem {
    ContextItem {
        label: label.to_owned(),
        tokens,
    }
}

fn part(source: ContextSource, tokens: u64, items: Vec<ContextItem>) -> ContextPart {
    ContextPart {
        source,
        tokens,
        items,
    }
}

fn refusal(error: &anyhow::Error) -> Option<SessionErrorCode> {
    error.downcast_ref::<SessionError>().map(|error| error.code)
}

#[tokio::test]
async fn a_running_session_breaks_its_context_down_by_source() {
    let claude = fixture(&response(USAGE));
    let session = Session::settled(&claude).await;

    let breakdown = session.client.context_breakdown(session.id).await.unwrap();

    assert_eq!(
        breakdown,
        ContextBreakdown {
            fill: ContextFill {
                occupied_tokens: 9486,
                capacity_tokens: Some(1000000),
            },
            reserved_tokens: Some(33000),
            parts: vec![
                part(ContextSource::SystemPrompt, 2805, vec![]),
                part(ContextSource::SystemTools, 3947, vec![]),
                part(
                    ContextSource::McpTools,
                    700,
                    vec![item("mcp__docs__search", 700)]
                ),
                part(
                    ContextSource::Instructions,
                    1283,
                    vec![
                        item("/repo/CLAUDE.md", 1154),
                        item("<auto-memory-index>", 129)
                    ]
                ),
                part(
                    ContextSource::Skills,
                    239,
                    vec![item("tdd", 144), item("grilling", 95)]
                ),
                part(ContextSource::Agents, 90, vec![item("reviewer", 90)]),
                part(
                    ContextSource::Other {
                        label: "Hook output".to_owned()
                    },
                    12,
                    vec![]
                ),
                part(
                    ContextSource::Messages,
                    410,
                    vec![
                        item("Tool calls", 100),
                        item("Tool results", 200),
                        item("Assistant messages", 60),
                        item("User messages", 50),
                    ]
                ),
            ],
        }
    );
    session.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_refused_request_fails_with_the_providers_reason() {
    let claude = fixture(
        r#"emit '{"type":"control_response","response":{"subtype":"error","request_id":"'"$request_id"'","error":"context usage is unavailable"}}'"#,
    );
    let session = Session::settled(&claude).await;

    let refused = session
        .client
        .context_breakdown(session.id)
        .await
        .expect_err("a refusal is no breakdown");

    assert_eq!(
        refusal(&refused),
        Some(SessionErrorCode::ContextBreakdownFailed),
        "{refused:#}"
    );
    assert!(
        format!("{refused:#}").contains("context usage is unavailable"),
        "{refused:#}"
    );
    session.server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_session_reopened_after_a_restart_has_no_provider_to_ask_until_it_works_again() {
    let claude = fixture(&response(USAGE));
    let session = Session::settled(&claude).await;
    let id = session.id;
    session.server.shutdown().await.unwrap();
    let (server, client) = hosting_runtime(
        ClaudeRuntime::new(claude.executable()),
        "claude-context-breakdown",
        session.state.path(),
    )
    .await;

    let refused = client
        .context_breakdown(id)
        .await
        .expect_err("no CLI runs for the Session yet");

    assert_eq!(
        refusal(&refused),
        Some(SessionErrorCode::ContextBreakdownUnavailable),
        "{refused:#}"
    );
    server.shutdown().await.unwrap();
}
