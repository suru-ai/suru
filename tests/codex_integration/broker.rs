//! The Broker handed to Codex (ADR 0034, docs/validation/0408-codex-per-thread-mcp-config.md).
//!
//! Codex takes the Broker through the per-thread `config` map: one override, `mcp_servers.suru`,
//! naming the endpoint, the bearer token as a static header, a tool timeout above the Broker's
//! longest call, and approve-by-default for its Tools so no call waits on a reviewer or an
//! elicitation. Nothing of the server is kept with the thread, so every `thread/resume` carries it
//! again — with the token its own launch was handed. Both also carry a note in the thread's
//! developer instructions saying the Broker is there: a resumed thread reads its developer
//! instructions from the configuration it is resumed under, and a compaction rebuilds the thread's
//! opening context from them. With the Broker turned off neither request carries anything of it.
//!
//! A Codex Subagent a Session on another Provider spawns through the Broker starts its thread under
//! the Codex value ADR 0036's table gives for that Session's posture.
//!
//! A Subagent Report that wakes an idle Codex Agent is the whole input of a `turn/start` on its
//! thread, and one reaching a working Codex Turn is the harness's own `turn/steer` pinned to it
//! (ADR 0035).

use std::sync::Arc;

use crate::provider_support::ControlledProvider;
use crate::server_support::{PROGRESS_DEADLINE, broker::McpClient};
use crate::support::{ScriptedCodex, receive_initial_state};
use serde_json::{Value, json};
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig},
    protocol::{
        Activity, AdmitPromptRequest, AgentId, AgentIdentity, AgentSelection, ApprovalPosture,
        ClaudePermissionMode, CreateSessionRequest, InitialPrompt, ModelAvailability,
        ModelDescriptor, ModelId, PromptDelivery, PromptId, ProviderId, SessionId, TurnStatus,
    },
    provider::{CodexRuntime, SubagentReport, SubagentReportOutcome},
    server::{self, RunningServer, ServerConfig},
};
use tokio::time::timeout;

/// An app-server whose first process runs a Turn and then exits, so the next Turn resumes the same
/// thread on a process of its own.
const LOST_THEN_RESUMED: &str = r#"
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"broker-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"thread/resume"'*)
      printf '%s\n' '{"id":2,"result":{"thread":{"id":"broker-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":3,"result":{"turn":{"id":"turn-'"$attempt"'"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"broker-thread","turn":{"id":"turn-'"$attempt"'","status":"completed","items":[]}}}'
      if [ "$attempt" -eq 1 ]; then
        printf '%s\n' 'exited' > "$CODEX_FIXTURE_EXITED"
        exit 17
      fi
      ;;
"#;

/// A Session whose thread was started on one app-server and resumed on the next, with the
/// directories it lived in held for as long as the test holds this.
struct ResumedThread {
    codex: ScriptedCodex,
    server: RunningServer,
    _client: ManagedClient,
    _state_dir: tempfile::TempDir,
    _config_dir: tempfile::TempDir,
    _workspace: tempfile::TempDir,
}

impl ResumedThread {
    /// Runs a Session's first Turn, loses the app-server under it, and runs a second Turn on the
    /// app-server that resumes the thread, under the Config Document `document`.
    async fn run(channel: &'static str, document: &str) -> Self {
        let codex = ScriptedCodex::new_multiprocess(LOST_THEN_RESUMED);
        let state_dir = tempfile::tempdir().expect("create isolated state directory");
        let config_dir = tempfile::tempdir().expect("create isolated config directory");
        let workspace = tempfile::tempdir().expect("create valid Workspace");
        std::fs::write(config_dir.path().join("suru.jsonc"), document)
            .expect("write Config Document");
        let server = server::spawn_with_provider(
            ServerConfig::new(state_dir.path(), channel)
                .expect("configure server")
                .with_config_dir(config_dir.path()),
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
                    text: "Which Providers could you delegate to?".to_owned(),
                    skill_invocations: Vec::new(),
                },
            })
            .await
            .expect("create Session");
        let session_id = created.session.id;
        turn_settles(&client, session_id, 0).await;
        codex.wait_for_exit().await;

        client
            .admit_prompt(
                session_id,
                AdmitPromptRequest {
                    prompt: InitialPrompt {
                        id: PromptId::new(),
                        text: "And now?".to_owned(),
                        skill_invocations: Vec::new(),
                    },
                    delivery: PromptDelivery::Queue,
                },
            )
            .await
            .expect("admit a Prompt after the app-server was lost");
        turn_settles(&client, session_id, 1).await;
        Self {
            codex,
            server,
            _client: client,
            _state_dir: state_dir,
            _config_dir: config_dir,
            _workspace: workspace,
        }
    }

    /// The parameters of the first request Suru made for `method`.
    fn params_of(&self, method: &str) -> Value {
        self.codex
            .requests()
            .into_iter()
            .find(|request| request["method"] == method)
            .unwrap_or_else(|| panic!("Suru sent {method}: {:?}", self.codex.methods()))["params"]
            .clone()
    }

    async fn shutdown(self) {
        self.server.shutdown().await.expect("shut down server");
    }
}

