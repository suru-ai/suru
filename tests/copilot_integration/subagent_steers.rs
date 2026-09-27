//! Steering a working Copilot Subagent with a Delegation (ADR 0032): a message Copilot delivers
//! into a Subagent's running stretch (`user.message` with `delivery: "steering"`) stands in the
//! Subagent's current Turn where it arrived, naming the Agent that sent it, and the `write_agent`
//! call that sent it adds nothing to the sender's Transcript.
//!
//! The replays are live Copilot CLI 1.0.87 captures, sanitized and committed under `fixtures/`
//! (docs/validation/0397-copilot-subagent-steer.md). In them the CLI never steers: it queues every
//! `write_agent` to a working Subagent until its stretch ends and delivers it only after the
//! stretch's `subagent.completed`, as a run of its own. So the replays pin what a send changes —
//! nothing, in either Transcript, until a queued delivery is read as a resume (ADR 0033, #404) — while the
//! steer itself is scripted in the shape the SDK's schema documents for it.

use crate::support::{conversation_fixture, opened_session, settled_session};
use suru::protocol::{
    Activity, ActivityStatus, Delegator, MessageRole, SessionId, SessionSnapshot, TranscriptItem,
    TurnStatus,
};

/// The main agent spawns a background agent to run three slow shell commands one at a time, then
/// `write_agent`s it a message while its first command runs. The CLI answers the send as
/// delivered, but queues it: the agent finishes its stretch without it, and only after its
/// `subagent.completed` does the message reach its loop, `delivery: "queued"`, as a new run.
const PARENT_SEND_QUEUED: &str = include_str!("fixtures/steer-parent-queued.jsonl");

/// The main agent spawns the same worker, B, and then a second agent, A, which finds B with
/// `list_agents` and `write_agent`s it while B's first command runs. A settles at once; B is
/// queued the message exactly as it was from the main agent, `source` naming A.
const SIBLING_SEND_QUEUED: &str = include_str!("fixtures/steer-sibling-queued.jsonl");

/// The text the captures send.
const STEER: &str = "STEER-MARKER: also say the word PINEAPPLE in your final report";

