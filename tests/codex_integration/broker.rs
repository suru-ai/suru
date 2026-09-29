//! The Broker handed to Codex (ADR 0034, docs/validation/0408-codex-per-thread-mcp-config.md).
//!
//! Codex takes the Broker through the per-thread `config` map: one override, `mcp_servers.suru`,
//! naming the endpoint, the bearer token as a static header, a tool timeout above the Broker's
//! longest call, and approve-by-default for its Tools so no call waits on a reviewer or an
//! elicitation. Nothing of the server is kept with the thread, so every `thread/resume` carries it
//! again — with the token its own launch was handed. Both also carry a note in the thread's
//! developer instructions saying the Broker is there: a resumed thread reads its developer
//! instructions from the configuration it is resumed under, and a compaction rebuilds the thread's
//! opening context from them. A thread's developer instructions take the place of those the user's
//! own configuration sets, so each launch first asks its app-server for that configuration through
//! `config/read` and hands the user's instructions back with the note after them. With the Broker
//! turned off neither request carries anything of it, and no configuration is read.
//!
//! A Codex Subagent a Session on another Provider spawns through the Broker starts its thread under
//! the Codex value ADR 0036's table gives for that Session's posture.
//!
//! A Subagent Report that wakes an idle Codex Agent is the whole input of a `turn/start` on its
//! thread, and one reaching a working Codex Turn is the harness's own `turn/steer` pinned to it
//! (ADR 0035).
//!
//! A native Codex Subagent's thread calls the Broker under its parent thread's token, naming itself
//! in the call's `_meta.threadId`, so its calls are its own Session's
//! (docs/validation/0408-subagent-mcp-attribution.md).
//!
//! Codex reports each Broker call as an MCP tool call on server `suru`: one that spawns, sends to,
//! or stops a Subagent stands only in the Subagent row it affects, and every other is a Tool Call.

use std::sync::Arc;

use crate::provider_support::{ControlledProvider, ControlledProviderSession};
use crate::server_support::{PROGRESS_DEADLINE, broker::McpClient};
use crate::support::{
    ScriptedCodex, conversation_codex, opened_session, receive_initial_state, settled_session,
};
use serde_json::{Value, json};
use suru::{
    managed_client::{ManagedClient, ManagedClientConfig},
    protocol::{
        Activity, AdmitPromptRequest, AgentId, AgentIdentity, AgentSelection, ApprovalPosture,
        ClaudePermissionMode, CreateSessionRequest, InitialPrompt, MessageRole, ModelAvailability,
        ModelDescriptor, ModelId, PromptDelivery, PromptId, ProviderId, SessionId, SessionSnapshot,
        TurnStatus,
    },
    provider::{CodexRuntime, ProviderEvent, SubagentReport, SubagentReportOutcome},
    server::{self, RunningServer, ServerConfig},
};
use tokio::time::timeout;

/// An app-server whose first process runs a Turn and then exits, so the next Turn resumes the same
/// thread on a process of its own. Each process answers `config/read` with `__USER_CONFIG__`, the
/// user's own configuration as that launch finds it, and every request by the id it came with,
/// since a launch made while the Broker is off reads no configuration.
const LOST_THEN_RESUMED: &str = r#"
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":1,"result":{}}'
      ;;
    *'"method":"config/read"'*)
      id=$(printf '%s' "$line" | sed -n 's/^{"id":\([0-9][0-9]*\),.*/\1/p')
      printf '%s\n' '{"id":'"$id"',"result":{"config":__USER_CONFIG__,"origins":{}}}'
      ;;
    *'"method":"thread/start"'*|*'"method":"thread/resume"'*)
      id=$(printf '%s' "$line" | sed -n 's/^{"id":\([0-9][0-9]*\),.*/\1/p')
      printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"broker-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*)
      id=$(printf '%s' "$line" | sed -n 's/^{"id":\([0-9][0-9]*\),.*/\1/p')
      printf '%s\n' '{"id":'"$id"',"result":{"turn":{"id":"turn-'"$attempt"'"}}}'
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

/// A user's Codex configuration that sets no developer instructions.
const NO_USER_INSTRUCTIONS: &str = "{}";

