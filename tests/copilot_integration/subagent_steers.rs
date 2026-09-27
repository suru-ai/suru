//! Steering a working Copilot Subagent with a Delegation (ADR 0032): a message Copilot delivers
//! into a Subagent's running stretch (`user.message` with `delivery: "steering"`) stands in the
//! Subagent's current Turn where it arrived, naming the Agent that sent it, and the `write_agent`
//! call that sent it adds nothing to the sender's Transcript.
//!
//! No live CLI has been seen to steer: Copilot 1.0.87 queues every `write_agent` to a working
//! Subagent until its stretch ends and delivers it only after the stretch's `subagent.completed`,
//! as a run of its own (docs/validation/0397-copilot-subagent-steer.md), which is a resume
//! (`subagent_resumes`). So the steer here is scripted in the shape the SDK's schema documents
//! for it.

use crate::support::{
    conversation_fixture, opened_session, settled_session, subagent_children as children,
    transcript_outline as outline,
};
use suru::protocol::TurnStatus;

/// The shape a steer takes on Copilot's timeline, as the SDK's schema documents it — no live CLI
/// has been seen to emit one. The main agent spawns a worker and a helper, sends the worker a
/// message through `write_agent` while its first command runs, and the helper sends it another
/// from its own conversation. The worker's loop takes each in at its next round, reporting a
/// `user.message` with `delivery: "steering"` whose `source` names the sender: the main agent by
/// its Session's `agent-<id>`, the helper by its own instance.
const STEERED_TURN: &str = r#"      event e1 tool.execution_start '{"toolCallId":"t-worker","toolName":"task","arguments":{"description":"Run three sleeps","prompt":"Run three sleeps, one at a time."}}'
      agent_event e2 worker subagent.started '{"toolCallId":"t-worker","agentName":"task","agentDisplayName":"worker","agentDescription":"Run three sleeps"}'
      event e3 tool.execution_complete '{"toolCallId":"t-worker","success":true,"result":{"content":"Agent started in background with agent_id: worker."}}'
      agent_event e4 worker user.message '{"content":"Run three sleeps, one at a time.","delivery":"idle","source":"agent-main","turnId":"0"}'
      agent_event e5 worker tool.execution_start '{"toolCallId":"t-one","toolName":"bash","arguments":{"command":"sleep 15 && echo one"}}'
      event e6 tool.execution_start '{"toolCallId":"t-send","toolName":"write_agent","arguments":{"agent_id":"worker","message":"Also say PINEAPPLE."}}'
      event e7 tool.execution_complete '{"toolCallId":"t-send","success":true,"result":{"content":"Message delivered to agent worker."}}'
      event e8 tool.execution_start '{"toolCallId":"t-helper","toolName":"task","arguments":{"description":"Nudge the worker","prompt":"Tell the worker to say MANGO."}}'
      agent_event e9 helper subagent.started '{"toolCallId":"t-helper","agentName":"task","agentDisplayName":"helper","agentDescription":"Nudge the worker"}'
      event e10 tool.execution_complete '{"toolCallId":"t-helper","success":true,"result":{"content":"Agent started in background with agent_id: helper."}}'
      agent_event e11 worker tool.execution_complete '{"toolCallId":"t-one","success":true,"result":{"content":"one\n"}}'
      agent_event e12 worker user.message '{"content":"Also say PINEAPPLE.","delivery":"steering","source":"agent-main","turnId":"1"}'
      agent_event e13 helper tool.execution_start '{"toolCallId":"t-nudge","toolName":"write_agent","arguments":{"agent_id":"worker","message":"Also say MANGO."}}'
      agent_event e14 helper tool.execution_complete '{"toolCallId":"t-nudge","success":true,"result":{"content":"Message delivered to agent worker."}}'
      agent_event e15 helper assistant.message '{"messageId":"h1","content":"Done."}'
      agent_event e16 helper subagent.completed '{"toolCallId":"t-helper","agentName":"task","agentDisplayName":"helper"}'
      agent_event e17 worker tool.execution_start '{"toolCallId":"t-two","toolName":"bash","arguments":{"command":"sleep 15 && echo two"}}'
      agent_event e18 worker tool.execution_complete '{"toolCallId":"t-two","success":true,"result":{"content":"two\n"}}'
      agent_event e19 worker user.message '{"content":"Also say MANGO.","delivery":"steering","source":"agent-helper","turnId":"2"}'
      agent_event e20 worker assistant.message '{"messageId":"w1","content":"one, two, PINEAPPLE, MANGO."}'
      agent_event e21 worker subagent.completed '{"toolCallId":"t-worker","agentName":"task","agentDisplayName":"worker"}'
      event e22 assistant.message '{"messageId":"m1","content":"The worker said PINEAPPLE and MANGO."}'
      event e23 session.idle '{}'
"#;

#[tokio::test]
async fn a_steer_stands_in_the_working_turn_naming_its_sender_and_leaves_the_senders_as_they_were()
{
    let copilot = conversation_fixture(STEERED_TURN);
    let opened = opened_session(&copilot, "copilot-steer-scripted", "Steer the worker").await;
    let client = &opened.client;
    let parent = settled_session(client, opened.session_id, 0).await;
    let [(_, worker), (_, helper)] = children(&parent)[..] else {
        panic!("both spawns stand as rows, got {:?}", parent.activities);
    };
    let worker = settled_session(client, worker, 0).await;

    assert_eq!(
        worker.turns.len(),
        1,
        "a steer begins no Turn: {:?}",
        worker.turns
    );
    assert_eq!(worker.turns[0].status, TurnStatus::Completed);
    assert_eq!(
        outline(&worker, Some(opened.session_id)),
        [
            "turn 0: delegation from the parent: Run three sleeps, one at a time.",
            "turn 0: command sleep 15 && echo one",
            "turn 0: delegation from the parent: Also say PINEAPPLE.",
            "turn 0: command sleep 15 && echo two",
            "turn 0: delegation from helper: Also say MANGO.",
            "turn 0: Agent: one, two, PINEAPPLE, MANGO.",
        ],
        "each steer stands where the worker received it, naming the Agent that sent it, and the \
         spawn's own delivery is not repeated"
    );

    let unsent_timeline = STEERED_TURN
        .lines()
        .filter(|line| {
            !["t-send", "t-nudge", r#""delivery":"steering""#]
                .iter()
                .any(|marker| line.contains(marker))
        })
        .map(|line| format!("{line}\n"))
        .collect::<String>();
    let unsent = conversation_fixture(&unsent_timeline);
    let unsent_opened = opened_session(&unsent, "copilot-steer-unsent", "Steer the worker").await;
    let unsent_parent = settled_session(&unsent_opened.client, unsent_opened.session_id, 0).await;
    assert_eq!(
        outline(&parent, None),
        outline(&unsent_parent, None),
        "a steer adds nothing to the main agent's Transcript: no row, no description, no tool row"
    );
    let [_, (_, unsent_helper)] = children(&unsent_parent)[..] else {
        panic!(
            "both spawns stand as rows, got {:?}",
            unsent_parent.activities
        );
    };
    assert_eq!(
        outline(
            &settled_session(client, helper, 0).await,
            Some(opened.session_id)
        ),
        outline(
            &settled_session(&unsent_opened.client, unsent_helper, 0).await,
            Some(unsent_opened.session_id)
        ),
        "nor to the sibling's that sent it"
    );

    unsent_opened
        .server
        .shutdown()
        .await
        .expect("shut the unsent server down");
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}
