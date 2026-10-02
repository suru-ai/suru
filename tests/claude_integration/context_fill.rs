//! Native optional snapshots through the runtime, server, persistence and client contract.

use crate::server_support::PROGRESS_DEADLINE;
use std::time::Duration;

use suru::{
    managed_client::ManagedClient,
    protocol::{
        AdmitPromptRequest, AgentSelection, AgentSelectionOperationId, ContextFill,
        CreateSessionRequest, InitialPrompt, ModelId, PromptDelivery, PromptId, ProviderId,
        SessionId, SessionSnapshot, TurnStatus, UpdateAgentSelectionRequest,
    },
    provider::ClaudeRuntime,
    server::RunningServer,
};

use crate::support::{
    CLAUDE_MODELS, ScriptedClaude, discovery_arms, hosting_runtime, session_where, settled_session,
    user_turn_arm,
};

const INIT: &str = r#"emit '{"type":"system","subtype":"init","model":"claude-fixture-1"}'"#;
/// A compaction of the loop's own conversation: the boundary, and the summary the CLI writes
/// straight after it, which the boundary's completion waits for.
const COMPACT: &str = r#"emit '{"type":"system","subtype":"compact_boundary","compact_metadata":{"pre_tokens":999999}}'
emit '{"type":"user","isSynthetic":true,"message":{"role":"user","content":"This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.\n\nHalf done."}}'"#;
const RESULT: &str = r#"emit '{"type":"result","subtype":"success","is_error":false,"usage":{"input_tokens":900000,"output_tokens":100},"total_cost_usd":0.03}'"#;

fn response(value: &str) -> String {
    format!(
        r#"emit '{{"type":"control_response","response":{{"subtype":"success","request_id":"'"$request_id"'","response":{value}}}}}'"#
    )
}

fn snapshot(tokens: u64, raw: &str) -> String {
    response(&format!(
        r#"{{"totalTokens":{tokens},"rawMaxTokens":{raw},"maxTokens":180000,"model":"claude-fixture-1[1m]"}}"#
    ))
}