impl ResumedThread {
    /// Runs a Session's first Turn, loses the app-server under it, and runs a second Turn on the
    /// app-server that resumes the thread, under the Config Document `document` and a user's Codex
    /// configuration that sets no developer instructions.
    async fn run(channel: &'static str, document: &str) -> Self {
        Self::run_configured(channel, document, NO_USER_INSTRUCTIONS).await
    }

    /// As [`Self::run`], with each app-server finding the user's Codex configuration to be
    /// `user_config`, a JSON object spliced into the stand-in's shell script.
    async fn run_configured(channel: &'static str, document: &str, user_config: &str) -> Self {
        let codex = ScriptedCodex::new_multiprocess(
            &LOST_THEN_RESUMED.replace("__USER_CONFIG__", user_config),
        );
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
                    attachments: Vec::new(),
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
                        attachments: Vec::new(),
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

/// Asserts that `note` is the Broker's note as Codex's Agent is handed it.
fn assert_is_the_broker_note(note: &str, method: &str) {
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

#[tokio::test]
async fn thread_start_and_thread_resume_carry_the_broker_note_in_their_developer_instructions() {
    let resumed = ResumedThread::run("codex-broker-note", "{}").await;

    for method in ["thread/start", "thread/resume"] {
        let params = resumed.params_of(method);
        let note = params["developerInstructions"]
            .as_str()
            .unwrap_or_else(|| panic!("{method} carries developer instructions: {params}"));
        assert_is_the_broker_note(note, method);
        assert!(
            !note.contains('\n'),
            "{method} made for a user whose configuration sets no developer instructions carries \
             the one-line note alone: {note:?}"
        );
    }

    resumed.shutdown().await;
}

#[tokio::test]
async fn the_broker_note_follows_the_developer_instructions_the_users_own_configuration_sets() {
    // Each launch finds the user's instructions as they stand then, so the resume's differ from
    // the start's the way an edit made between launches would.
    let resumed = ResumedThread::run_configured(
        "codex-broker-user-instructions",
        "{}",
        r#"{"developer_instructions":"Launch '"$attempt"': answer in British English.\nName every file you touch."}"#,
    )
    .await;

    let started_in = resumed.params_of("thread/start")["cwd"].clone();
    let reads = resumed
        .codex
        .requests()
        .into_iter()
        .filter(|request| request["method"] == "config/read")
        .collect::<Vec<_>>();
    assert_eq!(
        reads.len(),
        2,
        "each launch reads the user's configuration afresh: {:?}",
        resumed.codex.methods()
    );
    for read in &reads {
        assert_eq!(
            read["params"]["cwd"], started_in,
            "the configuration read is the one a thread working where the Session works is built \
             from, project layers included: {read}"
        );
    }

    for (method, launch) in [("thread/start", 1), ("thread/resume", 2)] {
        let params = resumed.params_of(method);
        let instructions = params["developerInstructions"]
            .as_str()
            .unwrap_or_else(|| panic!("{method} carries developer instructions: {params}"));
        let user =
            format!("Launch {launch}: answer in British English.\nName every file you touch.\n\n");
        let note = instructions.strip_prefix(&user).unwrap_or_else(|| {
            panic!(
                "{method} keeps the user's own developer instructions, whole and first, a blank \
                 line before the note: {instructions:?}"
            )
        });
        assert_is_the_broker_note(note, method);
        assert!(
            !note.contains("British English"),
            "{method} carries the user's instructions once: {instructions:?}"
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
    assert!(
        !resumed
            .codex
            .methods()
            .iter()
            .any(|method| method == "config/read"),
        "a launch made while the Broker is off has no note to add and reads no configuration: {:?}",
        resumed.codex.methods()
    );

    resumed.shutdown().await;
}

/// An app-server that lists one Model and starts a thread and a Turn on it, answering each request
/// by the id it came with, since discovery and the Subagent's own launch are processes of their own.
const BROKERED_THREAD: &str = r#"#!/bin/sh
while IFS= read -r line; do
  append_line "$CODEX_FIXTURE_LOG" "$line"
  id=$(printf '%s' "$line" | sed -n 's/^{"id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":'"$id"',"result":{}}'
      ;;
    *'"method":"config/read"'*)
      printf '%s\n' '{"id":'"$id"',"result":{"config":{},"origins":{}}}'
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
                attachments: Vec::new(),
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
    *'"method":"config/read"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      printf '%s\n' '{"id":'"$id"',"result":{"config":{},"origins":{}}}'
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
                attachments: Vec::new(),
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
        None,
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
    *'"method":"config/read"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      printf '%s\n' '{"id":'"$id"',"result":{"config":{},"origins":{}}}'
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
                attachments: Vec::new(),
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

/// An app-server whose Session's Turn spawns a native Subagent on a thread of its own, through a
/// collab spawn, and leaves both working: the parent thread's Turn never completes, and the child
/// thread Suru attaches works in its own native turn until the test releases it, when that turn
/// completes. The release is awaited beside the read loop — which keeps answering meanwhile — and
/// only while this app-server runs. A `turn/start` on the child's thread, handing it input of
/// Suru's own, begins a native turn there whose input arrives on the thread as it would from Codex,
/// and in which the child answers and completes. Every request is answered by the id it came with,
/// since the Model Catalog's discovery runs a process of its own.
const NATIVE_CHILD_WORKING: &str = r#"#!/bin/sh
while IFS= read -r line; do
  append_line "$CODEX_FIXTURE_LOG" "$line"
  id=$(printf '%s' "$line" | sed -n 's/^{"id":\([0-9][0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' '{"id":'"$id"',"result":{}}'
      ;;
    *'"method":"config/read"'*)
      printf '%s\n' '{"id":'"$id"',"result":{"config":{},"origins":{}}}'
      ;;
    *'"method":"model/list"'*)
      printf '%s\n' '{"id":'"$id"',"result":{"data":[{"id":"gpt-fixture","displayName":"GPT Fixture","description":"Fixture model","hidden":false,"supportedReasoningEfforts":[],"defaultReasoningEffort":"medium","serviceTiers":[],"defaultServiceTier":null,"isDefault":true}],"nextCursor":null}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"root-thread"},"model":"gpt-fixture"}}'
      ;;
    *'"method":"turn/start"'*'"threadId":"child-thread"'*)
      input=$(printf '%s' "$line" | sed -En 's/.*"input":\[\{"type":"text","text":("([^"\\]|\\.)*")\}\].*/\1/p')
      printf '%s\n' '{"id":'"$id"',"result":{"turn":{"id":"child-report-turn"}}}'
      printf '%s\n' '{"method":"turn/started","params":{"threadId":"child-thread","turn":{"id":"child-report-turn","status":"inProgress","items":[]}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"child-thread","turnId":"child-report-turn","item":{"type":"userMessage","id":"child-input","content":[{"type":"text","text":'"$input"'}]}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"child-thread","turnId":"child-report-turn","item":{"type":"agentMessage","id":"child-answer","text":""}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"child-thread","turnId":"child-report-turn","item":{"type":"agentMessage","id":"child-answer","text":"The Researcher found the Claude seam."}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"child-thread","turn":{"id":"child-report-turn","status":"completed","items":[]}}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{"id":'"$id"',"result":{"turn":{"id":"root-turn"}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"collabAgentToolCall","id":"call-spawn","tool":"spawnAgent","status":"inProgress","senderThreadId":"root-thread","receiverThreadIds":[],"agentsStates":{}}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"root-thread","turnId":"root-turn","item":{"type":"collabAgentToolCall","id":"call-spawn","tool":"spawnAgent","status":"completed","senderThreadId":"root-thread","receiverThreadIds":["child-thread"],"prompt":"Map the crate layout","agentsStates":{"child-thread":{"status":"running"}}}}}'
      ;;
    *'"method":"thread/resume"'*)
      printf '%s\n' '{"id":'"$id"',"result":{"thread":{"id":"child-thread","parentThreadId":"root-thread"},"model":"gpt-child"}}'
      if [ -z "$attached" ]; then
        attached=1
        printf '%s\n' '{"method":"turn/started","params":{"threadId":"child-thread","turn":{"id":"child-turn","status":"inProgress","items":[]}}}'
        (
          while [ ! -e "$CODEX_FIXTURE_RELEASE" ] && kill -0 $$ 2>/dev/null; do sleep 0.01; done
          if [ -e "$CODEX_FIXTURE_RELEASE" ]; then
            printf '%s\n' '{"method":"turn/completed","params":{"threadId":"child-thread","turn":{"id":"child-turn","status":"completed","items":[]}}}'
          fi
        ) &
      fi
      ;;
  esac
done
"#;

/// The Session once `predicate` holds of it.
async fn session_where(
    client: &ManagedClient,
    session_id: SessionId,
    what: &str,
    predicate: impl Fn(&SessionSnapshot) -> bool,
) -> SessionSnapshot {
    timeout(PROGRESS_DEADLINE, async {
        loop {
            let snapshot = client.read_session(session_id).await.expect("read Session");
            if predicate(&snapshot) {
                return snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}"))
}

/// A Codex Session on [`NATIVE_CHILD_WORKING`] whose collab spawn has opened a native Subagent's
/// Session and attached its thread, on a Server hosting Claude's double beside it for the Subagents
/// the Broker spawns, with what a thread's MCP client needs to reach the Broker.
struct NativeCodexChild {
    codex: ScriptedCodex,
    server: RunningServer,
    client: ManagedClient,
    claude: ControlledProvider,
    session_id: SessionId,
    native_id: SessionId,
    native_name: String,
    endpoint: String,
    authorization: String,
    _state_dir: tempfile::TempDir,
    _workspace: tempfile::TempDir,
}

impl NativeCodexChild {
    async fn open(channel: &'static str) -> Self {
        let codex = ScriptedCodex::new(NATIVE_CHILD_WORKING);
        let (claude_runtime, claude) = ControlledProvider::with_provider(
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
        let workspace = tempfile::tempdir().expect("create valid Workspace");
        let server = server::spawn_with_providers(
            ServerConfig::new(state_dir.path(), channel).expect("configure server"),
            vec![
                Arc::new(CodexRuntime::new(codex.executable())),
                claude_runtime,
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
        let session_id = client
            .create_session(CreateSessionRequest {
                preparation_id: None,
                agent_selection: Some(AgentSelection {
                    provider: ProviderId::new("codex"),
                    model: ModelId::new("gpt-fixture"),
                    options: Vec::new(),
                }),
                execution_directory: suru::protocol::ExecutionDirectory {
                    path: workspace.path().to_owned(),
                },
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Map the crates".to_owned(),
                    skill_invocations: Vec::new(),
                    attachments: Vec::new(),
                },
            })
            .await
            .expect("create the Codex Session")
            .session
            .id;
        let parent = session_where(
            &client,
            session_id,
            "the collab spawn opens the native Subagent's row",
            |snapshot| {
                snapshot
                    .activities
                    .iter()
                    .any(|activity| matches!(activity, Activity::Subagent { .. }))
            },
        )
        .await;
        let Some(Activity::Subagent {
            session_id: native_id,
            name: native_name,
            ..
        }) = parent
            .activities
            .iter()
            .find(|activity| matches!(activity, Activity::Subagent { .. }))
        else {
            unreachable!()
        };
        let (native_id, native_name) = (*native_id, native_name.clone());
        codex.wait_for_method("thread/resume").await;

        // Every thread reaches the Broker the parent thread was handed, under its token.
        let thread_start = codex
            .requests()
            .into_iter()
            .find(|request| request["method"] == "thread/start")
            .expect("Suru starts the parent's thread")["params"]
            .clone();
        let broker = broker_server(&thread_start, "thread/start");
        let endpoint = broker["url"]
            .as_str()
            .expect("the Broker's endpoint")
            .to_owned();
        let authorization = broker["http_headers"]["Authorization"]
            .as_str()
            .expect("the parent thread's token")
            .to_owned();
        Self {
            codex,
            server,
            client,
            claude,
            session_id,
            native_id,
            native_name,
            endpoint,
            authorization,
            _state_dir: state_dir,
            _workspace: workspace,
        }
    }

    /// The MCP client the thread `thread` is, naming itself in every call as Codex does.
    async fn thread_client(&self, thread: &str) -> McpClient {
        let mut client = McpClient::presenting(&self.endpoint, Some(self.authorization.clone()))
            .with_call_meta(json!({ "threadId": thread, "sessionId": "root-thread" }));
        client.initialize().await;
        client
    }

    /// The brokered Subagent's own Provider on Claude's double, started and handed its
    /// Delegation, which it answers with.
    async fn run_brokered(&mut self) -> (ControlledProviderSession, String) {
        let start = timeout(PROGRESS_DEADLINE, self.claude.next_start())
            .await
            .expect("the brokered Subagent's Provider is asked to start");
        let mut provider = start.succeed(AgentIdentity {
            agent: AgentId::new("claude-agent"),
            selection: AgentSelection {
                provider: ProviderId::new("claude"),
                model: ModelId::new("opus"),
                options: Vec::new(),
            },
        });
        let turn = timeout(PROGRESS_DEADLINE, provider.next_turn())
            .await
            .expect("the Delegation reaches the brokered Subagent's Provider");
        let delegation = turn.prompt().to_owned();
        turn.succeed();
        (provider, delegation)
    }

    async fn shutdown(self) {
        drop(self.client);
        self.server.shutdown().await.expect("shut down server");
    }
}

/// `spawn_subagent`'s arguments for a Subagent named `name` on Claude's Opus.
fn claude_researcher(name: &str) -> Value {
    json!({
        "provider": "claude",
        "model": "opus",
        "name": name,
        "description": "Survey the Claude seam",
        "prompt": "Find where Claude plugs into Suru.",
    })
}

#[tokio::test]
async fn a_native_subagents_thread_calling_the_broker_is_attributed_to_the_subagents_session() {
    let mut native = NativeCodexChild::open("codex-broker-native-attribution").await;
    let mut child_thread = native.thread_client("child-thread").await;
    let mut root_thread = native.thread_client("root-thread").await;

    let childs = child_thread
        .spawn_subagent(claude_researcher("Researcher"))
        .await;
    assert_eq!(
        native
            .client
            .read_session(childs)
            .await
            .expect("read the brokered Subagent")
            .session
            .parent,
        Some(native.native_id),
        "the child thread's spawn is recorded beneath the native Subagent's Session"
    );
    let native_session = native
        .client
        .read_session(native.native_id)
        .await
        .expect("read the native Subagent");
    assert!(
        native_session.activities.iter().any(|activity| matches!(
            activity,
            Activity::Subagent { session_id, .. } if *session_id == childs
        )),
        "whose Transcript holds its row: {:?}",
        native_session.activities
    );
    let (provider, delegation) = native.run_brokered().await;
    assert_eq!(
        delegation,
        format!(
            "Delegated to you through Suru by the Subagent \"{}\".\n\nFind where Claude plugs \
             into Suru.",
            native.native_name
        ),
        "the Delegation names the native Subagent as the Agent that sent it"
    );

    let parents = root_thread.spawn_subagent(claude_researcher("Scout")).await;
    assert_eq!(
        native
            .client
            .read_session(parents)
            .await
            .expect("read the brokered Subagent")
            .session
            .parent,
        Some(native.session_id),
        "while the parent thread's own spawn is the Session's"
    );

    native.shutdown().await;
    drop(provider);
}

#[tokio::test]
async fn a_report_to_a_settled_native_subagent_leaves_as_a_turn_start_on_its_thread() {
    let mut native = NativeCodexChild::open("codex-broker-native-report").await;
    let mut child_thread = native.thread_client("child-thread").await;
    let childs = child_thread
        .spawn_subagent(claude_researcher("Researcher"))
        .await;
    let (provider, _) = native.run_brokered().await;

    // The native Subagent's own turn completes while the Subagent it delegated to works on.
    native.codex.release();
    session_where(
        &native.client,
        native.native_id,
        "the native Subagent's stretch of work settles",
        |snapshot| snapshot.turns[0].status == TurnStatus::Completed,
    )
    .await;

    // The brokered Subagent answers and settles, which it reports to the native Subagent.
    const ANSWER: &str = "Claude plugs in through its stream-json process.";
    for event in [
        ProviderEvent::AgentMessageStarted,
        ProviderEvent::AgentMessageDelta {
            content: ANSWER.to_owned(),
        },
        ProviderEvent::AgentMessageCompleted,
        ProviderEvent::TurnCompleted,
    ] {
        provider.emit_and_wait_until_observed(event).await;
    }

    // It wakes the native Subagent into a Continuation of its own Session, which the child thread
    // completes having answered.
    let woken = session_where(
        &native.client,
        native.native_id,
        "the Report wakes the native Subagent into a Continuation that settles",
        |snapshot| {
            snapshot
                .turns
                .get(1)
                .is_some_and(|turn| turn.status == TurnStatus::Completed)
        },
    )
    .await;
    let Some(Activity::Subagent { duration_ms, .. }) = woken.activities.iter().find(
        |activity| matches!(activity, Activity::Subagent { session_id, .. } if *session_id == childs),
    ) else {
        panic!("the native Subagent's Transcript holds the brokered Subagent's row");
    };
    let report = SubagentReport::new(
        childs,
        "Researcher",
        SubagentReportOutcome::Completed,
        *duration_ms,
        Some(ANSWER),
        None,
    )
    .to_string();
    let requests = native.codex.requests();
    let reporting = requests
        .iter()
        .position(|request| {
            request["method"] == "turn/start" && request["params"]["threadId"] == "child-thread"
        })
        .expect("the Report leaves as a turn/start on the child's thread");
    let turn_start = &requests[reporting]["params"];
    assert_eq!(
        turn_start["input"],
        json!([{ "type": "text", "text": report }]),
        "whose whole input is the Report as Suru words it: {turn_start}"
    );
    assert!(
        ["model", "effort", "summary"]
            .iter()
            .all(|setting| turn_start.get(setting).is_none()),
        "naming no Model, effort or Reasoning summary, so the Subagent goes on as its spawn set \
         it running: {turn_start}"
    );
    let attaches = requests[..reporting]
        .iter()
        .filter(|request| request["method"] == "thread/resume")
        .collect::<Vec<_>>();
    assert_eq!(
        attaches.len(),
        2,
        "the settled child's thread is attached again first, so the turn it begins streams \
         here: {:?}",
        native.codex.methods()
    );
    let reattach = &attaches[1]["params"];
    assert_eq!(reattach["threadId"], "child-thread");
    assert!(
        reattach.get("config").is_none() && reattach.get("developerInstructions").is_none(),
        "carrying neither the Broker nor its note, which the child inherits from its parent's \
         thread and Codex ignores on a running one: {reattach}"
    );
    assert_eq!(woken.turns[1].prompt_id, None, "a Continuation");
    assert_eq!(
        woken
            .messages
            .iter()
            .filter(|message| message.turn_id == woken.turns[1].id)
            .map(|message| (message.role.clone(), message.content.as_str()))
            .collect::<Vec<_>>(),
        [(MessageRole::Agent, "The Researcher found the Claude seam.")],
        "holding what the child thread did, and nothing for the Report its turn began with"
    );
    assert!(
        woken
            .activities
            .iter()
            .all(|activity| activity.turn_id() != woken.turns[1].id),
        "no row stands for the Report"
    );
    let parent = native
        .client
        .read_session(native.session_id)
        .await
        .expect("read the Codex Session");
    assert_eq!(
        (parent.turns.len(), parent.activities.len()),
        (1, 1),
        "and the Codex Session's own Transcript gains nothing"
    );

    native.shutdown().await;
    drop(provider);
}

/// Each of the Broker's Tools called as Codex reports an MCP tool call on the Broker's server `suru`
/// — the longest with progress while it waits — beside a call to another MCP server.
const BROKER_CALLS_TURN: &str = r#"      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"mcpToolCall","id":"call-list","server":"suru","tool":"list_providers","status":"inProgress","arguments":{},"result":null,"error":null,"durationMs":null}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"mcpToolCall","id":"call-list","server":"suru","tool":"list_providers","status":"completed","arguments":{},"result":{"content":[{"type":"text","text":"claude, codex, copilot"}],"structuredContent":null},"error":null,"durationMs":4}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"mcpToolCall","id":"call-spawn","server":"suru","tool":"spawn_subagent","status":"inProgress","arguments":{"provider":"claude","model":"opus","name":"Scout","description":"Map the crates","prompt":"Map the crates."},"result":null,"error":null,"durationMs":null}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"mcpToolCall","id":"call-spawn","server":"suru","tool":"spawn_subagent","status":"completed","arguments":{"provider":"claude","model":"opus","name":"Scout","description":"Map the crates","prompt":"Map the crates."},"result":{"content":[{"type":"text","text":"spawned"}],"structuredContent":null},"error":null,"durationMs":9}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"mcpToolCall","id":"call-read","server":"suru","tool":"read_subagent","status":"inProgress","arguments":{"session_id":"child-session"},"result":null,"error":null,"durationMs":null}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"mcpToolCall","id":"call-read","server":"suru","tool":"read_subagent","status":"completed","arguments":{"session_id":"child-session"},"result":{"content":[{"type":"text","text":"working"}],"structuredContent":null},"error":null,"durationMs":2}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"mcpToolCall","id":"call-send","server":"suru","tool":"send_to_subagent","status":"inProgress","arguments":{"session_id":"child-session","prompt":"More."},"result":null,"error":null,"durationMs":null}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"mcpToolCall","id":"call-send","server":"suru","tool":"send_to_subagent","status":"completed","arguments":{"session_id":"child-session","prompt":"More."},"result":{"content":[{"type":"text","text":"sent"}],"structuredContent":null},"error":null,"durationMs":2}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"mcpToolCall","id":"call-wait","server":"suru","tool":"wait_subagents","status":"inProgress","arguments":{},"result":null,"error":null,"durationMs":null}}}'
      printf '%s\n' '{"method":"item/mcpToolCall/progress","params":{"threadId":"native-thread","turnId":"native-turn","itemId":"call-wait","message":"30s of 60s"}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"mcpToolCall","id":"call-wait","server":"suru","tool":"wait_subagents","status":"completed","arguments":{},"result":{"content":[{"type":"text","text":"settled"}],"structuredContent":null},"error":null,"durationMs":30000}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"mcpToolCall","id":"call-stop","server":"suru","tool":"stop_subagent","status":"inProgress","arguments":{"session_id":"child-session"},"result":null,"error":null,"durationMs":null}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"mcpToolCall","id":"call-stop","server":"suru","tool":"stop_subagent","status":"completed","arguments":{"session_id":"child-session"},"result":{"content":[{"type":"text","text":"stopped"}],"structuredContent":null},"error":null,"durationMs":2}}}'
      printf '%s\n' '{"method":"item/started","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"mcpToolCall","id":"call-linear","server":"linear","tool":"list_issues","status":"inProgress","arguments":{},"result":null,"error":null,"durationMs":null}}}'
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"native-thread","turnId":"native-turn","item":{"type":"mcpToolCall","id":"call-linear","server":"linear","tool":"list_issues","status":"completed","arguments":{},"result":{"content":[{"type":"text","text":"No issues."}],"structuredContent":null},"error":null,"durationMs":7}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"native-thread","turn":{"id":"native-turn","status":"completed","items":[]}}}'
"#;

/// The Broker's calls that spawn, send to, or stop a Subagent stand only in the Subagent row they
/// affect, which the Broker adds itself; its calls that read — listing Providers, reading a
/// Subagent, waiting on Subagents — are Tool Calls on the Broker's own server.
#[tokio::test]
async fn the_brokers_reading_calls_are_tool_calls_and_its_subagent_calls_make_none() {
    let codex = conversation_codex(BROKER_CALLS_TURN);
    let opened = opened_session(
        &codex,
        "codex-broker-tool-calls",
        "Which Providers could you delegate to?",
    )
    .await;
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
            (Some("suru"), "list_providers", "claude, codex, copilot"),
            (Some("suru"), "read_subagent", "working"),
            (Some("suru"), "wait_subagents", "settled"),
            (Some("linear"), "list_issues", "No issues."),
        ],
        "the Broker's spawn, send and stop stand in no row of their own"
    );

    opened.server.shutdown().await.expect("shut down server");
}
