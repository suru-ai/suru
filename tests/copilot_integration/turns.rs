//! Copilot Turns: a delivered Prompt streaming an agent Message into the Transcript, the Turn
//! settling, and what happens to it when Copilot reports an error or its harness process dies.

use std::sync::Arc;

use crate::support::{
    COPILOT_MODELS, ScriptedCopilot, agent_messages, connect, connect_arm, conversation_fixture,
    create_session_arm, modelless_current_model_arm, models_arm, permission_decision_arm,
    resumable_conversation_fixture, send_arm, settled_session, settled_session_on, signed_in_arm,
    switch_model_arm, titling_turned_off,
};
use serde_json::Value;
use suru::{
    protocol::{
        Activity, AdmitPromptRequest, AgentSelection, Cost, CostBasis, CreateSessionRequest,
        InitialPrompt, MessageStatus, ModelId, ModelOptionChoiceId, ModelOptionId,
        ModelOptionSelection, ModelOptionValue, NativeMeter, PromptDelivery, PromptId,
        PromptStatus, ProviderId, TurnStatus, Usage,
    },
    provider::CopilotRuntime,
    server::{self, ServerConfig},
};

/// Two Turns whose native per-call usage has no Turn identity of its own. The first carries two
/// calls under a priced Model; the second carries one call under a Model Copilot did not publish
/// prices for. `sends` belongs to the fixture process, so the same `session.send` arm can play the
/// two distinct stretches in order.
const METERED_TURNS: &str = r#"      sends=$(( ${sends:-0} + 1 ))
      if [ "$sends" -eq 1 ]; then
        event u1 assistant.usage '{"model":"claude-fixture","inputTokens":1000,"cacheReadTokens":100,"cacheWriteTokens":50,"cacheTtlSeconds":3600,"outputTokens":200,"reasoningTokens":50,"cost":1.5,"maxPromptTokens":128000,"maxOutputTokens":32000}'
        event u2 assistant.usage '{"model":"claude-fixture","inputTokens":500,"cacheReadTokens":50,"cacheWriteTokens":0,"outputTokens":100,"reasoningTokens":0,"cost":0.5}'
        event u_sparse assistant.usage '{"model":"unpriced-fixture","inputTokens":0,"cacheReadTokens":0,"cacheWriteTokens":0,"outputTokens":0,"reasoningTokens":0}'
        event m1 assistant.message '{"messageId":"m1","content":"First answer"}'
        event i1 session.idle '{}'
      else
        event u4 assistant.usage '{"model":"unpriced-fixture","inputTokens":100,"outputTokens":20,"cost":0.25,"maxPromptTokens":100000,"maxOutputTokens":20000}'
        event m2 assistant.message '{"messageId":"m2","content":"Second answer"}'
        event i2 session.idle '{}'
      fi
"#;

const USAGE_WITH_UNPRICED_CACHE: &str = r#"      event u1 assistant.usage '{"model":"incomplete-cache-fixture","inputTokens":1000,"cacheReadTokens":100,"outputTokens":200,"cost":1}'
      event m1 assistant.message '{"messageId":"m1","content":"Answer"}'
      event i1 session.idle '{}'
"#;

/// One agent Message arriving as Copilot produces it, then the loop going idle.
const STREAMED_MESSAGE: &str = r#"      event e1 assistant.message_start '{"messageId":"m1"}'
      event e2 assistant.message_delta '{"messageId":"m1","deltaContent":"Hello"}'
      event e3 assistant.message_delta '{"messageId":"m1","deltaContent":" from Copilot"}'
      event e4 assistant.message '{"messageId":"m1","content":"Hello from Copilot"}'
      event e5 session.idle '{}'
"#;

fn selection(model: &str, effort: &str, tier: &str) -> AgentSelection {
    AgentSelection {
        provider: ProviderId::new("copilot"),
        model: ModelId::new(model),
        options: vec![
            ModelOptionSelection {
                id: ModelOptionId::new("reasoning_effort"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new(effort),
                },
            },
            ModelOptionSelection {
                id: ModelOptionId::new("context_tier"),
                value: ModelOptionValue::Select {
                    choice: ModelOptionChoiceId::new(tier),
                },
            },
        ],
    }
}

