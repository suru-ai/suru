//! Steering a Claude Turn that is already under way: a Prompt delivered into the running loop
//! rather than one that begins another Turn.

use crate::support::{
    CLAUDE_MODELS, LiveTurn, ScriptedClaude, agent_messages, discovery_arms, settled_session,
    user_turn_arm,
};
use serde_json::Value;
use suru::{
    protocol::{
        Activity, ActivityStatus, AdmitPromptRequest, Cost, CostBasis, InitialPrompt, MessageRole,
        MessageStatus, PromptDelivery, PromptId, PromptStatus, TurnStatus, Usage,
    },
    provider::ClaudeRuntime,
};

/// A conversation whose first Prompt streams the opening of an answer and leaves its block open,
/// and whose steer ends that stretch and answers again under the redirection.
///
/// The two stretches are how the CLI answers a steer that arrives while the loop's last request is
/// already streaming its answer (docs/validation/0407-claude-folded-steer.md, case D): the loop
/// makes no further request to fold the steer into, so the steer is still queued when the loop's
/// `result` is out, and it begins a loop of its own that ends with a `result` of its own. The CLI
/// says so under the uuid each message was written with — the steer is `queued` before the first
/// `result` and `started` only after it — which is what keeps the second loop inside the Turn the
/// steer joined. The first `result` arrives only once the steer is in hand, which is what a
/// mid-Turn steer looks like, and arrives twice, as a restated result can. Everything past the
/// steer waits on the release, so a test can read the Session while the steered Turn is still
/// running.
const STEERED_CONVERSATION: &str = r#"      prompts=$(( ${prompts:-0} + 1 ))
      if [ "$prompts" -eq 1 ]; then
        prompt=$uuid
        lifecycle "$prompt" queued
        lifecycle "$prompt" started
        emit '{"type":"system","subtype":"init","session_id":"prov-session","model":"claude-fixture-1"}'
        emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
        emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Hello"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      else
        steer=$uuid
        lifecycle "$steer" queued
        (
          while [ ! -e "$CLAUDE_FIXTURE_RELEASE" ]; do sleep 0.01; done
          emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
          emit '{"type":"result","uuid":"result-1","subtype":"success","is_error":false,"duration_ms":9,"num_turns":1,"result":"Hello","terminal_reason":"completed","session_id":"prov-session","usage":{"input_tokens":10,"output_tokens":5},"total_cost_usd":0.01}'
          emit '{"type":"result","uuid":"result-1","subtype":"success","is_error":false,"duration_ms":9,"num_turns":1,"result":"Hello","terminal_reason":"completed","session_id":"prov-session","usage":{"input_tokens":10,"output_tokens":5},"total_cost_usd":0.01}'
          lifecycle "$prompt" completed
          lifecycle "$steer" started
          emit '{"type":"system","subtype":"init","session_id":"prov-session","model":"claude-fixture-1"}'
          emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
          emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Bonjour"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
          emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
          emit '{"type":"result","uuid":"result-2","subtype":"success","is_error":false,"duration_ms":4,"num_turns":1,"result":"Bonjour","terminal_reason":"completed","session_id":"prov-session","usage":{"input_tokens":20,"output_tokens":7},"total_cost_usd":0.02}'
          lifecycle "$steer" completed
        ) &
      fi
"#;

