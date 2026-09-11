//! Steering a Claude Turn that is already under way: a Prompt delivered into the running loop
//! rather than one that begins another Turn.

use crate::support::{
    CLAUDE_MODELS, LiveTurn, ScriptedClaude, agent_messages, discovery_arms, settled_session,
    user_turn_arm,
};
use serde_json::Value;
use suru::{
    protocol::{
        AdmitPromptRequest, Cost, CostBasis, InitialPrompt, MessageRole, MessageStatus,
        PromptDelivery, PromptId, PromptStatus, TurnStatus, Usage,
    },
    provider::ClaudeRuntime,
};

/// A conversation whose first Prompt streams the opening of an answer and leaves its block open,
/// and whose steer ends that stretch and answers again under the redirection.
///
/// The two stretches are how the CLI itself answers a steer: the message queued into the running
/// loop is answered after the loop it joined, so the wire carries a terminal `result` for each —
/// the first one arriving only once the steer is in hand, which is what a mid-Turn steer looks
/// like. Everything past the steer waits on the release, so a test can read the Session while the
/// steered Turn is still running.
const STEERED_CONVERSATION: &str = r#"      prompts=$(( ${prompts:-0} + 1 ))
      if [ "$prompts" -eq 1 ]; then
        emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Hello"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      else
        (
          while [ ! -e "$CLAUDE_FIXTURE_RELEASE" ]; do sleep 0.01; done
          emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
          emit '{"type":"result","uuid":"result-1","subtype":"success","is_error":false,"duration_ms":9,"num_turns":1,"result":"Hello","terminal_reason":"completed","session_id":"prov-session","usage":{"input_tokens":10,"output_tokens":5},"total_cost_usd":0.01}'
          emit '{"type":"result","uuid":"result-1","subtype":"success","is_error":false,"duration_ms":9,"num_turns":1,"result":"Hello","terminal_reason":"completed","session_id":"prov-session","usage":{"input_tokens":10,"output_tokens":5},"total_cost_usd":0.01}'
          emit '{"type":"system","subtype":"init","session_id":"prov-session","model":"claude-fixture-1"}'
          emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Bonjour"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
          emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
          emit '{"type":"result","uuid":"result-2","subtype":"success","is_error":false,"duration_ms":4,"num_turns":1,"result":"Bonjour","terminal_reason":"completed","session_id":"prov-session","usage":{"input_tokens":20,"output_tokens":7},"total_cost_usd":0.02}'
        ) &
      fi
"#;

#[tokio::test]
async fn a_steer_prompt_joins_the_running_turn_rather_than_beginning_another() {
    let claude = ScriptedClaude::new(&format!(
        "{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(STEERED_CONVERSATION)
    ));
    let mut live = LiveTurn::start(
        ClaudeRuntime::new(claude.executable()),
        "claude-steering",
        "Say hello",
    )
    .await;

    let steer = live
        .client
        .admit_prompt(
            live.session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Answer in French instead".to_owned(),
                    skill_invocations: Vec::new(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("steer the running Turn");
    assert_eq!(steer.status, PromptStatus::Pending);

    let steered = live
        .wait_for("the steer Prompt reaches the running loop", |snapshot| {
            snapshot
                .prompts
                .iter()
                .any(|prompt| prompt.id == steer.id && prompt.status == PromptStatus::Delivered)
                && !agent_messages(snapshot).is_empty()
        })
        .await;

    assert_eq!(
        steered.turns.len(),
        1,
        "a steer joins the Turn in flight rather than beginning another"
    );
    assert_eq!(steered.turns[0].status, TurnStatus::Active);
    let asked = steered
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::User)
        .collect::<Vec<_>>();
    assert_eq!(
        asked
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        ["Say hello", "Answer in French instead"]
    );
    assert_eq!(
        asked[1].turn_id, live.turn_id,
        "the steer's Message belongs to the Turn it steered"
    );
    assert_eq!(
        agent_messages(&steered)[0].status,
        MessageStatus::Streaming,
        "the answer the steer interrupted is still streaming when the steer lands"
    );

    claude.release();
    let settled = settled_session(&live.client, live.session_id, 0).await;

    assert_eq!(settled.turns.len(), 1, "no second Turn ever began");
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    assert_eq!(
        settled.turns[0].usage,
        Some(Usage {
            fresh_input_tokens: Some(30),
            output_tokens: Some(12),
            ..Usage::default()
        }),
        "both result stretches contribute to the one Turn's Usage"
    );
    assert_eq!(settled.turns[0].cost, Cost::from_usd(0.02));
    assert_eq!(settled.turns[0].cost_basis, Some(CostBasis::Reported));
    assert_eq!(
        agent_messages(&settled)
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        ["Hello", "Bonjour"],
        "the stretch the steer joined and the one it redirected are both the same Turn's work"
    );

    let prompts = claude
        .requests()
        .into_iter()
        .filter(|request| request.get("type").and_then(Value::as_str) == Some("user"))
        .filter_map(|request| {
            request
                .pointer("/message/content/0/text")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        prompts,
        ["Say hello", "Answer in French instead"],
        "the steer reaches the CLI as another user message on the running loop's stdin"
    );

    live.shutdown().await;
}