/// A timeline replaying `capture` — one timeline entry per line — as the scripted CLI's output for
/// the Prompt, keeping only the lines `keep` accepts.
///
/// The captures ran under approve-all, so each `permission.requested` was answered the moment it
/// was asked. Replayed with no one to answer it, it opens an Approval whose settle races the
/// timeline, which is nothing to do with a steer, so the permission entries are left out.
fn replay(capture: &str, keep: impl Fn(&str) -> bool) -> String {
    capture
        .lines()
        .filter(|line| {
            !line.trim().is_empty() && !line.contains(r#""type":"permission."#) && keep(line)
        })
        .map(|line| format!("      raw_event '{}'\n", line.replace('\'', r"'\''")))
        .collect()
}

/// The same capture with every `write_agent` taken out: each execution's start, output, and
/// completion, whichever conversation ran it.
fn without_write_agents(capture: &str) -> String {
    let sends = capture
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|event| {
            event["type"] == "tool.execution_start" && event["data"]["toolName"] == "write_agent"
        })
        .filter_map(|event| event["data"]["toolCallId"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    assert!(!sends.is_empty(), "the capture sends something to take out");
    replay(capture, |line| {
        !sends
            .iter()
            .any(|id| line.contains(&format!(r#""toolCallId":"{id}""#)))
    })
}

/// `snapshot`'s Transcript, in order, as lines naming each Turn by its place and each item by
/// what a reader meets: a Message by who it is from and what it says, an Activity by its kind and
/// what it ran or was asked. `parent` is the Session a Delegation from the parent names.
fn outline(snapshot: &SessionSnapshot, parent: Option<SessionId>) -> Vec<String> {
    let turn = |turn_id| {
        snapshot
            .turns
            .iter()
            .position(|turn| turn.id == turn_id)
            .expect("every item stands in one of the Session's Turns")
    };
    snapshot
        .transcript
        .iter()
        .map(|item| match item {
            TranscriptItem::Message { message_id } => {
                let message = snapshot
                    .messages
                    .iter()
                    .find(|message| message.id == *message_id)
                    .expect("the Transcript names a Message the Session holds");
                let from = match &message.role {
                    MessageRole::Delegation(Delegator { session_id, name }) => {
                        if Some(*session_id) == parent {
                            "delegation from the parent".to_owned()
                        } else {
                            format!("delegation from {}", name.as_deref().unwrap_or("?"))
                        }
                    }
                    role => format!("{role:?}"),
                };
                format!(
                    "turn {}: {from}: {}",
                    turn(message.turn_id),
                    message.content
                )
            }
            TranscriptItem::Activity { activity_id } => {
                let activity = snapshot
                    .activities
                    .iter()
                    .find(|activity| activity.id() == *activity_id)
                    .expect("the Transcript names an Activity the Session holds");
                let what = match activity {
                    Activity::Command { command, .. } => format!("command {command}"),
                    Activity::Subagent {
                        name,
                        description,
                        status,
                        ..
                    } => format!("subagent {name} ({description}) {status:?}"),
                    Activity::Reasoning { .. } => "reasoning".to_owned(),
                    other => format!("{other:?}"),
                };
                format!("turn {}: {what}", turn(activity.turn_id()))
            }
        })
        .collect()
}

/// The child Sessions `parent`'s Subagent rows lead into, by the name each row gives.
fn children(parent: &SessionSnapshot) -> Vec<(String, SessionId)> {
    parent
        .activities
        .iter()
        .filter_map(|activity| match activity {
            Activity::Subagent {
                name, session_id, ..
            } => Some((name.clone(), *session_id)),
            _ => None,
        })
        .collect()
}

/// Whether `snapshot` holds a Delegation of the text `text` anywhere — the send itself, rather
/// than a spawn prompt that quotes it.
fn holds_delegation(snapshot: &SessionSnapshot, text: &str) -> bool {
    snapshot
        .messages
        .iter()
        .any(|message| message.role.delegator().is_some() && message.content.starts_with(text))
}

#[tokio::test]
async fn a_write_agent_the_cli_queues_leaves_both_transcripts_as_they_were_without_it() {
    let copilot = conversation_fixture(&replay(PARENT_SEND_QUEUED, |_| true));
    let opened = opened_session(&copilot, "copilot-steer-parent-queued", "Steer the sleeper").await;
    let client = &opened.client;
    let parent = settled_session(client, opened.session_id, 0).await;
    let [(_, child_id)] = children(&parent)[..] else {
        panic!("the spawn stands as one row, got {:?}", parent.activities);
    };
    let child = settled_session(client, child_id, 0).await;

    let unsent = conversation_fixture(&without_write_agents(PARENT_SEND_QUEUED));
    let unsent_opened =
        opened_session(&unsent, "copilot-steer-parent-unsent", "Steer the sleeper").await;
    let unsent_parent = settled_session(&unsent_opened.client, unsent_opened.session_id, 0).await;
    let [(_, unsent_child_id)] = children(&unsent_parent)[..] else {
        panic!(
            "the spawn stands as one row, got {:?}",
            unsent_parent.activities
        );
    };
    let unsent_child = settled_session(&unsent_opened.client, unsent_child_id, 0).await;

    assert_eq!(
        outline(&parent, None),
        outline(&unsent_parent, None),
        "the write_agent call adds no row to the main agent's Transcript"
    );
    assert!(
        outline(&parent, None)
            .iter()
            .all(|line| !line.contains("write_agent")),
        "{:?}",
        outline(&parent, None)
    );
    assert_eq!(
        outline(&child, Some(opened.session_id)),
        outline(&unsent_child, Some(unsent_opened.session_id)),
        "a queued delivery steers nothing: the stretch that was working never received it"
    );
    assert_eq!(child.turns[0].status, TurnStatus::Completed);
    assert!(
        !holds_delegation(&child, STEER),
        "the queued message stands nowhere in the stretch it never reached: {:?}",
        outline(&child, Some(opened.session_id))
    );
    let Some(Activity::Subagent { status, .. }) = parent
        .activities
        .iter()
        .find(|activity| matches!(activity, Activity::Subagent { .. }))
    else {
        unreachable!()
    };
    assert_eq!(*status, ActivityStatus::Completed);

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

#[tokio::test]
async fn a_siblings_write_agent_the_cli_queues_adds_nothing_to_the_senders_transcript() {
    let copilot = conversation_fixture(&replay(SIBLING_SEND_QUEUED, |_| true));
    let opened = opened_session(
        &copilot,
        "copilot-steer-sibling-queued",
        "Steer the sleeper",
    )
    .await;
    let client = &opened.client;
    let parent = settled_session(client, opened.session_id, 0).await;

    let unsent = conversation_fixture(&without_write_agents(SIBLING_SEND_QUEUED));
    let unsent_opened =
        opened_session(&unsent, "copilot-steer-sibling-unsent", "Steer the sleeper").await;
    let unsent_parent = settled_session(&unsent_opened.client, unsent_opened.session_id, 0).await;

    assert_eq!(outline(&parent, None), outline(&unsent_parent, None));
    let sent = children(&parent);
    let unsent_children = children(&unsent_parent);
    assert_eq!(
        sent.iter().map(|(name, _)| name).collect::<Vec<_>>(),
        ["agent-B", "agent-A"],
        "both spawns stand as rows"
    );
    for ((name, child_id), (_, unsent_child_id)) in sent.iter().zip(&unsent_children) {
        let child = settled_session(client, *child_id, 0).await;
        let unsent_child = settled_session(&unsent_opened.client, *unsent_child_id, 0).await;
        assert_eq!(
            outline(&child, Some(opened.session_id)),
            outline(&unsent_child, Some(unsent_opened.session_id)),
            "{name}'s Transcript is as it was without the send: the sender's shows no \
             write_agent row, and the receiver's working stretch never received it"
        );
        assert!(!holds_delegation(&child, STEER), "{name}");
    }

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