#[tokio::test]
async fn copilot_per_call_usage_is_bracketed_into_turns_with_reported_catalog_cost() {
    let copilot = conversation_fixture(METERED_TURNS);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "copilot-metered-turns").expect("configure server"),
        Arc::new(CopilotRuntime::new(copilot.executable())),
    )
    .await
    .expect("spawn server");
    let client = connect(state_dir.path(), "copilot-metered-turns").await;

    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Measure two Turns".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session");
    let mut feed = client
        .subscribe_session(created.session.id)
        .await
        .expect("subscribe to Session SSE");
    let first = settled_session_on(&client, &mut feed, created.session.id, 0).await;

    assert_eq!(
        first.turns[0].usage,
        Some(Usage {
            fresh_input_tokens: Some(1_300),
            cache_read_tokens: Some(150),
            cache_write_tokens: Some(50),
            output_tokens: Some(250),
            reasoning_tokens: Some(50),
            native_meter: NativeMeter::from_units(2.0),
            model_context_window: Some(160_000),
        }),
        "both calls are summed into the first Turn with disjoint token parts"
    );
    assert_eq!(
        first.session.context_fill, None,
        "Usage is never a context fallback"
    );
    assert_eq!(first.turns[0].cost, Cost::from_usd(0.05325));
    assert_eq!(first.turns[0].cost_basis, Some(CostBasis::Reported));
    assert!(
        first.turns[0]
            .cost_details
            .as_ref()
            .is_some_and(|details| details.is_partial),
        "the known subtotal remains visible when another call has no published price"
    );
    assert_eq!(
        serde_json::to_value(first.turns[0].usage.as_ref().expect("first Turn Usage"))
            .expect("serialize Usage")["native_meter"],
        serde_json::json!(2.0),
        "fractional per-call premium-request cost is retained as the Turn's native meter"
    );

    client
        .admit_prompt(
            created.session.id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Measure another Turn".to_owned(),
                    skill_invocations: Vec::new(),
                },
                delivery: PromptDelivery::Queue,
            },
        )
        .await
        .expect("admit second Prompt");
    let second = settled_session_on(&client, &mut feed, created.session.id, 1).await;

    assert_eq!(
        second.turns[1].usage,
        Some(Usage {
            fresh_input_tokens: Some(100),
            cache_read_tokens: None,
            cache_write_tokens: None,
            output_tokens: Some(20),
            reasoning_tokens: None,
            native_meter: NativeMeter::from_units(0.25),
            model_context_window: Some(120_000),
        }),
        "the second Turn starts its own token accumulator"
    );
    assert_eq!(
        serde_json::to_value(second.turns[1].usage.as_ref().expect("second Turn Usage"))
            .expect("serialize Usage")["native_meter"],
        serde_json::json!(0.25),
    );
    assert_eq!(second.turns[1].cost, None, "unknown pricing stays absent");
    assert_eq!(second.turns[1].cost_basis, None);
    assert_eq!(
        second.turns[0].usage, first.turns[0].usage,
        "later per-call events do not bleed into a settled Turn"
    );

    drop(feed);
    server.shutdown().await.expect("shut the server down");
}

#[tokio::test]
async fn copilot_cost_is_absent_when_an_applicable_cache_price_is_not_published() {
    let copilot = conversation_fixture(USAGE_WITH_UNPRICED_CACHE);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "copilot-incomplete-cache-price")
            .expect("configure server"),
        Arc::new(CopilotRuntime::new(copilot.executable())),
    )
    .await
    .expect("spawn server");
    let client = connect(state_dir.path(), "copilot-incomplete-cache-price").await;

    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Use the cache".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session");
    let settled = settled_session(&client, created.session.id, 0).await;

    assert_eq!(settled.turns[0].cost, None);
    assert_eq!(settled.turns[0].cost_basis, None);
    assert_eq!(
        settled.turns[0]
            .usage
            .as_ref()
            .and_then(|usage| usage.cache_read_tokens),
        Some(100),
        "usage remains available even though its cache component cannot be priced"
    );

    server.shutdown().await.expect("shut the server down");
}

