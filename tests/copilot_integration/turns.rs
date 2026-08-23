//! Copilot Turns: a delivered Prompt streaming an agent Message into the Transcript, the Turn
//! settling, and what happens to it when Copilot reports an error or its harness process dies.

use std::sync::Arc;

use crate::support::{
    agent_messages, connect, conversation_fixture, settled_session, settled_session_on,
};
use serde_json::Value;
use suru::{
    protocol::{
        Activity, AdmitPromptRequest, AgentSelection, CreateSessionRequest, InitialPrompt,
        MessageStatus, ModelId, ModelOptionChoiceId, ModelOptionId, ModelOptionSelection,
        ModelOptionValue, PromptDelivery, PromptId, PromptStatus, ProviderId, TurnStatus,
        Workspace,
    },
    provider::CopilotRuntime,
    server::{self, ServerConfig},
};

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
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Say hello".to_owned(),
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
    let server = server::spawn_with_provider(
        ServerConfig::new(state_dir.path(), "copilot-model-switch").expect("configure server"),
        Arc::new(CopilotRuntime::new(copilot.executable())),
    )
    .await
    .expect("spawn server");
    let client = connect(state_dir.path(), "copilot-model-switch").await;
    // The Agent Selection is normalized against the catalog, so discover it before naming one.
    client.list_models().await.expect("discover Copilot Models");

    let created = client
        .create_session(CreateSessionRequest {
            agent_selection: Some(selection("claude-fixture", "low", "long_context")),
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Think less, read more".to_owned(),
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
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Run something that needs permission".to_owned(),
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
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Spend the quota".to_owned(),
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
async fn a_harness_crash_mid_turn_loses_the_session_and_the_next_prompt_respawns_it() {
    let copilot = conversation_fixture(CRASH_MID_TURN);
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
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Start something the harness will not finish".to_owned(),
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
            agent_selection: None,
            workspace: Workspace {
                path: workspace.path().to_owned(),
            },
            prompt: InitialPrompt {
                id: PromptId::new(),
                text: "Spend the quota".to_owned(),
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