/// A conversation whose first Prompt has the loop call Bash and wait on it, and whose steer —
/// written while that call runs — the loop folds into its next request at the tool round, so that
/// one `result` answers both (docs/validation/0407-claude-folded-steer.md, case A). The CLI reports
/// the steer `queued` as it reads it and `started` only once the tool result is back, and it
/// reports the steer `completed` before the `result` that answers it. Everything past the steer
/// waits on the release.
const FOLDED_CONVERSATION: &str = r#"      prompts=$(( ${prompts:-0} + 1 ))
      if [ "$prompts" -eq 1 ]; then
        prompt=$uuid
        lifecycle "$prompt" queued
        lifecycle "$prompt" started
        emit '{"type":"system","subtype":"init","session_id":"prov-session","model":"claude-fixture-1"}'
        emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
        emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_sleep","name":"Bash","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
        emit '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"sleep 20\"}"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
        emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
        emit '{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null,"session_id":"prov-session"}'
      else
        steer=$uuid
        lifecycle "$steer" queued
        (
          while [ ! -e "$CLAUDE_FIXTURE_RELEASE" ]; do sleep 0.01; done
          emit '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_sleep","content":"","is_error":false}]},"parent_tool_use_id":null,"session_id":"prov-session"}'
          lifecycle "$steer" started
          emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
          emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"DONE MARK"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
          emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
          lifecycle "$steer" completed
          emit '{"type":"result","uuid":"result-1","subtype":"success","is_error":false,"duration_ms":25799,"num_turns":2,"result":"DONE MARK","terminal_reason":"completed","session_id":"prov-session","usage":{"input_tokens":30,"output_tokens":9},"total_cost_usd":0.02}'
          lifecycle "$prompt" completed
        ) &
      fi
"#;

/// The same wire from a CLI that reports no message's lifecycle — which is what 2.1.283 writes for
/// messages sent without a uuid — so nothing tells a steer folded into the running loop from one
/// that will begin a loop of its own.
fn without_lifecycle(conversation: &str) -> String {
    conversation
        .lines()
        .filter(|line| !line.trim_start().starts_with("lifecycle "))
        .map(|line| format!("{line}\n"))
        .collect()
}

/// Admits `text` as a steer of the running Turn and comes back once it has reached the loop.
async fn steer(live: &mut LiveTurn, text: &str) -> PromptId {
    let steer = live
        .client
        .admit_prompt(
            live.session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: text.to_owned(),
                    skill_invocations: Vec::new(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("steer the running Turn");
    assert_eq!(steer.status, PromptStatus::Pending);
    live.wait_for("the steer Prompt reaches the running loop", |snapshot| {
        snapshot
            .prompts
            .iter()
            .any(|prompt| prompt.id == steer.id && prompt.status == PromptStatus::Delivered)
    })
    .await;
    steer.id
}

/// The user messages the CLI was handed on stdin, in the order it read them.
fn user_messages(claude: &ScriptedClaude) -> Vec<Value> {
    claude
        .requests()
        .into_iter()
        .filter(|request| request.get("type").and_then(Value::as_str) == Some("user"))
        .collect()
}

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

    let prompts = user_messages(&claude);
    assert_eq!(
        prompts
            .iter()
            .filter_map(|request| request.pointer("/message/content/0/text")?.as_str())
            .collect::<Vec<_>>(),
        ["Say hello", "Answer in French instead"],
        "the steer reaches the CLI as another user message on the running loop's stdin"
    );
    let uuids = prompts
        .iter()
        .filter_map(|request| request.get("uuid")?.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        uuids.len(),
        2,
        "each message goes with the uuid the CLI reports its lifecycle under: {prompts:?}"
    );
    assert_ne!(uuids[0], uuids[1]);

    live.shutdown().await;
}

#[tokio::test]
async fn a_steer_during_a_tool_call_is_folded_into_the_loop_and_settles_on_its_one_result() {
    let claude = ScriptedClaude::new(&format!(
        "{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(FOLDED_CONVERSATION)
    ));
    let mut live = LiveTurn::start(
        ClaudeRuntime::new(claude.executable()),
        "claude-folded-steer",
        "Run sleep 20, then say DONE",
    )
    .await;
    live.wait_for("the loop is waiting on its Bash call", |snapshot| {
        snapshot.activities.iter().any(|activity| {
            matches!(
                activity,
                Activity::Command {
                    status: ActivityStatus::Active,
                    ..
                }
            )
        })
    })
    .await;

    steer(&mut live, "Also say the word MARK").await;
    let steered = live
        .client
        .read_session(live.session_id)
        .await
        .expect("read the steered Session");
    assert_eq!(steered.turns.len(), 1);
    assert_eq!(steered.turns[0].status, TurnStatus::Active);

    claude.release();
    let settled = settled_session(&live.client, live.session_id, 0).await;

    assert_eq!(
        settled.turns[0].status,
        TurnStatus::Completed,
        "the one result the loop answered the Prompt and the steer with Settles the Turn"
    );
    assert_eq!(settled.turns.len(), 1, "no second Turn ever began");
    assert_eq!(
        agent_messages(&settled)
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        ["DONE MARK"]
    );
    assert!(
        settled
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::User)
            .all(|message| message.turn_id == live.turn_id),
        "the steer's Message belongs to the Turn it steered"
    );
    assert!(
        settled.activities.iter().all(|activity| !matches!(
            activity,
            Activity::Command {
                status: ActivityStatus::Active,
                ..
            }
        )),
        "the Bash call settled with its tool result"
    );
    assert_eq!(
        settled.turns[0].usage,
        Some(Usage {
            fresh_input_tokens: Some(30),
            output_tokens: Some(9),
            ..Usage::default()
        }),
        "the one result carries the whole Turn's Usage"
    );

    live.shutdown().await;
}