#[tokio::test]
async fn a_prompt_streams_a_copilot_message_into_the_transcript_and_settles_the_turn() {
    let copilot = conversation_fixture(STREAMED_MESSAGE);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "copilot-streaming-turn").expect("configure server"),
        Arc::new(CopilotRuntime::new(copilot.executable())),
    )
    .await
    .expect("spawn server");
    let client = connect(state_dir.path(), "copilot-streaming-turn").await;

    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Say hello".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session without waiting for Copilot startup");

    let settled = settled_session(&client, created.session.id, 0).await;

    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
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
    assert_eq!(message.content, "Hello from Copilot");
    assert_eq!(
        settled.session.agent_selection,
        Some(selection("claude-fixture", "high", "default")),
        "the Session runs under the Agent Selection Copilot resolved for it"
    );

    let created_request = copilot.wait_for_request("session.create").await;
    let parameters = &created_request["params"];
    assert!(
        parameters["sessionId"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "Suru generates the Copilot Session identifier before creation: {parameters}"
    );
    assert_eq!(parameters["streaming"], Value::Bool(true));
    assert_eq!(
        parameters["requestPermission"],
        Value::Bool(true),
        "the harness answers Copilot's permission requests itself"
    );
    assert_eq!(
        parameters["workingDirectory"].as_str(),
        workspace.path().to_str(),
        "the Copilot Session works in the Suru Session's Workspace"
    );
    assert_eq!(
        copilot
            .methods()
            .into_iter()
            .filter(|method| method.starts_with("session."))
            .collect::<Vec<_>>(),
        ["session.create", "session.model.getCurrent", "session.send"],
        "a Session already on Copilot's Model needs no Model switch"
    );

    server.shutdown().await.expect("shut the server down");
    copilot.wait_for_exit().await;
}

#[tokio::test]
async fn a_turn_selected_under_another_model_switches_copilot_onto_it_first() {
    let copilot = conversation_fixture(STREAMED_MESSAGE);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let titling = titling_turned_off();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "copilot-model-switch")
            .expect("configure server")
            .with_config_dir(titling.path()),
        Arc::new(CopilotRuntime::new(copilot.executable())),
    )
    .await
    .expect("spawn server");
    let client = connect(state_dir.path(), "copilot-model-switch").await;
    // The Agent Selection is normalized against the catalog, so discover it before naming one.
    client.list_models().await.expect("discover Copilot Models");

    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: Some(selection("claude-fixture", "low", "long_context")),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Think less, read more".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session under a chosen Agent Selection");

    let settled = settled_session(&client, created.session.id, 0).await;
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);

    let switched = copilot.wait_for_request("session.model.switchTo").await;
    assert_eq!(switched["params"]["modelId"], "claude-fixture");
    assert_eq!(switched["params"]["reasoningEffort"], "low");
    assert_eq!(switched["params"]["contextTier"], "long_context");
    assert_eq!(
        copilot
            .methods()
            .into_iter()
            .filter(|method| method.starts_with("session."))
            .collect::<Vec<_>>(),
        [
            "session.create",
            "session.model.getCurrent",
            "session.model.switchTo",
            "session.send"
        ],
        "the Model switch precedes the Prompt so the Turn runs under the chosen Selection"
    );

    server.shutdown().await.expect("shut the server down");
}

#[tokio::test]
async fn a_session_whose_cli_reports_no_active_model_runs_under_the_selected_one() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}{}{}{}{}{}",
        connect_arm(),
        signed_in_arm(),
        models_arm(COPILOT_MODELS),
        create_session_arm(),
        modelless_current_model_arm(),
        switch_model_arm(),
        permission_decision_arm(),
        send_arm(STREAMED_MESSAGE),
    ));
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let titling = titling_turned_off();
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "copilot-modelless-startup")
            .expect("configure server")
            .with_config_dir(titling.path()),
        Arc::new(CopilotRuntime::new(copilot.executable())),
    )
    .await
    .expect("spawn server");
    let client = connect(state_dir.path(), "copilot-modelless-startup").await;
    // The Agent Selection is normalized against the catalog, so discover it before naming one.
    client.list_models().await.expect("discover Copilot Models");

    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: Some(selection("claude-fixture", "low", "long_context")),
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Say hello".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session under a chosen Agent Selection");

    let settled = settled_session(&client, created.session.id, 0).await;
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);

    let switched = copilot.wait_for_request("session.model.switchTo").await;
    assert_eq!(
        switched["params"]["modelId"], "claude-fixture",
        "with nothing in force, the first Turn switches Copilot onto its Selection"
    );

    server.shutdown().await.expect("shut the server down");
}

