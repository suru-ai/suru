//! The Broker handed to Copilot (ADR 0034, docs/validation/0408-copilot-mcp-tool-timeout.md).
//!
//! One Copilot CLI serves every Copilot Session, so the Broker is handed to each Session on the
//! only per-Session seam there is: the MCP server list on `session.create` and on every
//! `session.resume`, naming the endpoint as the HTTP server `suru` with the token its launch was
//! handed as a header and a per-server timeout above the Broker's longest call. Copilot asks
//! Suru's own permission handler before every MCP call, and the handler approves the Broker's
//! calls itself, so none reaches the user as an Approval; nor does a Broker call stand in the
//! Transcript as a Command. Beside the server list, each of those requests appends a note to
//! Copilot's own system message saying the Broker is there. With the Broker turned off no Session
//! is handed the server or the note.
//!
//! A Subagent Report that wakes an idle Copilot Agent leaves as a `session.send` in immediate mode
//! (ADR 0035).

use std::{path::Path, sync::Arc};

use crate::{
    server_support::{PROGRESS_DEADLINE, broker::McpClient},
    support::{
        LiveTurn, ScriptedCopilot, connect_in, conversation_arms, conversation_fixture,
        opened_session, resumable_conversation_fixture, send_arm, settled_session,
        settled_session_on,
    },
};
use serde_json::{Value, json};
use suru::{
    managed_client::ManagedClientConfig,
    protocol::{
        Activity, AdmitPromptRequest, ApprovalSubject, CreateSessionRequest, InitialPrompt,
        PromptDelivery, PromptId, TurnStatus,
    },
    provider::{CopilotRuntime, SubagentReport, SubagentReportOutcome},
    server::{self, RunningServer, ServerConfig},
};
use tokio::time::timeout;

/// One agent Message and the loop going idle.
const ANSWERED: &str = r#"      event e1 assistant.message '{"messageId":"m1","content":"Done."}'
      event e2 session.idle '{}'
"#;

async fn spawn(config: ServerConfig, copilot: &ScriptedCopilot) -> RunningServer {
    server::spawn_with_provider(config, Arc::new(CopilotRuntime::new(copilot.executable())))
        .await
        .expect("spawn server")
}

fn client_config(state_dir: &Path, data_dir: &Path, channel: &str) -> ManagedClientConfig {
    ManagedClientConfig::new(state_dir, channel)
        .expect("configure client")
        .with_data_dir(data_dir)
}

/// What a Session created on one server and resumed on its replacement sent Copilot, under the
/// Config Document `document`: the `session.create` and `session.resume` parameters, beside the
/// Broker endpoint each server serves.
struct CreatedThenResumed {
    create: Value,
    create_endpoint: String,
    resume: Value,
    resume_endpoint: String,
}