async fn turn_settles(client: &ManagedClient, session_id: SessionId, turn_index: usize) {
    timeout(PROGRESS_DEADLINE, async {
        loop {
            let snapshot = client.read_session(session_id).await.expect("read Session");
            if snapshot
                .turns
                .get(turn_index)
                .is_some_and(|turn| turn.status == TurnStatus::Completed)
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("Codex Turn {turn_index} completes"));
}

/// The Broker's entry in a thread request's `config` map, which must be the map's only override,
/// keyed by the dotted path that sets that one server and leaves the user's own servers standing.
fn broker_server<'a>(params: &'a Value, method: &str) -> &'a Value {
    let config = params["config"]
        .as_object()
        .unwrap_or_else(|| panic!("{method} carries a config map: {params}"));
    assert_eq!(
        config.keys().collect::<Vec<_>>(),
        ["mcp_servers.suru"],
        "{method} overrides the Broker's server entry and nothing else: {params}"
    );
    &config["mcp_servers.suru"]
}

#[tokio::test]
async fn thread_start_and_thread_resume_carry_the_server_headers_timeout_and_approval_mode() {
    let resumed = ResumedThread::run("codex-broker-config", "{}").await;
    let endpoint = format!("{}/broker", resumed.server.descriptor().base_url);

    let mut tokens = Vec::new();
    for method in ["thread/start", "thread/resume"] {
        let params = resumed.params_of(method);
        let server = broker_server(&params, method);
        assert_eq!(server["url"], endpoint.as_str(), "{method}: {server}");
        assert_eq!(
            server["tool_timeout_sec"].as_f64(),
            Some(900.0),
            "{method} sets a per-call limit above the Broker's longest call: {server}"
        );
        assert!(
            server["tool_timeout_sec"].is_f64(),
            "{method} states the limit in the float seconds Codex reads it as: {server}"
        );
        assert_eq!(
            server["default_tools_approval_mode"], "approve",
            "{method} approves the Broker's Tools by default, so none waits on a reviewer: {server}"
        );
        assert!(
            server.get("bearer_token").is_none(),
            "{method} never uses the inline token Codex refuses for streamable HTTP: {server}"
        );
        let token = server["http_headers"]["Authorization"]
            .as_str()
            .and_then(|header| header.strip_prefix("Bearer "))
            .filter(|token| !token.is_empty())
            .unwrap_or_else(|| panic!("{method} presents a bearer token: {server}"));
        tokens.push(token.to_owned());
    }
    assert_ne!(
        tokens[0], tokens[1],
        "the resume's launch is handed a token of its own rather than the retired one"
    );

    resumed.shutdown().await;
}