#[tokio::test]
async fn a_modelless_session_with_no_chosen_selection_runs_under_the_catalog_default() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}{}{}{}{}{}",
        connect_arm(),
        signed_in_arm(),
        models_arm(COPILOT_MODELS),
        create_session_arm(),
        modelless_current_model_arm(),
        switch_model_arm(),
        permission_decision_arm(),
        send_arm(STREAMED_MESSAGE),
    ));
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "copilot-modelless-default").expect("configure server"),
        Arc::new(CopilotRuntime::new(copilot.executable())),
    )
    .await
    .expect("spawn server");
    let client = connect(state_dir.path(), "copilot-modelless-default").await;

    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Say hello".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session without choosing an Agent Selection");

    let settled = settled_session(&client, created.session.id, 0).await;
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    assert_eq!(
        settled
            .session
            .agent_selection
            .as_ref()
            .map(|selection| selection.model.as_str()),
        Some("auto"),
        "with nothing in force and nothing chosen, the Session settles on the catalog default"
    );

    let switched = copilot.wait_for_request("session.model.switchTo").await;
    assert_eq!(
        switched["params"]["modelId"], "auto",
        "the first Turn puts the catalog default in force"
    );

    server.shutdown().await.expect("shut the server down");
}

/// Copilot answers the Prompt with a permission request rather than a Message.
const PERMISSION_REQUEST: &str = r#"      event e1 permission.requested '{"requestId":"p1","permissionRequest":{"kind":"shell","toolCallId":"t1"}}'
      event e2 assistant.message '{"messageId":"m1","content":"Ran it"}'
      event e3 session.idle '{}'
"#;

#[tokio::test]
async fn a_permission_request_is_answered_inside_the_harness() {
    let copilot = conversation_fixture(PERMISSION_REQUEST);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "copilot-auto-approval").expect("configure server"),
        Arc::new(CopilotRuntime::new(copilot.executable())),
    )
    .await
    .expect("spawn server");
    let client = connect(state_dir.path(), "copilot-auto-approval").await;

    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Run something that needs permission".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session");

    let settled = settled_session(&client, created.session.id, 0).await;
    assert_eq!(
        settled.turns[0].status,
        TurnStatus::Completed,
        "the agent works on without an approval interruption"
    );

    let decision = copilot
        .wait_for_request("session.permissions.handlePendingPermissionRequest")
        .await;
    assert_eq!(decision["params"]["requestId"], "p1");
    assert_eq!(
        agent_messages(&settled)
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        ["Ran it"],
        "no approval concept crosses the Provider seam into the Transcript"
    );
    assert!(settled.activities.is_empty());

    server.shutdown().await.expect("shut the server down");
}

/// Copilot reports a remote error part-way through the Turn.
const REMOTE_ERROR: &str = r#"      event e1 assistant.message_start '{"messageId":"m1"}'
      event e2 assistant.message_delta '{"messageId":"m1","deltaContent":"Working"}'
      event e3 session.error '{"errorType":"rate_limit","errorCode":"user_weekly_rate_limited","message":"You have exhausted this week of premium requests."}'
      event e4 session.idle '{}'
"#;