#[tokio::test]
async fn a_cli_reporting_no_lifecycle_settles_a_turn_whose_steer_it_folded_on_the_one_result() {
    let claude = ScriptedClaude::new(&format!(
        "{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(&without_lifecycle(FOLDED_CONVERSATION))
    ));
    let mut live = LiveTurn::start(
        ClaudeRuntime::new(claude.executable()),
        "claude-folded-steer-unreported",
        "Run sleep 20, then say DONE",
    )
    .await;
    live.wait_for("the loop is waiting on its Bash call", |snapshot| {
        !snapshot.activities.is_empty()
    })
    .await;
    steer(&mut live, "Also say the word MARK").await;

    claude.release();
    let settled = settled_session(&live.client, live.session_id, 0).await;

    assert_eq!(
        settled.turns[0].status,
        TurnStatus::Completed,
        "a result with nothing said of the steer Settles the Turn rather than wait on another"
    );
    assert_eq!(settled.turns.len(), 1);
    assert_eq!(agent_messages(&settled)[0].content, "DONE MARK");

    live.shutdown().await;
}

#[tokio::test]
async fn a_cli_reporting_no_lifecycle_answers_a_steer_it_did_not_fold_in_a_continuation() {
    let claude = ScriptedClaude::new(&format!(
        "{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(&without_lifecycle(STEERED_CONVERSATION))
    ));
    let mut live = LiveTurn::start(
        ClaudeRuntime::new(claude.executable()),
        "claude-separate-steer-unreported",
        "Say hello",
    )
    .await;
    live.wait_for("the answer starts streaming", |snapshot| {
        !agent_messages(snapshot).is_empty()
    })
    .await;
    steer(&mut live, "Answer in French instead").await;

    claude.release();
    let answered = settled_session(&live.client, live.session_id, 1).await;

    assert_eq!(
        answered.turns[0].status,
        TurnStatus::Completed,
        "the steered Turn Settles on the result that ended its loop"
    );
    assert_eq!(answered.turns.len(), 2);
    assert_eq!(
        answered.turns[1].prompt_id, None,
        "the loop the steer began is a native Continuation"
    );
    assert_eq!(answered.turns[1].status, TurnStatus::Completed);
    let answers = agent_messages(&answered);
    assert_eq!(
        answers
            .iter()
            .map(|message| (message.content.as_str(), message.turn_id))
            .collect::<Vec<_>>(),
        [
            ("Hello", answered.turns[0].id),
            ("Bonjour", answered.turns[1].id)
        ],
        "nothing the second loop wrote is dropped"
    );

    live.shutdown().await;
}
