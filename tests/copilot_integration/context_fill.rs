//! Native ephemeral context snapshots through Server updates and durable Session state.
use crate::support::{conversation_fixture, hosting, session_where};
use suru::protocol::{
    Activity, ContextFill, CreateSessionRequest, InitialPrompt, PromptId, TurnStatus,
};

const CONTEXT_TIMELINE: &str = r#"      context_event() {
        reply '{"jsonrpc":"2.0","method":"session.event","params":{"sessionId":"'"$sid"'","event":{"id":"'"$1"'","timestamp":"2026-01-01T00:00:00Z","ephemeral":true,"agentId":'"$2"',"type":"session.usage_info","data":'"$3"'}}}'
      }
      context_event c1 null '{"currentTokens":12400,"tokenLimit":128000,"messagesLength":3}'
      agent_event spawn agent-1 subagent.started '{"toolCallId":"t-spawn","agentName":"researcher","agentDisplayName":"Researcher","agentDescription":"Scout"}'
      context_event child1 '"agent-1"' '{"currentTokens":2000,"tokenLimit":64000,"messagesLength":1}'
      (
        while [ ! -e "$COPILOT_FIXTURE_RELEASE" ]; do sleep 0.01; done
        event usage assistant.usage '{"model":"claude-fixture","inputTokens":1000,"outputTokens":200,"cost":1}'
        event answer assistant.message '{"messageId":"m1","content":"Measured"}'
        event idle session.idle '{}'
        context_event c2 null '{"currentTokens":4000}'
        context_event invalid null '{"currentTokens":-1,"tokenLimit":200000}'
        context_event absent null '{"tokenLimit":200000}'
        agent_event complete agent-1 subagent.completed '{"toolCallId":"t-spawn","agentName":"researcher","agentDisplayName":"Researcher"}'
        context_event child2 '"agent-1"' '{"currentTokens":0,"tokenLimit":0}'
        context_event orphan '"unknown-agent"' '{"currentTokens":999999}'
      ) &
"#;

#[tokio::test]
async fn ephemeral_context_is_live_independent_replaceable_and_durable() {
    let copilot = conversation_fixture(CONTEXT_TIMELINE);
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let channel = "copilot-context-fill";
    let (server, client) = hosting(&copilot, channel, state.path()).await;
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
                text: "Measure context".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .await
        .unwrap();
    let id = created.session.id;
    let mut feed = client.subscribe_session(id).await.unwrap();
    let live = session_where(&client, &mut feed, id, "live native occupancy", |s| {
        s.session.context_fill == Some(within(12400, 128_000))
            && s.activities
                .iter()
                .any(|a| matches!(a, Activity::Subagent { .. }))
    })
    .await;
    assert_eq!(live.turns[0].status, TurnStatus::Active);
    assert_eq!(live.total_usage(), None, "occupancy is not consumption");
    let child = live
        .activities
        .iter()
        .find_map(|a| match a {
            Activity::Subagent { session_id, .. } => Some(*session_id),
            _ => None,
        })
        .unwrap();
    let mut child_feed = client.subscribe_session(child).await.unwrap();
    session_where(
        &client,
        &mut child_feed,
        child,
        "child owns its occupancy",
        |s| s.session.context_fill == Some(within(2000, 64_000)),
    )
    .await;
    copilot.release();
    let settled = session_where(
        &client,
        &mut feed,
        id,
        "post-idle compaction decreases occupancy",
        |s| {
            s.session.context_fill == Some(fill(4000)) && s.turns[0].status == TurnStatus::Completed
        },
    )
    .await;
    assert_eq!(
        settled.turns.len(),
        1,
        "context does not start a Continuation"
    );
    assert!(settled.turns[0].usage.is_some());
    assert!(settled.turns[0].cost.is_some(), "Cost remains independent");
    let settled_child = session_where(
        &client,
        &mut child_feed,
        child,
        "settled child accepts a reported zero",
        |s| s.session.context_fill == Some(fill(0)) && s.turns[0].status == TurnStatus::Completed,
    )
    .await;
    assert_eq!(settled_child.turns.len(), 1);
    assert_eq!(
        client.read_session(id).await.unwrap().session.context_fill,
        Some(fill(4000))
    );
    drop(child_feed);
    drop(feed);
    drop(client);
    server.shutdown().await.unwrap();

    let (server, client) = hosting(&copilot, channel, state.path()).await;
    let restored = client.read_session(id).await.unwrap();
    assert_eq!(restored.session.context_fill, Some(fill(4000)));
    assert_eq!(restored.turns[0].usage, settled.turns[0].usage);
    assert_eq!(restored.turns[0].cost, settled.turns[0].cost);
    assert_eq!(
        client
            .read_session(child)
            .await
            .unwrap()
            .session
            .context_fill,
        Some(fill(0))
    );
    drop(client);
    server.shutdown().await.unwrap();
}