#[tokio::test]
async fn a_copilot_error_settles_the_turn_as_failed_with_a_concise_reason() {
    let copilot = conversation_fixture(REMOTE_ERROR);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "copilot-remote-error").expect("configure server"),
        Arc::new(CopilotRuntime::new(copilot.executable())),
    )
    .await
    .expect("spawn server");
    let client = connect(state_dir.path(), "copilot-remote-error").await;

    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
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
            "a Copilot error settles the Turn with an Error Activity, got {:?}",
            settled.activities
        );
    };
    assert_eq!(
        text, "Copilot rate limit error: You have exhausted this week of premium requests.",
        "the failure names the category Copilot typed it as"
    );
    assert!(!text.contains('\n'));
    assert_eq!(
        agent_messages(&settled)[0].content,
        "Working",
        "what Copilot did stream before failing stays in the Transcript"
    );

    server.shutdown().await.expect("shut the server down");
}

/// A harness that dies part-way through its first Turn and serves the next Prompt normally.
const CRASH_MID_TURN: &str = r#"      if [ "$attempt" -gt 1 ]; then
        event e1 assistant.message '{"messageId":"m2","content":"Back again"}'
        event e2 session.idle '{}'
      else
        event e1 assistant.message_start '{"messageId":"m1"}'
        event e2 assistant.message_delta '{"messageId":"m1","deltaContent":"Half a"}'
        exit 9
      fi
"#;

#[tokio::test]
async fn a_harness_crash_mid_turn_loses_the_session_and_the_next_prompt_resumes_it() {
    // The replacement process is asked to resume rather than create, because the Session it lost
    // has Resume State by the time the crash takes it.
    let copilot = resumable_conversation_fixture(CRASH_MID_TURN);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "copilot-harness-crash-mid-turn")
            .expect("configure server"),
        Arc::new(CopilotRuntime::new(copilot.executable())),
    )
    .await
    .expect("spawn server");
    let client = connect(state_dir.path(), "copilot-harness-crash-mid-turn").await;

    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Start something the harness will not finish".to_owned(),
                skill_invocations: Vec::new(),
            },
        })
        .await
        .expect("create Session");

    let lost = settled_session(&client, created.session.id, 0).await;
    assert_eq!(lost.turns[0].status, TurnStatus::Failed);
    let [Activity::Error { text, .. }] = lost.activities.as_slice() else {
        panic!(
            "a lost harness settles the Turn with an Error Activity, got {:?}",
            lost.activities
        );
    };
    assert!(
        text.contains("Copilot CLI server exited unexpectedly"),
        "the failure says the harness process is what went, got: {text}"
    );
    assert_eq!(
        agent_messages(&lost)[0].content,
        "Half a",
        "what the lost Turn had already streamed survives it"
    );

    let mut feed = client
        .subscribe_session(created.session.id)
        .await
        .expect("re-subscribe to the surviving Session");
    client
        .admit_prompt(
            created.session.id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Try again".to_owned(),
                    skill_invocations: Vec::new(),
                },
                delivery: PromptDelivery::Queue,
            },
        )
        .await
        .expect("the Session survives its lost harness");

    let recovered = settled_session_on(&client, &mut feed, created.session.id, 1).await;

    assert_eq!(recovered.turns[1].status, TurnStatus::Completed);
    assert_eq!(agent_messages(&recovered)[1].content, "Back again");
    assert_eq!(
        copilot.launches(),
        2,
        "the demand after a crash launches a fresh process, with no restart in between"
    );
    assert!(
        copilot.methods().contains(&"session.resume".to_owned()),
        "the respawned harness picks the lost Copilot Session back up rather than opening one: {:?}",
        copilot.methods()
    );

    drop(feed);
    server.shutdown().await.expect("shut the server down");
}

/// A first Turn that fails only once the test has queued a second Prompt behind it, and a second
/// Turn that answers normally.
const QUEUED_PROMPT_AFTER_ERROR: &str = r#"      sends=$(( ${sends:-0} + 1 ))
      if [ "$sends" -eq 1 ]; then
        while [ ! -e "$COPILOT_FIXTURE_RELEASE" ]; do sleep 0.01; done
        event e1 session.error '{"errorType":"quota","message":"Out of premium requests."}'
        event e2 session.idle '{}'
      else
        event e3 assistant.message '{"messageId":"m2","content":"Second answer"}'
        event e4 session.idle '{}'
      fi
