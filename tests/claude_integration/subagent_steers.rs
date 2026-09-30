//! Steering a working Claude Subagent with a Delegation (ADR 0032): a SendMessage the CLI queues
//! for an agent still working stands in the Subagent's current Turn where the agent read it — at
//! its next tool round — and adds nothing to the delegating Transcript, while one the agent
//! finished too soon to read is the Delegation that opens the resume the CLI restarts it with.
//!
//! The replays are live 2.1.280 captures, sanitized and committed under `fixtures/`: the loop's
//! own conversation arrives there as full snapshots rather than chunks, as it does from a CLI not
//! asked for partial messages.

use crate::support::{
    CLAUDE_MODELS, ScriptedClaude, conversation_fixture, discovery_arms, opened_session,
    session_where, settled_session, user_turn_arm,
};
use suru::protocol::{
    Activity, ActivityStatus, Delegator, MessageRole, SessionId, SessionSnapshot, TranscriptItem,
    TurnStatus,
};

/// The mid-run steer: the loop spawns a background agent to run three slow Bash commands, tries a
/// SendMessage to a name no agent answers to — refused — then sends the agent a steer while its
/// first command runs. The CLI queues it, and the agent reads it once that command's result goes
/// back to it, acting on it in its final report.
const STEER_MID_RUN: &str = include_str!("fixtures/steer-mid-run.jsonl");

/// The late steer: the loop spawns a background agent that writes one long reply and uses no
/// tools, and steers it while it writes. The agent finishes without a tool round, so without
/// reading the steer, and the CLI restarts the same task — naming the original Agent call — with
/// the steer as its prompt, while the loop's own Turn has long since ended.
const STEER_LATE_RESTART: &str = include_str!("fixtures/steer-late-restart.jsonl");

/// The text the captures steer with.
const STEER: &str = "STEER-MARKER: also say the word PINEAPPLE in your final report";

/// A timeline replaying `capture` — one wire message per line — as the scripted CLI's output for
/// the Prompt, keeping only the lines `keep` accepts.
fn replay(capture: &str, keep: impl Fn(&str) -> bool) -> String {
    capture
        .lines()
        .filter(|line| !line.trim().is_empty() && keep(line))
        .map(|line| format!("      emit '{}'\n", line.replace('\'', r"'\''")))
        .collect()
}