async fn created_then_resumed(channel: &'static str, document: &str) -> CreatedThenResumed {
    let copilot = resumable_conversation_fixture(ANSWERED);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let data_dir = tempfile::tempdir().expect("create isolated data directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    std::fs::write(config_dir.path().join("suru.jsonc"), document).expect("write Config Document");
    let config = ServerConfig::new(state_dir.path(), channel)
        .expect("configure server")
        .with_data_dir(data_dir.path())
        .with_config_dir(config_dir.path());

    let original = spawn(config.clone(), &copilot).await;
    let create_endpoint = format!("{}/broker", original.descriptor().base_url);
    let client = connect_in(client_config(state_dir.path(), data_dir.path(), channel)).await;
    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Which Providers could you delegate to?".to_owned(),
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
    drop(client);
    original.shutdown().await.expect("stop the original server");
    copilot.wait_for_exit().await;

    let replacement = spawn(config, &copilot).await;
    let resume_endpoint = format!("{}/broker", replacement.descriptor().base_url);
    let client = connect_in(client_config(state_dir.path(), data_dir.path(), channel)).await;
    let mut feed = client
        .subscribe_session(session_id)
        .await
        .expect("subscribe to the reopened Session");
    client
        .admit_prompt(
            session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "And now?".to_owned(),
                    skill_invocations: Vec::new(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("admit a Prompt to the reopened Session");
    assert_eq!(
        settled_session_on(&client, &mut feed, session_id, 1)
            .await
            .turns[1]
            .status,
        TurnStatus::Completed
    );
    drop(feed);
    drop(client);
    replacement
        .shutdown()
        .await
        .expect("stop the replacement server");

    let params_of = |method: &str| {
        copilot
            .requests()
            .into_iter()
            .find(|request| request["method"] == method)
            .unwrap_or_else(|| panic!("Suru sent {method}: {:?}", copilot.methods()))["params"]
            .clone()
    };
    CreatedThenResumed {
        create: params_of("session.create"),
        create_endpoint,
        resume: params_of("session.resume"),
        resume_endpoint,
    }
}

/// The bearer token the Broker's entry in `params`' server list presents, once the entry is
/// checked to be the Broker at `endpoint` over HTTP with the Broker's call timeout, and the only
/// server Suru names.
fn broker_token<'a>(params: &'a Value, method: &str, endpoint: &str) -> &'a str {
    let servers = params["mcpServers"]
        .as_object()
        .unwrap_or_else(|| panic!("{method} carries a server list: {params}"));
    assert_eq!(
        servers.keys().collect::<Vec<_>>(),
        ["suru"],
        "{method} names the Broker and nothing else: {params}"
    );
    let server = &servers["suru"];
    assert_eq!(server["type"], "http", "{method}: {server}");
    assert_eq!(server["url"], endpoint, "{method}: {server}");
    assert_eq!(
        server["timeout"], 900_000,
        "{method} gives a Broker call longer than its longest wait: {server}"
    );
    assert!(
        server.get("tools").is_none(),
        "{method} offers every Tool the Broker serves: {server}"
    );
    server["headers"]["Authorization"]
        .as_str()
        .and_then(|header| header.strip_prefix("Bearer "))
        .filter(|token| !token.is_empty())
        .unwrap_or_else(|| panic!("{method} presents a bearer token: {server}"))
}

#[tokio::test]
async fn session_create_and_resume_carry_the_broker_server_each_with_its_own_token() {
    let sent = created_then_resumed("copilot-broker-servers", "{}").await;

    let created = broker_token(&sent.create, "session.create", &sent.create_endpoint);
    let resumed = broker_token(&sent.resume, "session.resume", &sent.resume_endpoint);
    assert_ne!(
        created, resumed,
        "the resume's launch is handed a token of its own rather than the retired one"
    );
}

#[tokio::test]
async fn session_create_and_resume_append_the_broker_note_to_copilots_system_message() {
    let sent = created_then_resumed("copilot-broker-note", "{}").await;

    for (method, params) in [
        ("session.create", &sent.create),
        ("session.resume", &sent.resume),
    ] {
        let message = &params["systemMessage"];
        assert_eq!(
            message["mode"], "append",
            "{method} adds to Copilot's own system message rather than replacing it: {params}"
        );
        assert!(
            message.get("sections").is_none(),
            "{method} customizes no section of Copilot's: {params}"
        );
        let note = message["content"]
            .as_str()
            .unwrap_or_else(|| panic!("{method} carries the note: {params}"));
        for tool in [
            "suru-list_providers",
            "suru-spawn_subagent",
            "suru-read_subagent",
            "suru-stop_subagent",
        ] {
            assert!(
                note.contains(tool),
                "{method}'s note names {tool} as Copilot names an MCP server's Tools: {note:?}"
            );
        }
        assert!(
            note.contains("when the user names another Provider or Model"),
            "{method}'s note says when to prefer the Broker: {note:?}"
        );
        assert!(
            note.contains("Suru never re-routes them"),
            "{method}'s note leaves the Agent's own subagent tools in place: {note:?}"
        );
    }
}

#[tokio::test]
async fn with_the_broker_off_neither_create_nor_resume_carries_the_server_or_note() {
    let sent = created_then_resumed("copilot-broker-off", r#"{"broker":{"enabled":false}}"#).await;

    for (method, params) in [
        ("session.create", &sent.create),
        ("session.resume", &sent.resume),
    ] {
        assert!(
            params.get("mcpServers").is_none(),
            "{method} made while the Broker is off names no server: {params}"
        );
        assert!(
            params.get("systemMessage").is_none(),
            "{method} made while the Broker is off tells the Agent of none: {params}"
        );
    }
}

/// The parameters of the decision Suru answered permission request `request_id` with, once it has.
async fn native_decision(copilot: &ScriptedCopilot, request_id: &str) -> Value {
    timeout(PROGRESS_DEADLINE, async {
        loop {
            if let Some(request) = copilot.requests().into_iter().find(|request| {
                request["method"] == "session.permissions.handlePendingPermissionRequest"
                    && request["params"]["requestId"] == request_id
            }) {
                return request["params"].clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("Suru answers permission request {request_id}"))
}

#[tokio::test]
async fn a_permission_request_for_the_broker_is_approved_without_reaching_the_user() {
    // Asked as Copilot asks before an MCP call: for the Broker, for a server of the user's own,
    // which the Session's posture leaves to the user, and for the Broker once more under a managed
    // policy demanding an explicit human decision, which Suru never answers for that policy.
    let requests = r#"      event p1 permission.requested '{"requestId":"managed","permissionRequest":{"kind":"mcp","serverName":"suru","toolName":"suru-list_providers","toolTitle":"list_providers","args":{},"readOnly":false,"toolCallId":"call-managed","managedApprovalRequired":true}}'
      event p2 permission.requested '{"requestId":"broker","permissionRequest":{"kind":"mcp","serverName":"suru","toolName":"suru-list_providers","toolTitle":"list_providers","args":{},"readOnly":false,"toolCallId":"call-broker"}}'
      event p3 permission.requested '{"requestId":"linear","permissionRequest":{"kind":"mcp","serverName":"linear","toolName":"linear-list_issues","toolTitle":"list_issues","args":{},"readOnly":true,"toolCallId":"call-linear"}}'
"#;
    let copilot = ScriptedCopilot::new(&format!("{}{}", conversation_arms(), send_arm(requests)));
    let mut live = LiveTurn::start(
        CopilotRuntime::new(copilot.executable()),
        "copilot-broker-permission",
        "Which Providers could you delegate to?",
    )
    .await;

    let pending = live
        .wait_for("the other server's request reaches the user", |snapshot| {
            !snapshot.pending_approvals.is_empty()
        })
        .await;
    let decision = native_decision(&copilot, "broker").await;
    assert_eq!(
        decision["result"]["kind"], "approve-once",
        "Suru's handler approves the Broker's call itself: {decision}"
    );
    let asked = pending
        .activities
        .iter()
        .filter_map(|activity| match activity {
            Activity::Approval { approval, .. } => Some(&approval.subject),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        matches!(asked.as_slice(), [ApprovalSubject::OtherTool { name, .. }]
            if name == "MCP linear/linear-list_issues"),
        "only the user's own server asks the user; the Broker's call never does: {asked:?}"
    );
    assert!(
        copilot.requests().iter().all(|request| {
            request["method"] != "session.permissions.handlePendingPermissionRequest"
                || request["params"]["requestId"] != "managed"
        }),
        "a managed policy's demand for a human decision is left to that policy's authority"
    );

    live.shutdown().await;
}

#[tokio::test]
async fn a_broker_tool_execution_adds_no_command() {
    // A Broker call as Copilot reports one — its start naming the server, a progress notification
    // while it waits, output, and completion — beside a call to another MCP server, which stands
    // as a Command as ever.
    let timeline = r#"      event e1 tool.execution_start '{"toolCallId":"call-broker","toolName":"suru-list_providers","mcpServerName":"suru","mcpToolName":"list_providers","arguments":{}}'
      event e2 tool.execution_progress '{"toolCallId":"call-broker","progressMessage":"30.0/150.0 (20%): 30s of 150s"}'
      event e3 tool.execution_partial_result '{"toolCallId":"call-broker","partialOutput":"claude"}'
      event e4 tool.execution_complete '{"toolCallId":"call-broker","success":true,"result":{"content":"claude, codex, copilot"}}'
      event e5 tool.execution_start '{"toolCallId":"call-linear","toolName":"linear-list_issues","mcpServerName":"linear","mcpToolName":"list_issues","arguments":{}}'
      event e6 tool.execution_complete '{"toolCallId":"call-linear","success":true,"result":{"content":"No issues."}}'
      event e7 assistant.message '{"messageId":"m1","content":"Claude, Codex and Copilot."}'
      event e8 session.idle '{}'
"#;
    let copilot = conversation_fixture(timeline);
    let opened = opened_session(
        &copilot,
        "copilot-broker-absorbed",
        "Which Providers could you delegate to?",
    )
    .await;
    let settled = settled_session(&opened.client, opened.session_id, 0).await;

    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    let commands = settled
        .activities
        .iter()
        .filter_map(|activity| match activity {
            Activity::Command { command, .. } => Some(command.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        commands,
        ["linear/list_issues"],
        "the Broker's call stands in no Command row: {:?}",
        settled.activities
    );

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

/// One agent Message and the loop going idle, for every send to every Session, each send's events
/// under ids of their own.
const ANSWERED_EVERY_SEND: &str = r#"      sends=$(( ${sends:-0} + 1 ))
      event "m$sends" assistant.message '{"messageId":"m'"$sends"'","content":"Done."}'
      event "i$sends" session.idle '{}'
"#;

#[tokio::test]
async fn a_report_leaves_as_an_immediate_session_send_waking_the_idle_parent() {
    let copilot = conversation_fixture(ANSWERED_EVERY_SEND);
    let opened = opened_session(&copilot, "copilot-broker-report", "Delegate the survey").await;
    let parent_id = opened.session_id;
    settled_session(&opened.client, parent_id, 0).await;
    let create = copilot.wait_for_request("session.create").await["params"].clone();
    let parent_sid = create["sessionId"].clone();
    let server_entry = &create["mcpServers"]["suru"];
    let mut broker = McpClient::presenting(
        server_entry["url"].as_str().expect("the Broker's URL"),
        server_entry["headers"]["Authorization"]
            .as_str()
            .map(str::to_owned),
    );
    broker.initialize().await;

    // The idle parent's Agent spawns a Copilot Subagent, which the same CLI runs as a Session of
    // its own, answering its Delegation and going idle.
    let child_id = broker
        .spawn_subagent(json!({
            "provider": "copilot",
            "model": "claude-fixture",
            "options": {},
            "name": "Researcher",
            "description": "Survey the seams",
            "prompt": "Survey the seams.",
        }))
        .await;

    // Its Report wakes the parent into a Continuation — its third Turn, after its first and the
    // Continuation that holds the Subagent's row — which the parent's loop settles.
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
        Some("Done."),
        None,
    )
    .to_string();
    let reporting = copilot
        .requests()
        .into_iter()
        .filter(|request| request["method"] == "session.send")
        .filter(|request| request["params"]["prompt"] == report.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        reporting.len(),
        1,
        "the Report leaves once, on a session.send"
    );
    assert_eq!(
        reporting[0]["params"]["sessionId"], parent_sid,
        "to the parent's own Session"
    );
    assert_eq!(
        reporting[0]["params"]["mode"], "immediate",
        "in immediate mode, so a loop Copilot is running takes it at once"
    );

    let server = opened.server;
    drop(opened.client);
    server.shutdown().await.expect("shut the server down");
}