"#;

#[tokio::test]
async fn a_prompt_queued_behind_a_failed_turn_runs_rather_than_settling_on_its_idle() {
    let copilot = conversation_fixture(QUEUED_PROMPT_AFTER_ERROR);
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let workspace = tempfile::tempdir().expect("create valid Workspace");
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "copilot-queued-after-error")
            .expect("configure server"),
        Arc::new(CopilotRuntime::new(copilot.executable())),
    )
    .await
    .expect("spawn server");
    let client = connect(state_dir.path(), "copilot-queued-after-error").await;

    let created = client
        .create_session(CreateSessionRequest {
            preparation_id: None,
            agent_selection: None,
            execution_directory: suru::protocol::ExecutionDirectory {
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
    let mut feed = client
        .subscribe_session(created.session.id)
        .await
        .expect("subscribe to Session SSE");

    // Queue the second Prompt while the first Turn is still running, so the failed Turn's settle
    // is what delivers it — the moment a stale idle would land on the wrong Turn.
    copilot.wait_for_request("session.send").await;
    client
        .admit_prompt(
            created.session.id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Try again".to_owned(),
                    skill_invocations: Vec::new(),
                },
                delivery: PromptDelivery::Queue,
            },
        )
        .await
        .expect("queue a Prompt behind the running Turn");
    copilot.release();

    let recovered = settled_session_on(&client, &mut feed, created.session.id, 1).await;

    assert_eq!(recovered.turns[0].status, TurnStatus::Failed);
    assert_eq!(
        recovered.turns[1].status,
        TurnStatus::Completed,
        "the queued Turn settles on its own idle, not the failed Turn's"
    );
    assert_eq!(
        agent_messages(&recovered)
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        ["Second answer"],
        "the queued Turn runs to an answer rather than completing empty"
    );

    drop(feed);
    server.shutdown().await.expect("shut the server down");
}

#[tokio::test]
async fn managed_worktree_native_copilot_starts_at_prepared_root() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}{}{}{}{}",
        crate::support::conversation_arms(),
        crate::support::resume_session_arm(),
        crate::support::skills_reload_arm(),
        crate::support::destroy_session_arm(),
        crate::support::delete_session_arm(),
        r#"
    *'"method":"session.commands.list"'*) reply '{"jsonrpc":"2.0","id":'"$id"',"result":{"commands":[]}}' ;;
    *'"method":"session.skills.list"'*) reply '{"jsonrpc":"2.0","id":'"$id"',"result":{"skills":[]}}' ;;
"#,
        send_arm(STREAMED_MESSAGE)
    ));
    let state = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    let server = server::spawn_with_provider_and_timings(
        ServerConfig::new(state.path(), "prepared-copilot").unwrap(),
        Arc::new(CopilotRuntime::new(copilot.executable())),
        server::ServerTimings::default()
            .with_checkout_skill_timeout(tokio::time::Duration::from_secs(2)),
    )
    .await
    .unwrap();
    let client = connect(state.path(), "prepared-copilot").await;
    let prepared = crate::managed_worktree::prepare(&client, source.path(), "copilot").await;
    assert!(
        !copilot
            .requests()
            .iter()
            .any(|r| r["method"] == "session.send")
    );
    let created = client
        .create_session(crate::managed_worktree::creation(&prepared))
        .await
        .unwrap();
    settled_session(&client, created.session.id, 0).await;
    let request = copilot
        .requests()
        .into_iter()
        .find(|r| r["method"] == "session.create" && r["params"]["streaming"] == true)
        .unwrap();
    assert_eq!(
        request["params"]["workingDirectory"].as_str(),
        prepared.destination.path.to_str()
    );
    crate::managed_worktree::recover(&client, created.session.id, &prepared.destination.path).await;
    let resumed = copilot
        .requests()
        .into_iter()
        .find(|r| r["method"] == "session.resume")
        .expect("recovered native connection resumes");
    assert_eq!(
        resumed["params"]["workingDirectory"].as_str(),
        prepared.destination.path.to_str()
    );
    server.shutdown().await.unwrap();
}