#[tokio::test]
async fn thread_start_and_thread_resume_carry_the_broker_note_in_their_developer_instructions() {
    let resumed = ResumedThread::run("codex-broker-note", "{}").await;

    for method in ["thread/start", "thread/resume"] {
        let params = resumed.params_of(method);
        let note = params["developerInstructions"]
            .as_str()
            .unwrap_or_else(|| panic!("{method} carries developer instructions: {params}"));
        for tool in [
            "mcp__suru__list_providers",
            "mcp__suru__spawn_subagent",
            "mcp__suru__read_subagent",
            "mcp__suru__stop_subagent",
        ] {
            assert!(
                note.contains(tool),
                "{method}'s note names {tool} as Codex names an MCP server's Tools: {note:?}"
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

    resumed.shutdown().await;
}

#[tokio::test]
async fn with_the_broker_off_neither_thread_start_nor_thread_resume_carries_the_server_or_note() {
    let resumed = ResumedThread::run("codex-broker-off", r#"{"broker":{"enabled":false}}"#).await;

    for method in ["thread/start", "thread/resume"] {
        let params = resumed.params_of(method);
        assert!(
            params.get("config").is_none(),
            "{method} made while the Broker is off overrides nothing: {params}"
        );
        assert!(
            params.get("developerInstructions").is_none(),
            "{method} made while the Broker is off tells the Agent of none: {params}"
        );
    }

    resumed.shutdown().await;
}

/// An app-server that lists one Model and starts a thread and a Turn on it, answering each request
/// by the id it came with, since discovery and the Subagent's own launch are processes of their own.
const BROKERED_THREAD: &str = r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CODEX_FIXTURE_LOG"
  id=$(printf '%s' "$line" | sed -n 's/^{"id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":'"$id"',"result":{}}'
      ;;
    *'"method":"model/list"'*)
      printf '%s\n' '{"id":'"$id"',"result":{"data":[{"id":"gpt-fixture","displayName":"GPT Fixture","description":"Fixture model","hidden":false,"supportedReasoningEfforts":[],"defaultReasoningEffort":"medium","serviceTiers":[],"defaultServiceTier":null,"isDefault":true}],"nextCursor":null}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"brokered-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":'"$id"',"result":{"turn":{"id":"brokered-turn"}}}'
      ;;
  esac
done
"#;

#[tokio::test]
async fn a_codex_subagent_of_a_claude_session_set_to_bypass_permissions_starts_its_thread_never_asking_with_full_access()
 {
    let codex = ScriptedCodex::new(BROKERED_THREAD);
    let opus = AgentSelection {
        provider: ProviderId::new("claude"),
        model: ModelId::new("opus"),
        options: Vec::new(),
    };
    let (claude_runtime, mut claude) = ControlledProvider::with_provider(
        ProviderId::new("claude"),
        vec![ModelDescriptor {
            provider: ProviderId::new("claude"),
            id: ModelId::new("opus"),
            display_name: "Opus".to_owned(),
            description: String::new(),
            is_default: true,
            availability: ModelAvailability::Available,
            options: Vec::new(),
        }],
    );
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let config_dir = tempfile::tempdir().expect("create isolated config directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    std::fs::write(
        config_dir.path().join("suru.jsonc"),
        r#"{"provider":{"claude":{"permissionMode":"bypassPermissions"}}}"#,
    )
    .expect("write Config Document");
    let channel = "codex-broker-derived-posture";
    let server = server::spawn_with_providers(
        ServerConfig::new(state_dir.path(), channel)
            .expect("configure server")
            .with_config_dir(config_dir.path()),
        vec![
            claude_runtime,
            Arc::new(CodexRuntime::new(codex.executable())),
        ],
    )
    .await
    .expect("spawn server");
    let mut client = ManagedClient::connect(
        ManagedClientConfig::new(state_dir.path(), channel).expect("configure client"),
    )
    .await
    .expect("connect client");
    receive_initial_state(&mut client).await;
    client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: Some(opus.clone()),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Hand the Codex seam to a Codex Agent".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create the Claude Session");
    let start = timeout(PROGRESS_DEADLINE, claude.next_start())
        .await
        .expect("the Claude Session's Provider is asked to start");
    assert_eq!(
        start.approval_posture(),
        Some(&ApprovalPosture::Claude {
            permission_mode: ClaudePermissionMode::BypassPermissions,
        }),
        "the Claude Session follows the bypassPermissions Setting"
    );
    let handoff = start
        .broker()
        .cloned()
        .expect("the Claude Session is handed the Broker");
    let mut parent = start.succeed(AgentIdentity {
        agent: AgentId::new("claude-agent"),
        selection: opus,
    });
    timeout(PROGRESS_DEADLINE, parent.next_turn())
        .await
        .expect("the Claude Session's first Turn reaches its Provider")
        .succeed();

    let mut broker = McpClient::handed(&handoff);
    broker.initialize().await;
    broker
        .spawn_subagent(json!({
            "provider": "codex",
            "model": "gpt-fixture",
            "name": "Researcher",
            "description": "Survey the Codex seam",
            "prompt": "Find where Codex plugs into Suru.",
        }))
        .await;

    codex.wait_for_method("turn/start").await;
    let requests = codex.requests();
    let params_of = |method: &str| {
        requests
            .iter()
            .find(|request| request["method"] == method)
            .unwrap_or_else(|| panic!("Suru sent {method}: {:?}", codex.methods()))["params"]
            .clone()
    };
    let thread_start = params_of("thread/start");
    assert_eq!(
        (&thread_start["approvalPolicy"], &thread_start["sandbox"]),
        (&json!("never"), &json!("danger-full-access")),
        "the Subagent's thread starts under Codex's value at bypassPermissions' level, not the \
         Codex Setting: {thread_start}"
    );
    let turn_start = params_of("turn/start");
    assert_eq!(
        (
            &turn_start["approvalPolicy"],
            &turn_start["sandboxPolicy"]["type"]
        ),
        (&json!("never"), &json!("dangerFullAccess")),
        "and its Delegation's Turn runs under the same: {turn_start}"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
    drop(parent);
}

/// An app-server for every Session, each running one thread whose every Turn answers "Done." and
/// completes, echoing each request's own id so a process answers as many Turns as it is asked.
const ANSWERING_EVERY_TURN: &str = r#"
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"model/list"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      printf '%s\n' '{"id":'"$id"',"result":{"data":[{"id":"gpt-fixture","displayName":"GPT Fixture","description":"Fixture model","hidden":false,"supportedReasoningEfforts":[],"defaultReasoningEffort":"medium","serviceTiers":[],"defaultServiceTier":null,"isDefault":true}],"nextCursor":null}}'
      ;;
    *'"method":"thread/start"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"report-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      printf '%s\n' '{"id":'"$id"',"result":{"turn":{"id":"turn-'"$id"'"}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"report-thread","turnId":"turn-'"$id"'","item":{"type":"agentMessage","id":"message-'"$id"'","text":""}}}'
      printf '%s\n' '{"method":"item/agentMessage/delta","params":{"threadId":"report-thread","turnId":"turn-'"$id"'","itemId":"message-'"$id"'","delta":"Done."}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"report-thread","turnId":"turn-'"$id"'","item":{"type":"agentMessage","id":"message-'"$id"'","text":"Done."}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"report-thread","turn":{"id":"turn-'"$id"'","status":"completed","items":[]}}}'
      ;;
"#;

#[tokio::test]
async fn a_report_leaves_as_the_input_of_a_turn_start_waking_the_idle_parent() {
    let codex = ScriptedCodex::new_multiprocess(ANSWERING_EVERY_TURN);
    let channel = "codex-broker-report";
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
    let parent_id = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Delegate the survey".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session")
        .session
        .id;
    turn_settles(&client, parent_id, 0).await;
    let thread_start = codex
        .requests()
        .into_iter()
        .find(|request| request["method"] == "thread/start")
        .expect("the parent's thread was started")["params"]
        .clone();
    let server_entry = broker_server(&thread_start, "thread/start");
    let mut broker = McpClient::presenting(
        server_entry["url"].as_str().expect("the Broker's URL"),
        server_entry["http_headers"]["Authorization"]
            .as_str()
            .map(str::to_owned),
    );
    broker.initialize().await;

    // The idle parent's Agent spawns a Codex Subagent, whose own app-server answers its
    // Delegation and settles.
    let child_id = broker
        .spawn_subagent(json!({
            "provider": "codex",
            "model": "gpt-fixture",
            "options": {},
            "name": "Researcher",
            "description": "Survey the seams",
            "prompt": "Survey the seams.",
        }))
        .await;

    // Its Report wakes the parent into a Continuation — its third Turn, after its first and the
    // Continuation that holds the Subagent's row — which the parent's app-server completes.
    turn_settles(&client, parent_id, 2).await;
    let woken = client.read_session(parent_id).await.expect("read Session");
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
    )
    .to_string();
    let reporting = codex
        .requests()
        .into_iter()
        .filter(|request| request["method"] == "turn/start")
        .filter(|request| {
            request["params"]["input"]
                .as_array()
                .is_some_and(|input| input.iter().any(|item| item["text"] == report.as_str()))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        reporting.len(),
        1,
        "the Report leaves once, on a turn/start"
    );
    assert_eq!(
        reporting[0]["params"]["input"],
        json!([{ "type": "text", "text": report }]),
        "whose whole input is the Report as Suru words it"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}

/// The same app-servers, except that the Turn a Session's first Prompt begins keeps running until
/// it is steered, and the steer's answer completes it.
const RUNNING_UNTIL_STEERED: &str = r#"
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"model/list"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      printf '%s\n' '{"id":'"$id"',"result":{"data":[{"id":"gpt-fixture","displayName":"GPT Fixture","description":"Fixture model","hidden":false,"supportedReasoningEfforts":[],"defaultReasoningEffort":"medium","serviceTiers":[],"defaultServiceTier":null,"isDefault":true}],"nextCursor":null}}'
      ;;
    *'"method":"thread/start"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"report-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*'"text":"Delegate the survey"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      running="turn-$id"
      printf '%s\n' '{"id":'"$id"',"result":{"turn":{"id":"'"$running"'"}}}'
      ;;
    *'"method":"turn/start"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      printf '%s\n' '{"id":'"$id"',"result":{"turn":{"id":"turn-'"$id"'"}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"report-thread","turn":{"id":"turn-'"$id"'","status":"completed","items":[]}}}'
      ;;
    *'"method":"turn/steer"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      printf '%s\n' '{"id":'"$id"',"result":{"turnId":"'"$running"'"}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"report-thread","turn":{"id":"'"$running"'","status":"completed","items":[]}}}'
      ;;
"#;

#[tokio::test]
async fn a_report_reaching_a_working_parent_leaves_as_a_turn_steer_pinned_to_its_turn() {
    let codex = ScriptedCodex::new_multiprocess(RUNNING_UNTIL_STEERED);
    let channel = "codex-broker-report-steer";
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
    let parent_id = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Delegate the survey".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session")
        .session
        .id;
    codex.wait_for_method("turn/start").await;
    let thread_start = codex
        .requests()
        .into_iter()
        .find(|request| request["method"] == "thread/start")
        .expect("the parent's thread was started")["params"]
        .clone();
    let server_entry = broker_server(&thread_start, "thread/start");
    let mut broker = McpClient::presenting(
        server_entry["url"].as_str().expect("the Broker's URL"),
        server_entry["http_headers"]["Authorization"]
            .as_str()
            .map(str::to_owned),
    );
    broker.initialize().await;

    // The parent's Agent spawns a Codex Subagent while its own Turn still works, and the
    // Subagent's app-server settles it at once.
    let child_id = broker
        .spawn_subagent(json!({
            "provider": "codex",
            "model": "gpt-fixture",
            "options": {},
            "name": "Researcher",
            "description": "Survey the seams",
            "prompt": "Survey the seams.",
        }))
        .await;

    // Its Report steers the parent's working Turn, whose steer answer completes it.
    turn_settles(&client, parent_id, 0).await;
    let steered = client.read_session(parent_id).await.expect("read Session");
    assert_eq!(
        steered.turns.len(),
        1,
        "the Report began no Turn of its own"
    );
    let Some(Activity::Subagent { duration_ms, .. }) = steered.activities.iter().find(
        |activity| matches!(activity, Activity::Subagent { session_id, .. } if *session_id == child_id),
    ) else {
        panic!("the parent's Transcript holds the Subagent's row");
    };
    let report = SubagentReport::new(
        child_id,
        "Researcher",
        SubagentReportOutcome::Completed,
        *duration_ms,
        None,
    )
    .to_string();
    let requests = codex.requests();
    let running = requests
        .iter()
        .filter(|request| request["method"] == "turn/start")
        .find(|request| request["params"]["input"][0]["text"] == "Delegate the survey")
        .map(|request| format!("turn-{}", request["id"]))
        .expect("the parent's first Turn started");
    let steers = requests
        .iter()
        .filter(|request| request["method"] == "turn/steer")
        .collect::<Vec<_>>();
    assert_eq!(steers.len(), 1, "the Report leaves once, as a steer");
    assert_eq!(
        steers[0]["params"]["input"],
        json!([{ "type": "text", "text": report }]),
        "whose whole input is the Report as Suru words it"
    );
    assert_eq!(
        steers[0]["params"]["expectedTurnId"], running,
        "pinned to the Turn it steers, so a Turn that has ended is never steered"
    );
    assert!(
        requests
            .iter()
            .filter(|request| request["method"] == "turn/start")
            .all(|request| request["params"]["input"][0]["text"] != report.as_str()),
        "and no turn/start carries it"
    );

    drop(client);
    server.shutdown().await.expect("shut down server");
}