fn fill(occupied_tokens: u64) -> ContextFill {
    ContextFill {
        occupied_tokens,
        capacity_tokens: None,
    }
}

/// A reading whose positive native `tokenLimit` gives its capacity.
fn within(occupied_tokens: u64, capacity_tokens: u64) -> ContextFill {
    ContextFill {
        occupied_tokens,
        capacity_tokens: Some(capacity_tokens),
    }
}

#[tokio::test]
async fn context_tracks_a_continuation_and_its_post_idle_snapshot() {
    let copilot = conversation_fixture(
        r#"
      event first_context session.usage_info '{"currentTokens":12000}'
      agent_event spawn agent-1 subagent.started '{"toolCallId":"t-spawn","agentName":"researcher","agentDisplayName":"Researcher","agentDescription":"Scout"}'
      event first_answer assistant.message '{"messageId":"m1","content":"Delegated"}'
      event first_idle session.idle '{}'
      agent_event complete agent-1 subagent.completed '{"toolCallId":"t-spawn","agentName":"researcher","agentDisplayName":"Researcher"}'
      event continuation assistant.message '{"messageId":"m2","content":"Research finished"}'
      event continuation_context session.usage_info '{"currentTokens":6000}'
      (
        while [ ! -e "$COPILOT_FIXTURE_RELEASE" ]; do sleep 0.01; done
        event continuation_idle session.idle '{}'
        event compacted_context session.usage_info '{"currentTokens":3000}'
      ) &
    "#,
    );
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (server, client) = hosting(&copilot, "copilot-continuation-context", state.path()).await;
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
                text: "Delegate".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .await
        .unwrap();
    let id = created.session.id;
    let mut feed = client.subscribe_session(id).await.unwrap();
    let live = session_where(
        &client,
        &mut feed,
        id,
        "continuation context is visible",
        |s| s.turns.len() == 2 && s.session.context_fill == Some(fill(6000)),
    )
    .await;
    assert_eq!(live.turns[0].status, TurnStatus::Completed);
    assert_eq!(live.turns[1].status, TurnStatus::Active);
    copilot.release();
    let settled = session_where(
        &client,
        &mut feed,
        id,
        "post-continuation context is visible",
        |s| {
            s.session.context_fill == Some(fill(3000)) && s.turns[1].status == TurnStatus::Completed
        },
    )
    .await;
    assert_eq!(settled.turns.len(), 2);
    drop(feed);
    drop(client);
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_failed_model_start_cannot_rebind_old_continuation_context() {
    failed_start_keeps_context_invalidated(false).await;
}

#[tokio::test]
async fn a_rejected_send_cannot_revive_context_after_switching_models() {
    failed_start_keeps_context_invalidated(true).await;
}

async fn failed_start_keeps_context_invalidated(reject_send: bool) {
    use crate::support::{
        ScriptedCopilot, connect, conversation_arms, send_arm, titling_turned_off,
    };
    use suru::{
        protocol::{
            AdmitPromptRequest, AgentSelection, AgentSelectionOperationId, ModelId, PromptDelivery,
            ProviderId, UpdateAgentSelectionRequest,
        },
        provider::CopilotRuntime,
        server::{self, ServerConfig},
    };
    let failed_start = if reject_send {
        r#"    *'"method":"session.send"'*'Use the other Model'*)
      event queued_context session.usage_info '{"currentTokens":98000}'
      reply '{"jsonrpc":"2.0","id":'"$id"',"error":{"code":-32600,"message":"fixture refused prompt"}}'
      event late_context session.usage_info '{"currentTokens":99000}'
      agent_event child_barrier agent-1 session.usage_info '{"currentTokens":777}'
      ;;
    "#
    } else {
        r#"    *'"method":"session.model.switchTo"'*'incomplete-cache-fixture'*)
      event stale_context session.usage_info '{"currentTokens":99000}'
      agent_event child_barrier agent-1 session.usage_info '{"currentTokens":777}'
      reply '{"jsonrpc":"2.0","id":'"$id"',"error":{"code":-32600,"message":"fixture refused model switch"}}'
      ;;
    "#
    };
    let timeline = r#"
      agent_event spawn agent-1 subagent.started '{"toolCallId":"t-spawn","agentName":"researcher","agentDisplayName":"Researcher","agentDescription":"Scout"}'
      event first_answer assistant.message '{"messageId":"m1","content":"Delegated"}'
      event first_idle session.idle '{}'
      agent_event complete agent-1 subagent.completed '{"toolCallId":"t-spawn","agentName":"researcher","agentDisplayName":"Researcher"}'
      event continuation assistant.message '{"messageId":"m2","content":"Research finished"}'
      event continuation_context session.usage_info '{"currentTokens":6000}'
      event continuation_idle session.idle '{}'
    "#;
    let copilot = ScriptedCopilot::new(&format!(
        "{failed_start}{}{}",
        conversation_arms(),
        send_arm(timeline)
    ));
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let titling = titling_turned_off();
    let channel = "copilot-failed-model-context";
    let server = server::spawn_with_provider(
        ServerConfig::new(state.path(), channel)
            .unwrap()
            .with_config_dir(titling.path()),
        std::sync::Arc::new(CopilotRuntime::new(copilot.executable())),
    )
    .await
    .unwrap();
    let client = connect(state.path(), channel).await;
    client.list_models().await.unwrap();
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
                text: "Delegate".to_owned(),
                skill_invocations: Vec::new(),
                attachments: Vec::new(),
            },
        })
        .await
        .unwrap();
    let id = created.session.id;
    let mut feed = client.subscribe_session(id).await.unwrap();
    let before = session_where(
        &client,
        &mut feed,
        id,
        "continuation settles with context",
        |s| {
            s.turns.len() == 2
                && s.turns[1].status == TurnStatus::Completed
                && s.session.context_fill == Some(fill(6000))
        },
    )
    .await;
    let child = before
        .activities
        .iter()
        .find_map(|a| match a {
            Activity::Subagent { session_id, .. } => Some(*session_id),
            _ => None,
        })
        .unwrap();
    let mut child_feed = client.subscribe_session(child).await.unwrap();
    client
        .update_agent_selection(
            id,
            UpdateAgentSelectionRequest {
                operation_id: AgentSelectionOperationId::new(),
                selection: AgentSelection {
                    provider: ProviderId::new("copilot"),
                    model: ModelId::new("incomplete-cache-fixture"),
                    options: Vec::new(),
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(
        client.read_session(id).await.unwrap().session.context_fill,
        Some(fill(6000))
    );
    client
        .admit_prompt(
            id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Use the other Model".to_owned(),
                    skill_invocations: Vec::new(),
                    attachments: Vec::new(),
                },
                delivery: PromptDelivery::Queue,
            },
        )
        .await
        .unwrap();
    session_where(&client, &mut feed, id, "new Model start fails", |s| {
        s.turns.len() == 3 && s.turns[2].status == TurnStatus::Failed
    })
    .await;
    // This later native event proves the old parent report has crossed projection.
    session_where(
        &client,
        &mut child_feed,
        child,
        "old report queue is drained",
        |s| s.session.context_fill == Some(fill(777)),
    )
    .await;
    assert_eq!(
        client.read_session(id).await.unwrap().session.context_fill,
        None
    );
    drop(child_feed);
    drop(feed);
    drop(client);
    server.shutdown().await.unwrap();
}