fn fixture(timeline: &str, query: &str) -> ScriptedClaude {
    ScriptedClaude::new(&format!(
        "{}{}\n *'\"subtype\":\"get_context_usage\"'*)\n{query}\n;;\n",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(timeline)
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
    async fn start(claude: &ScriptedClaude, timeout: Duration) -> Self {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let (server, client) = hosting_runtime(
            ClaudeRuntime::new(claude.executable()).with_context_request_timeout(timeout),
            "claude-context",
            state.path(),
        )
        .await;
        let created = client
            .create_session(CreateSessionRequest {
                session_id: None,
                preparation_id: None,
                agent_selection: None,
                execution_directory: suru::protocol::ExecutionDirectory {
                    path: workspace.path().to_owned(),
                },
                prompt: prompt(),
            })
            .await
            .unwrap();
        Self {
            state,
            _workspace: workspace,
            server,
            client,
            id: created.session.id,
        }
    }

    async fn wait(&self, predicate: impl Fn(&SessionSnapshot) -> bool) -> SessionSnapshot {
        let mut feed = self.client.subscribe_session(self.id).await.unwrap();
        session_where(
            &self.client,
            &mut feed,
            self.id,
            "expected Claude context state",
            predicate,
        )
        .await
    }

    async fn next_turn(&self) {
        self.client
            .admit_prompt(
                self.id,
                AdmitPromptRequest {
                    prompt: prompt(),
                    delivery: PromptDelivery::Queue,
                },
            )
            .await
            .unwrap();
    }

    async fn shutdown(self) {
        self.server.shutdown().await.unwrap();
    }
}

fn prompt() -> InitialPrompt {
    InitialPrompt {
        id: PromptId::new(),
        text: "Measure context".to_owned(),
        skill_invocations: vec![],
        attachments: Vec::new(),
    }
}

fn fill(tokens: u64, capacity: Option<u64>) -> Option<ContextFill> {
    Some(ContextFill {
        occupied_tokens: tokens,
        capacity_tokens: capacity,
    })
}

async fn wait_for_queries(claude: &ScriptedClaude, count: usize) {
    tokio::time::timeout(PROGRESS_DEADLINE, async {
        while claude
            .control_subtypes()
            .iter()
            .filter(|s| *s == "get_context_usage")
            .count()
            < count
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("native context query reaches CLI");
}

#[tokio::test]
async fn compaction_and_settlement_refresh_raw_context_and_reopening_restores_it() {
    let query = format!(
        r#"
        queries=$(( ${{queries:-0}} + 1 ))
        if [ "$queries" -eq 1 ]; then
            {}
            (
                while [ ! -e "$CLAUDE_FIXTURE_RELEASE" ]; do sleep 0.01; done
                {RESULT}
            ) &
        else
            {}
        fi
    "#,
        snapshot(12400, "1000000"),
        snapshot(4000, "1000000")
    );
    let claude = fixture(&format!("{INIT}\n{COMPACT}"), &query);
    let session = Session::start(&claude, Duration::from_millis(500)).await;
    let active = session
        .wait(|s| s.session.context_fill == fill(12400, Some(1000000)))
        .await;
    assert_eq!(active.turns[0].status, TurnStatus::Active);
    claude.release();
    let settled = session
        .wait(|s| {
            s.session.context_fill == fill(4000, Some(1000000))
                && s.turns[0].status == TurnStatus::Completed
        })
        .await;
    assert_eq!(
        settled.turns[0].usage.as_ref().unwrap().fresh_input_tokens,
        Some(900000)
    );
    assert_eq!(settled.turns[0].cost, suru::protocol::Cost::from_usd(0.03));
    assert_eq!(
        claude
            .control_subtypes()
            .iter()
            .filter(|s| *s == "get_context_usage")
            .count(),
        2
    );
    let id = session.id;
    session.server.shutdown().await.unwrap();
    let (server, client) = hosting_runtime(
        ClaudeRuntime::new(claude.executable()),
        "claude-context",
        session.state.path(),
    )
    .await;
    let restored = client.read_session(id).await.unwrap();
    assert_eq!(restored.session.context_fill, fill(4000, Some(1000000)));
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn settlement_and_another_turn_do_not_wait_for_context_and_old_turn_reply_is_rejected() {
    let query = format!(
        r#"
        queries=$(( ${{queries:-0}} + 1 ))
        if [ "$queries" -eq 1 ]; then
            old_id=$request_id
        else
            {}
            request_id=$old_id
            {}
        fi
    "#,
        snapshot(4000, "1000000"),
        snapshot(90000, "1000000")
    );
    let claude = fixture(&format!("{INIT}\n{RESULT}"), &query);
    let session = Session::start(&claude, Duration::from_secs(2)).await;
    let settled = settled_session(&session.client, session.id, 0).await;
    assert_eq!(settled.session.context_fill, None);
    wait_for_queries(&claude, 1).await;
    session.next_turn().await;
    session
        .wait(|s| {
            s.turns.len() == 2
                && s.turns[1].status == TurnStatus::Completed
                && s.session.context_fill == fill(4000, Some(1000000))
        })
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        session
            .client
            .read_session(session.id)
            .await
            .unwrap()
            .session
            .context_fill,
        fill(4000, Some(1000000))
    );
    session.shutdown().await;
}

#[tokio::test]
async fn out_of_order_compaction_reply_cannot_overwrite_newer_settlement_snapshot() {
    let query = format!(
        r#"
        queries=$(( ${{queries:-0}} + 1 ))
        if [ "$queries" -eq 1 ]; then
            old_id=$request_id
            {RESULT}
        else
            {}
            request_id=$old_id
            {}
        fi
    "#,
        snapshot(4000, "1000000"),
        snapshot(90000, "1000000")
    );
    let claude = fixture(&format!("{INIT}\n{COMPACT}"), &query);
    let session = Session::start(&claude, Duration::from_secs(2)).await;
    session
        .wait(|s| s.session.context_fill == fill(4000, Some(1000000)))
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        session
            .client
            .read_session(session.id)
            .await
            .unwrap()
            .session
            .context_fill,
        fill(4000, Some(1000000))
    );
    session.shutdown().await;
}

#[tokio::test]
async fn invalid_raw_capacity_is_count_only_and_invalid_occupancy_never_uses_usage() {
    for (native, expected) in [
        (r#"{"totalTokens":0,"maxTokens":180000,"model":"claude-fixture-1"}"#.to_owned(), fill(0, None)),
        (r#"{"totalTokens":12,"rawMaxTokens":0,"maxTokens":180000,"model":"claude-fixture-1"}"#.to_owned(), fill(12, None)),
        (r#"{"totalTokens":12,"rawMaxTokens":-1,"maxTokens":180000,"model":"claude-fixture-1"}"#.to_owned(), fill(12, None)),
        (r#"{"totalTokens":12,"rawMaxTokens":"invalid","maxTokens":180000,"model":"claude-fixture-1"}"#.to_owned(), fill(12, None)),
        (r#"{"rawMaxTokens":1000000,"model":"claude-fixture-1"}"#.to_owned(), None),
        (r#"{"totalTokens":-1,"rawMaxTokens":1000000,"model":"claude-fixture-1"}"#.to_owned(), None),
    ] {
        let claude = fixture(&format!("{INIT}\n{RESULT}"), &response(&native));
        let session = Session::start(&claude, Duration::from_millis(100)).await;
        settled_session(&session.client, session.id, 0).await;
        wait_for_queries(&claude, 1).await;
        if expected.is_some() { session.wait(|s| s.session.context_fill == expected).await; }
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(session.client.read_session(session.id).await.unwrap().session.context_fill, expected, "{native}");
        session.shutdown().await;
    }
}

#[tokio::test]
async fn unsupported_errors_and_timeouts_preserve_the_previous_measurement() {
    for failure in [
        r#"emit '{"type":"control_response","response":{"subtype":"error","request_id":"'"$request_id"'","error":"Unsupported control request subtype: get_context_usage"}}'"#,
        r#"emit '{"type":"control_response","response":{"subtype":"error","request_id":"'"$request_id"'","error":"context unavailable"}}'"#,
        ":",
    ] {
        let query = format!(
            r#"
            queries=$(( ${{queries:-0}} + 1 ))
            if [ "$queries" -eq 1 ]; then
                {}
            else
                {failure}
            fi
        "#,
            snapshot(12400, "1000000")
        );
        let claude = fixture(&format!("{INIT}\n{RESULT}"), &query);
        let session = Session::start(&claude, Duration::from_millis(40)).await;
        session
            .wait(|s| s.session.context_fill == fill(12400, Some(1000000)))
            .await;
        session.next_turn().await;
        settled_session(&session.client, session.id, 1).await;
        wait_for_queries(&claude, 2).await;
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(
            session
                .client
                .read_session(session.id)
                .await
                .unwrap()
                .session
                .context_fill,
            fill(12400, Some(1000000))
        );
        session.shutdown().await;
    }
}

#[tokio::test]
async fn a_model_change_invalidates_context_and_wrong_model_responses_do_not_revive_it() {
    let timeline = format!(
        r#"
        case "$*" in
            *'middling'*) emit '{{"type":"system","subtype":"init","model":"claude-fixture-2"}}' ;;
            *) {INIT} ;;
        esac
        {RESULT}
    "#
    );
    let claude = fixture(&timeline, &snapshot(12400, "1000000"));
    let session = Session::start(&claude, Duration::from_millis(100)).await;
    session
        .wait(|s| s.session.context_fill == fill(12400, Some(1000000)))
        .await;
    session
        .client
        .update_agent_selection(
            session.id,
            UpdateAgentSelectionRequest {
                operation_id: AgentSelectionOperationId::new(),
                selection: AgentSelection {
                    provider: ProviderId::new("claude"),
                    model: ModelId::new("middling"),
                    options: vec![],
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(
        session
            .client
            .read_session(session.id)
            .await
            .unwrap()
            .session
            .context_fill,
        fill(12400, Some(1000000))
    );
    session.next_turn().await;
    settled_session(&session.client, session.id, 1).await;
    wait_for_queries(&claude, 2).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        session
            .client
            .read_session(session.id)
            .await
            .unwrap()
            .session
            .context_fill,
        None
    );
    session.shutdown().await;
}

#[tokio::test]
async fn child_compaction_does_not_query_or_replace_the_parents_context() {
    let timeline = format!(
        r#"{INIT}
        emit '{{"type":"system","subtype":"compact_boundary","parent_tool_use_id":"child-tool","compact_metadata":{{"pre_tokens":999999}}}}'
        {RESULT}"#
    );
    let claude = fixture(&timeline, &snapshot(12400, "1000000"));
    let session = Session::start(&claude, Duration::from_millis(100)).await;
    session
        .wait(|s| s.session.context_fill == fill(12400, Some(1000000)))
        .await;
    assert_eq!(
        claude
            .control_subtypes()
            .iter()
            .filter(|s| *s == "get_context_usage")
            .count(),
        1
    );
    session.shutdown().await;
}

#[tokio::test]
async fn late_child_outcome_continuation_refreshes_only_the_owning_session() {
    let timeline = format!(
        r#"{INIT}
        emit '{{"type":"system","subtype":"task_started","task_id":"agent-task-bg","tool_use_id":"task_bg","description":"Audit","task_type":"local_agent","subagent_type":"general-purpose"}}'
        {RESULT}"#
    );
    let query = format!(
        r#"
        queries=$(( ${{queries:-0}} + 1 ))
        if [ "$queries" -eq 1 ]; then
            {}
            (
                while [ ! -e "$CLAUDE_FIXTURE_RELEASE" ]; do sleep 0.01; done
                emit '{{"type":"system","subtype":"task_notification","task_id":"agent-task-bg","status":"completed","summary":"Done"}}'
                emit '{{"type":"stream_event","event":{{"type":"content_block_start","index":0,"content_block":{{"type":"text","text":"Child finished"}}}},"parent_tool_use_id":null}}'
                emit '{{"type":"stream_event","event":{{"type":"content_block_stop","index":0}},"parent_tool_use_id":null}}'
                {RESULT}
            ) &
        else
            {}
        fi
    "#,
        snapshot(12400, "1000000"),
        snapshot(4000, "1000000")
    );
    let claude = fixture(&timeline, &query);
    let session = Session::start(&claude, Duration::from_millis(500)).await;
    let first = session
        .wait(|s| s.session.context_fill == fill(12400, Some(1000000)))
        .await;
    let child_id = first
        .activities
        .iter()
        .find_map(|activity| {
            if let suru::protocol::Activity::Subagent { session_id, .. } = activity {
                Some(*session_id)
            } else {
                None
            }
        })
        .expect("native task opens a child Session");
    claude.release();
    let continued = session
        .wait(|s| {
            s.turns.len() == 2
                && s.turns[1].status == TurnStatus::Completed
                && s.session.context_fill == fill(4000, Some(1000000))
        })
        .await;
    assert!(continued.turns[1].prompt_id.is_none());
    assert_eq!(
        session
            .client
            .read_session(child_id)
            .await
            .unwrap()
            .session
            .context_fill,
        None
    );
    session.shutdown().await;
}

#[tokio::test]
async fn unavailable_context_with_no_snapshot_stays_unknown_after_timeout() {
    let claude = fixture(&format!("{INIT}\n{RESULT}"), ":");
    let session = Session::start(&claude, Duration::from_millis(30)).await;
    let settled = settled_session(&session.client, session.id, 0).await;
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    wait_for_queries(&claude, 1).await;
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(
        session
            .client
            .read_session(session.id)
            .await
            .unwrap()
            .session
            .context_fill,
        None
    );
    session.next_turn().await;
    settled_session(&session.client, session.id, 1).await;
    session.shutdown().await;
}

#[tokio::test]
async fn an_old_query_cannot_replace_a_newer_reading_when_a_continuation_begins() {
    let timeline = format!(
        r#"{INIT}
        emit '{{"type":"system","subtype":"task_started","task_id":"agent-task-bg","tool_use_id":"task_bg","description":"Audit","task_type":"local_agent","subagent_type":"general-purpose"}}'
        {COMPACT}"#
    );
    let query = format!(
        r#"
        queries=$(( ${{queries:-0}} + 1 ))
        if [ "$queries" -eq 1 ]; then
            old_id=$request_id
            {RESULT}
        elif [ "$queries" -eq 2 ]; then
            {}
            (
                while [ ! -e "$CLAUDE_FIXTURE_RELEASE" ]; do sleep 0.01; done
                emit '{{"type":"stream_event","event":{{"type":"content_block_start","index":0,"content_block":{{"type":"text","text":"Child outcome"}}}},"parent_tool_use_id":null}}'
                request_id=$old_id
                {}
                while [ ! -e "$CLAUDE_FIXTURE_SIGNED_IN" ]; do sleep 0.01; done
                emit '{{"type":"stream_event","event":{{"type":"content_block_stop","index":0}},"parent_tool_use_id":null}}'
                {RESULT}
            ) &
        else
            {}
        fi
    "#,
        snapshot(12400, "1000000"),
        snapshot(90000, "1000000"),
        snapshot(4000, "1000000")
    );
    let claude = fixture(&timeline, &query);
    let session = Session::start(&claude, Duration::from_secs(2)).await;
    session
        .wait(|s| s.session.context_fill == fill(12400, Some(1000000)))
        .await;
    claude.release();
    session
        .wait(|s| s.turns.len() == 2 && s.turns[1].status == TurnStatus::Active)
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        session
            .client
            .read_session(session.id)
            .await
            .unwrap()
            .session
            .context_fill,
        fill(12400, Some(1000000))
    );
    claude.sign_in();
    session
        .wait(|s| s.session.context_fill == fill(4000, Some(1000000)))
        .await;
    session.shutdown().await;
}

#[tokio::test]
async fn failed_new_model_setup_cannot_query_old_native_context_as_the_failed_turn() {
    use futures_util::StreamExt;
    use suru::{
        protocol::{
            ModelOptionChoiceId, ModelOptionId, ModelOptionSelection, ModelOptionValue, TurnId,
        },
        provider::{
            ProviderEvent, ProviderInput, ProviderPrompt, ProviderRuntime, ProviderSessionRequest,
            ProviderTurnInput,
        },
    };
    let query = format!(
        r#"
        {}
        (
            while [ ! -e "$CLAUDE_FIXTURE_RELEASE" ]; do sleep 0.01; done
            emit '{{"type":"stream_event","event":{{"type":"content_block_start","index":0,"content_block":{{"type":"text","text":"Old output"}}}},"parent_tool_use_id":null}}'
            {COMPACT}
            {RESULT}
        ) &
    "#,
        snapshot(12400, "1000000")
    );
    let claude = fixture(&format!("{INIT}\n{RESULT}"), &query);
    let workspace = tempfile::tempdir().unwrap();
    let runtime = ClaudeRuntime::new(claude.executable())
        .with_context_request_timeout(Duration::from_millis(100));
    let (identity, _, session, mut events) = runtime
        .start_session(ProviderSessionRequest {
            execution_directory: workspace.path().to_owned(),
            resume_state: None,
            approval_posture: None,
            broker: None,
        })
        .await
        .unwrap()
        .into_parts();
    session
        .start_turn(ProviderTurnInput {
            turn_id: TurnId::new(),
            input: ProviderInput::from_prompt(ProviderPrompt::plain("First")),
            selection: identity.selection,
            approval_posture: None,
        })
        .await
        .unwrap();
    tokio::time::timeout(PROGRESS_DEADLINE, async {
        while let Some(event) = events.next().await {
            if matches!(event.unwrap().event, ProviderEvent::ContextFill { .. }) {
                return;
            }
        }
        panic!("first Turn reports context");
    })
    .await
    .unwrap();
    let error = session
        .start_turn(ProviderTurnInput {
            turn_id: TurnId::new(),
            input: ProviderInput::from_prompt(ProviderPrompt::plain("New Model")),
            selection: AgentSelection {
                provider: ProviderId::new("claude"),
                model: ModelId::new("middling"),
                options: vec![ModelOptionSelection {
                    id: ModelOptionId::new("unsupported"),
                    value: ModelOptionValue::Select {
                        choice: ModelOptionChoiceId::new("anything"),
                    },
                }],
            },
            approval_posture: None,
        })
        .await
        .expect_err("unsupported native option fails before replacing the old child");
    assert!(error.to_string().contains("unsupported"), "{error}");
    claude.release();
    let unexpected = tokio::time::timeout(Duration::from_millis(150), async {
        while let Some(event) = events.next().await {
            if matches!(event.unwrap().event, ProviderEvent::ContextFill { .. }) {
                return;
            }
        }
    })
    .await;
    assert!(
        unexpected.is_err(),
        "old compaction cannot produce a report under the failed new Turn ID"
    );
    assert_eq!(
        claude
            .control_subtypes()
            .iter()
            .filter(|s| *s == "get_context_usage")
            .count(),
        1
    );
    session.shutdown().await.unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_usage_only_continuation_refreshes_context_after_settlement() {
    let timeline = format!(
        r#"{INIT}
        emit '{{"type":"system","subtype":"task_started","task_id":"agent-task-bg","tool_use_id":"task_bg","description":"Audit","task_type":"local_agent","subagent_type":"general-purpose"}}'
        {RESULT}"#
    );
    let query = format!(
        r#"
        queries=$(( ${{queries:-0}} + 1 ))
        if [ "$queries" -eq 1 ]; then
            {}
            (
                while [ ! -e "$CLAUDE_FIXTURE_RELEASE" ]; do sleep 0.01; done
                {RESULT}
            ) &
        else
            {}
        fi
    "#,
        snapshot(12400, "1000000"),
        snapshot(4000, "1000000")
    );
    let claude = fixture(&timeline, &query);
    let session = Session::start(&claude, Duration::from_millis(500)).await;
    session
        .wait(|s| s.session.context_fill == fill(12400, Some(1000000)))
        .await;
    claude.release();
    let continued = session
        .wait(|s| {
            s.turns.len() == 2
                && s.turns[1].status == TurnStatus::Completed
                && s.session.context_fill == fill(4000, Some(1000000))
        })
        .await;
    assert!(continued.turns[1].prompt_id.is_none());
    session.shutdown().await;
}