/// The same capture with every SendMessage taken out: each SendMessage tool use, and the tool
/// result the CLI answered it with.
fn without_send_messages(capture: &str) -> String {
    let send_messages = capture
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .flat_map(|message| {
            message
                .pointer("/message/content")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .filter(|block| block["type"] == "tool_use" && block["name"] == "SendMessage")
        .filter_map(|block| block["id"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    assert!(
        !send_messages.is_empty(),
        "the capture sends something to take out"
    );
    replay(capture, |line| {
        !send_messages
            .iter()
            .any(|id| line.contains(&format!(r#""{id}""#)))
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
                    other => format!("{other:?}"),
                };
                format!("turn {}: {what}", turn(activity.turn_id()))
            }
        })
        .collect()
}

/// The Subagent rows in `snapshot`, in Transcript order.
fn subagent_rows(snapshot: &SessionSnapshot) -> Vec<&Activity> {
    snapshot
        .activities
        .iter()
        .filter(|activity| matches!(activity, Activity::Subagent { .. }))
        .collect()
}

/// The child Session the one Subagent `parent` holds rows for leads into.
fn child_of(parent: &SessionSnapshot) -> SessionId {
    let mut children = subagent_rows(parent)
        .into_iter()
        .map(|row| match row {
            Activity::Subagent { session_id, .. } => *session_id,
            _ => unreachable!(),
        })
        .collect::<Vec<_>>();
    children.dedup();
    let [child] = children[..] else {
        panic!(
            "every row leads into one Subagent's Session, got {:?}",
            parent.activities
        );
    };
    child
}

#[tokio::test]
async fn a_steer_the_subagent_reads_mid_run_stands_where_it_read_it_and_leaves_the_parent_as_it_was()
 {
    let claude = conversation_fixture(&replay(STEER_MID_RUN, |_| true));
    let opened = opened_session(&claude, "claude-steer-mid-run", "Steer the sleeper").await;
    let client = &opened.client;
    let parent = settled_session(client, opened.session_id, 1).await;
    let child_id = child_of(&parent);
    let child = settled_session(client, child_id, 0).await;

    assert_eq!(
        child.turns.len(),
        1,
        "a steer begins no Turn: {:?}",
        child.turns
    );
    assert_eq!(child.turns[0].status, TurnStatus::Completed);
    assert_eq!(
        outline(&child, Some(opened.session_id)),
        [
            "turn 0: delegation from the parent: Run these three Bash commands one at a time, \
             each as its own separate Bash tool call, waiting for each to finish: `sleep 12 && \
             echo one`, then `sleep 12 && echo two`, then `sleep 12 && echo three`. Then reply \
             with a one-line final report. If you receive any additional instructions from the \
             coordinator while working, follow them in your final report."
                .to_owned(),
            "turn 0: Agent: I'll run these three Bash commands sequentially, waiting for each to \
             complete before starting the next one."
                .to_owned(),
            "turn 0: command sleep 12 && echo one".to_owned(),
            format!("turn 0: delegation from the parent: {STEER}"),
            "turn 0: command sleep 12 && echo two".to_owned(),
            "turn 0: command sleep 12 && echo three".to_owned(),
            "turn 0: Agent: All three commands completed successfully in sequence: one, two, \
             three — each waited 12 seconds as expected, PINEAPPLE."
                .to_owned(),
        ],
        "the steer stands after the command the agent was running when it was queued, where the \
         agent read it at its next tool round, and the refused SendMessage stands nowhere"
    );

    let unsteered = conversation_fixture(&without_send_messages(STEER_MID_RUN));
    let unsteered_opened = opened_session(
        &unsteered,
        "claude-steer-mid-run-unsent",
        "Steer the sleeper",
    )
    .await;
    let unsteered_parent =
        settled_session(&unsteered_opened.client, unsteered_opened.session_id, 1).await;
    assert_eq!(
        outline(&parent, None),
        outline(&unsteered_parent, None),
        "a steer changes nothing in the parent's Transcript: no row, no description, no tool row"
    );
    assert_eq!(
        parent
            .turns
            .iter()
            .map(|turn| (turn.prompt_id.is_some(), turn.status))
            .collect::<Vec<_>>(),
        unsteered_parent
            .turns
            .iter()
            .map(|turn| (turn.prompt_id.is_some(), turn.status))
            .collect::<Vec<_>>(),
    );

    unsteered_opened
        .server
        .shutdown()
        .await
        .expect("shut the unsteered server down");
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn a_steer_the_subagent_finished_too_soon_to_read_opens_the_resume_the_cli_restarts_it_with()
{
    let claude = conversation_fixture(&replay(STEER_LATE_RESTART, |_| true));
    let opened = opened_session(&claude, "claude-steer-late-restart", "Steer the essayist").await;
    let client = &opened.client;
    let parent = settled_session(client, opened.session_id, 2).await;

    let [
        Activity::Subagent {
            turn_id: spawn_turn,
            status: spawn_status,
            description: spawn_description,
            session_id: spawned,
            ..
        },
        Activity::Subagent {
            turn_id: resume_turn,
            status: resume_status,
            name: resume_name,
            description: resume_description,
            session_id: resumed,
            ..
        },
    ] = subagent_rows(&parent)[..]
    else {
        panic!(
            "the spawn and the restart each stand as a row, got {:?}",
            parent.activities
        );
    };
    assert_eq!(*spawn_turn, parent.turns[0].id);
    assert_eq!(*spawn_status, ActivityStatus::Completed);
    assert_eq!(spawn_description, "essay writer");
    assert_eq!(
        *resume_turn, parent.turns[1].id,
        "the restart arrives once the Turn that sent the steer has settled, so its row stands in \
         the Continuation it begins"
    );
    assert!(
        parent.turns[1].prompt_id.is_none(),
        "that Turn is a Continuation: {:?}",
        parent.turns[1]
    );
    assert_eq!(*resume_status, ActivityStatus::Completed);
    assert_eq!(resume_name, "general-purpose");
    assert_eq!(
        resume_description, "steer pineapple",
        "the restart's row reads the SendMessage's summary, not the spawn's description"
    );
    assert_eq!(
        resumed, spawned,
        "both rows lead into the Subagent's one Session"
    );

    let child = settled_session(client, *spawned, 1).await;
    let [first, second] = child.turns.as_slice() else {
        panic!(
            "the restart begins a second Turn in the Subagent's Session, got {:?}",
            child.turns
        );
    };
    assert_eq!(first.status, TurnStatus::Completed);
    assert_eq!(second.status, TurnStatus::Completed);
    let outline = outline(&child, Some(opened.session_id));
    let [spawn, first_reply, steer, second_reply] = &outline[..] else {
        panic!("each Turn holds its Delegation and the reply to it, got {outline:?}");
    };
    assert!(
        spawn.starts_with(
            "turn 0: delegation from the parent: Without using any tools at all, write a detailed \
             2000-word essay"
        ),
        "{spawn}"
    );
    assert!(
        first_reply.starts_with("turn 0: Agent: I appreciate the request"),
        "{first_reply}"
    );
    assert_eq!(
        *steer,
        format!("turn 1: delegation from the parent: {STEER}"),
        "the steer the agent never read opens the restarted Turn as its Delegation"
    );
    assert!(
        second_reply.starts_with("turn 1: Agent: I acknowledge the coordinator's instruction"),
        "{second_reply}"
    );

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

/// A sibling steering a working Subagent: the loop spawns a writer and a reviewer, the writer
/// starts a command, and the reviewer — whose conversation arrives as snapshots — sends the writer
/// a steer the CLI queues. The writer reads it once its command's result goes back to it.
const SIBLING_STEERS_SUBAGENT: &str = r#"      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"agent_writer","name":"Agent","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"writer-task","tool_use_id":"agent_writer","description":"Draft the notes","prompt":"Draft the release notes.","task_type":"local_agent","subagent_type":"Writer","session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"agent_reviewer","name":"Agent","input":{}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":1},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"reviewer-task","tool_use_id":"agent_reviewer","description":"Review the notes","prompt":"Review the notes as they are drafted.","task_type":"local_agent","subagent_type":"Reviewer","session_id":"prov-session"}'
      emit '{"type":"assistant","message":{"id":"msg_w1","role":"assistant","content":[{"type":"tool_use","id":"toolu_draft","name":"Bash","input":{"command":"cat NOTES.md"}}]},"parent_tool_use_id":"agent_writer","session_id":"prov-session"}'
      emit '{"type":"assistant","message":{"id":"msg_r1","role":"assistant","content":[{"type":"tool_use","id":"send_review","name":"SendMessage","input":{"to":"writer-task","message":"Tighten the second paragraph.","summary":"Tighten the notes"}}]},"parent_tool_use_id":"agent_reviewer","session_id":"prov-session"}'
      emit '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"send_review","content":[{"type":"text","text":"{\"success\":true,\"message\":\"Message queued for delivery to writer-task at its next tool round.\",\"pin\":{\"id\":\"writer-task\",\"name\":\"writer-task\",\"ref\":\"f9fef7\"}}"}]}]},"parent_tool_use_id":"agent_reviewer","session_id":"prov-session"}'
      emit '{"type":"assistant","message":{"id":"msg_r2","role":"assistant","content":[{"type":"text","text":"Sent."}]},"parent_tool_use_id":"agent_reviewer","session_id":"prov-session"}'
      emit '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_draft","content":"Notes\n","is_error":false}]},"parent_tool_use_id":"agent_writer","session_id":"prov-session"}'
      emit '{"type":"assistant","message":{"id":"msg_w2","role":"assistant","content":[{"type":"text","text":"Tightened."}]},"parent_tool_use_id":"agent_writer","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_notification","task_id":"reviewer-task","status":"completed","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_notification","task_id":"writer-task","status":"completed","session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":900,"num_turns":1,"result":"Reviewed.","session_id":"prov-session"}'
"#;

#[tokio::test]
async fn a_steer_sent_by_a_sibling_is_attributed_to_the_sibling_and_adds_no_row() {
    let claude = conversation_fixture(SIBLING_STEERS_SUBAGENT);
    let opened = opened_session(&claude, "claude-sibling-steers", "Write then review").await;
    let session_id = opened.session_id;
    let client = &opened.client;
    let parent = settled_session(client, session_id, 0).await;

    let [
        Activity::Subagent {
            session_id: writer, ..
        },
        Activity::Subagent {
            session_id: reviewer,
            ..
        },
    ] = subagent_rows(&parent)[..]
    else {
        panic!(
            "the parent holds only the two spawns, got {:?}",
            parent.activities
        );
    };
    let reviewer_session = settled_session(client, *reviewer, 0).await;
    assert_eq!(
        outline(&reviewer_session, Some(session_id)),
        [
            "turn 0: delegation from the parent: Review the notes as they are drafted.",
            "turn 0: Agent: Sent.",
        ],
        "the sibling's steer adds no row to the sibling's own Transcript"
    );

    let writer_session = settled_session(client, *writer, 0).await;
    assert_eq!(
        outline(&writer_session, Some(session_id)),
        [
            "turn 0: delegation from the parent: Draft the release notes.",
            "turn 0: command cat NOTES.md",
            "turn 0: delegation from Reviewer: Tighten the second paragraph.",
            "turn 0: Agent: Tightened.",
        ]
    );
    let steer = writer_session
        .messages
        .iter()
        .find(|message| message.content == "Tighten the second paragraph.")
        .expect("the steer stands in the writer's Transcript");
    assert_eq!(
        steer.role,
        MessageRole::Delegation(Delegator {
            session_id: *reviewer,
            name: Some("Reviewer".to_owned()),
        }),
        "the steer names the sibling that sent it"
    );

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

/// A steer queued for a background agent that is stopped before its command's result goes back to
/// it: the loop spawns the agent, the agent starts a command, the loop steers it, and the Turn
/// ends with the agent still working.
const STEER_PENDING_WHEN_STOPPED: &str = r#"      emit '{"type":"assistant","message":{"id":"msg_1","role":"assistant","content":[{"type":"tool_use","id":"agent_1","name":"Agent","input":{"description":"Run the sleeps","prompt":"Sleep three times.","run_in_background":true}}]},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"agent-task","tool_use_id":"agent_1","description":"Run the sleeps","task_type":"local_agent","subagent_type":"general-purpose","session_id":"prov-session"}'
      emit '{"type":"assistant","message":{"id":"msg_c1","role":"assistant","content":[{"type":"tool_use","id":"toolu_sleep","name":"Bash","input":{"command":"sleep 12"}}]},"parent_tool_use_id":"agent_1","session_id":"prov-session"}'
      emit '{"type":"assistant","message":{"id":"msg_2","role":"assistant","content":[{"type":"tool_use","id":"send_1","name":"SendMessage","input":{"to":"agent-task","message":"Never read.","summary":"steer"}}]},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"send_1","content":[{"type":"text","text":"{\"success\":true,\"message\":\"Message queued for delivery to agent-task at its next tool round.\",\"pin\":{\"id\":\"agent-task\",\"name\":\"agent-task\",\"ref\":\"5283b6\"}}"}]}]},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":300,"num_turns":1,"result":"Steered.","session_id":"prov-session"}'
"#;

/// A `stop_task` arm acknowledging the stop, then playing what the stopped agent's conversation
/// still trails: its command's result going back to it, a message after it, and the CLI's own
/// notification that the task stopped.
fn stop_task_trailing_arm() -> String {
    r#"    *'"subtype":"stop_task"'*)
      printf '%s\n' '{"type":"control_response","response":{"subtype":"success","request_id":"'"$request_id"'","response":{}}}'
      emit '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_sleep","content":"The user doesn\u0027t want to proceed with this tool use. The tool use was rejected (eg. if it was a file edit, the new_string was NOT written to the file). STOP what you are doing and wait for the user to tell you how to proceed.","is_error":true}]},"parent_tool_use_id":"agent_1","session_id":"prov-session"}'
      emit '{"type":"assistant","message":{"id":"msg_c2","role":"assistant","content":[{"type":"text","text":"Stopping."}]},"parent_tool_use_id":"agent_1","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_notification","task_id":"agent-task","tool_use_id":"agent_1","status":"stopped","session_id":"prov-session"}'
      ;;
"#
    .to_owned()
}

#[tokio::test]
async fn a_steer_pending_when_its_subagent_is_stopped_never_appears() {
    let claude = ScriptedClaude::new(&format!(
        "{}{}{}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(STEER_PENDING_WHEN_STOPPED),
        stop_task_trailing_arm(),
    ));
    let opened = opened_session(&claude, "claude-steer-stopped", "Steer then stop").await;
    let session_id = opened.session_id;
    let client = &opened.client;
    let mut feed = client
        .subscribe_session(session_id)
        .await
        .expect("subscribe to Session SSE");
    let steered = session_where(
        client,
        &mut feed,
        session_id,
        "the Turn that steered the agent settles with the agent still working",
        |snapshot| {
            snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.status != TurnStatus::Active)
                && matches!(
                    subagent_rows(snapshot)[..],
                    [Activity::Subagent {
                        status: ActivityStatus::Active,
                        ..
                    }]
                )
        },
    )
    .await;
    let child_id = child_of(&steered);

    client
        .interrupt_session(child_id)
        .await
        .expect("the Subagent's Session accepts the stop");
    let child = settled_session(client, child_id, 0).await;
    assert_eq!(child.turns[0].status, TurnStatus::Interrupted);
    // The trailing output the stop left queued has been read by the time the stopped Subagent's
    // row settles, since the parent's feed carries the stop the same actor makes after it.
    session_where(
        client,
        &mut feed,
        session_id,
        "the stopped Subagent's row settles",
        |snapshot| {
            matches!(
                subagent_rows(snapshot)[..],
                [Activity::Subagent {
                    status: ActivityStatus::Interrupted,
                    ..
                }]
            )
        },
    )
    .await;
    let child = settled_session(client, child_id, 0).await;
    assert_eq!(
        outline(&child, Some(session_id)),
        [
            "turn 0: delegation from the parent: Sleep three times.",
            "turn 0: command sleep 12",
        ],
        "the steer the stopped Subagent never read stands nowhere"
    );

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}
