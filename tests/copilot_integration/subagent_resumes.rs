//! Resuming a settled Copilot Subagent (ADR 0033): every message Copilot delivers to a Subagent
//! whose stretch has settled runs the same agent again on its own context, whatever its
//! `delivery` says — `queued` when it was sent while the Subagent still worked and held until
//! that stretch's `subagent.completed`, `idle` when it was sent afterwards. Copilot opens the new
//! run with no `subagent.started` and closes it with nothing, so Suru settles it where the agent
//! loop exits.
//!
//! Each resume is a second Turn in the Subagent's one Session, opened by the message as its
//! Delegation, and a new row in the delegating Agent's Transcript leading into that Session
//! (ADR 0031), while the Subagents Section still lists the Subagent once, where it spawned.
//!
//! The replays are live captures, sanitized and committed under `fixtures/`: CLI 1.0.87's
//! `queued` sends (docs/validation/0397-copilot-subagent-steer.md) and CLI 1.0.88's `idle` one
//! (docs/validation/0398-copilot-subagent-resume.md).

use crate::{
    server_support::PROGRESS_DEADLINE,
    support::{
        conversation_fixture, opened_session, replay_capture, settled_session,
        subagent_children as children, transcript_outline as outline,
    },
};
use suru::{
    managed_client::{ManagedClient, SubagentTreeEvent},
    protocol::{SessionId, SessionSnapshot, TurnStatus},
};
use tokio::time::timeout;

/// The main agent spawns a Subagent to run three slow shell commands one at a time, then
/// `write_agent`s it a message while its first command runs. The CLI queues it: the agent
/// finishes its stretch without it, and only after its `subagent.completed` does the message
/// reach its loop, `delivery: "queued"`, as a new run the agent answers in one model call.
const PARENT_SEND_QUEUED: &str = include_str!("fixtures/steer-parent-queued.jsonl");

/// The main agent spawns the same Subagent as `agent-B`, and then `agent-A`, which finds B with
/// `list_agents`, `write_agent`s it while B's first command runs, and settles at once. B is
/// queued the message exactly as it was from the main agent, `source` naming A — which settled
/// long before B consumes it.
const SIBLING_SEND_QUEUED: &str = include_str!("fixtures/steer-sibling-queued.jsonl");

/// The main agent spawns an agent that runs one command and reports, waits for it with
/// `read_agent`, and only then `write_agent`s it a follow-up. The message reaches the settled
/// agent as `delivery: "idle"`, and its new run takes a tool round before it answers.
const SENT_AFTER_SETTLE: &str = include_str!("fixtures/resume-idle.jsonl");

/// The text the `queued` captures send.
const QUEUED_MESSAGE: &str = "STEER-MARKER: also say the word PINEAPPLE in your final report";

/// The follow-up the `idle` capture sends.
const FOLLOW_UP: &str = "FOLLOW-UP: now run `echo two` with one bash tool call and write a \
                         one-sentence report that names both words you have printed so far, and \
                         say PINEAPPLE.";

/// The Subagents Section's entries for the tree `session_id` heads, as (name, the Session each
/// hangs under).
async fn subagents_section(
    client: &ManagedClient,
    session_id: SessionId,
) -> Vec<(String, SessionId)> {
    let mut tree = client.subscribe_subagent_tree(session_id);
    let Some(SubagentTreeEvent::Snapshot(tree)) = timeout(PROGRESS_DEADLINE, tree.next())
        .await
        .expect("the Subagent tree arrives")
    else {
        panic!("the Subagent tree subscription opens with its snapshot");
    };
    tree.subagents
        .into_iter()
        .map(|entry| (entry.name, entry.parent_session_id))
        .collect()
}

/// The lines of `outline` that stand in the Turn at `turn`.
fn in_turn(outline: &[String], turn: usize) -> Vec<&str> {
    let prefix = format!("turn {turn}: ");
    outline
        .iter()
        .filter(|line| line.starts_with(&prefix))
        .map(String::as_str)
        .collect()
}

/// Whether an outline line is a row recording a `write_agent` send, as a Command or a Tool Call.
fn records_a_send(line: &str) -> bool {
    line.contains(": command write_agent") || line.contains(": tool call write_agent")
}

/// `snapshot`'s Subagent rows as outline lines, in Transcript order.
fn subagent_rows(snapshot: &SessionSnapshot, parent: Option<SessionId>) -> Vec<String> {
    outline(snapshot, parent)
        .into_iter()
        .filter(|line| line.contains(": subagent "))
        .collect()
}

#[tokio::test]
async fn a_message_queued_during_the_stretch_resumes_the_subagent_once_it_settled() {
    let copilot = conversation_fixture(&replay_capture(PARENT_SEND_QUEUED, |_| true));
    let opened = opened_session(
        &copilot,
        "copilot-resume-parent-queued",
        "Steer the sleeper",
    )
    .await;
    let (client, session_id) = (&opened.client, opened.session_id);
    // The parent's loop goes idle while the Subagent works, settling the Turn the Subagent
    // outlives; the Subagent's settling wakes the loop into a Continuation.
    let parent = settled_session(client, session_id, 1).await;

    assert_eq!(
        parent.turns[1].prompt_id, None,
        "the woken loop's work is a Continuation"
    );
    assert_eq!(
        subagent_rows(&parent, None),
        [
            "turn 0: subagent sequential-sleeps (Run three bash sleeps sequentially) Completed",
            &format!("turn 1: subagent sequential-sleeps ({QUEUED_MESSAGE}) Completed"),
        ],
        "the resume stands as a second row, in the Continuation the Subagent's settling began, \
         described by what it asked"
    );
    assert!(
        outline(&parent, None)
            .iter()
            .all(|line| !records_a_send(line)),
        "the send itself adds no row: {:?}",
        outline(&parent, None)
    );
    let [(_, spawned), (_, resumed)] = children(&parent)[..] else {
        unreachable!()
    };
    assert_eq!(
        spawned, resumed,
        "both rows lead into the Subagent's one Session"
    );

    let child = settled_session(client, spawned, 1).await;
    assert_eq!(
        child
            .turns
            .iter()
            .map(|turn| turn.status)
            .collect::<Vec<_>>(),
        [TurnStatus::Completed, TurnStatus::Completed],
        "the resumed stretch is the Session's second Turn, settled where its loop exited"
    );
    let child_outline = outline(&child, Some(session_id));
    assert!(
        !in_turn(&child_outline, 0)
            .iter()
            .any(|line| line.contains(QUEUED_MESSAGE)),
        "the stretch that was working never received the message: {child_outline:#?}"
    );
    let resumed_turn = in_turn(&child_outline, 1);
    assert_eq!(
        resumed_turn.first().copied(),
        Some(format!("turn 1: delegation from the parent: {QUEUED_MESSAGE}").as_str()),
        "the message opens the resumed Turn as the parent's Delegation: {child_outline:#?}"
    );
    assert!(
        resumed_turn
            .iter()
            .any(|line| line.starts_with("turn 1: Agent: ") && line.contains("PINEAPPLE")),
        "the resumed run's answer, which acts on the message, lands in its Turn: \
         {child_outline:#?}"
    );
    assert_eq!(
        child.turns[1]
            .agent
            .as_ref()
            .map(|agent| agent.selection.model.as_str()),
        Some("gpt-5.6-luna"),
        "the Subagent's Model carries into the resumed Turn"
    );

    assert_eq!(
        subagents_section(client, session_id).await,
        [("sequential-sleeps".to_owned(), session_id)],
        "the Subagents Section lists the resumed Subagent once, where it spawned"
    );

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn a_message_sent_to_a_settled_subagent_resumes_it() {
    let copilot = conversation_fixture(&replay_capture(SENT_AFTER_SETTLE, |_| true));
    let opened = opened_session(&copilot, "copilot-resume-idle", "Resume the agent").await;
    let (client, session_id) = (&opened.client, opened.session_id);
    let parent = settled_session(client, session_id, 0).await;

    assert_eq!(
        subagent_rows(&parent, None),
        [
            "turn 0: subagent echo-one-agent (Run echo one in background agent) Completed",
            &format!("turn 0: subagent echo-one-agent ({FOLLOW_UP}) Completed"),
        ],
    );
    assert!(
        outline(&parent, None)
            .iter()
            .all(|line| !records_a_send(line)),
        "{:?}",
        outline(&parent, None)
    );
    let [(_, spawned), (_, resumed)] = children(&parent)[..] else {
        unreachable!()
    };
    assert_eq!(spawned, resumed);

    let child = settled_session(client, spawned, 1).await;
    assert_eq!(
        child
            .turns
            .iter()
            .map(|turn| turn.status)
            .collect::<Vec<_>>(),
        [TurnStatus::Completed, TurnStatus::Completed]
    );
    let child_outline = outline(&child, Some(session_id));
    assert_eq!(
        in_turn(&child_outline, 1),
        [
            format!("turn 1: delegation from the parent: {FOLLOW_UP}").as_str(),
            "turn 1: command echo two",
            "turn 1: Agent: The words printed so far are `one` and `two`; PINEAPPLE.",
        ],
        "the resumed run, tool round and all, is the Session's second Turn: {child_outline:#?}"
    );

    assert_eq!(
        subagents_section(client, session_id).await,
        [("echo-one-agent".to_owned(), session_id)]
    );

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn a_settled_siblings_message_resumes_the_subagent_from_a_continuation_of_its_session() {
    let copilot = conversation_fixture(&replay_capture(SIBLING_SEND_QUEUED, |_| true));
    let opened = opened_session(
        &copilot,
        "copilot-resume-sibling-queued",
        "Steer the sleeper",
    )
    .await;
    let (client, session_id) = (&opened.client, opened.session_id);
    let parent = settled_session(client, session_id, 0).await;

    assert_eq!(
        subagent_rows(&parent, None),
        [
            "turn 0: subagent agent-B (Start agent B to run three bash commands and report) \
             Completed",
            "turn 0: subagent agent-A (Start agent A to message sibling agent B to steer its \
             report) Completed",
        ],
        "a resume the sibling delegated adds no row to the main agent's Transcript"
    );
    let [(_, b), (_, a)] = children(&parent)[..] else {
        unreachable!()
    };

    let sibling = settled_session(client, a, 1).await;
    assert_eq!(
        sibling
            .turns
            .iter()
            .map(|turn| turn.status)
            .collect::<Vec<_>>(),
        [TurnStatus::Completed, TurnStatus::Completed],
        "the settled sibling's Session gains a Continuation for the resume it delegated"
    );
    let sibling_outline = outline(&sibling, Some(session_id));
    assert_eq!(
        in_turn(&sibling_outline, 1),
        [format!("turn 1: subagent agent-B ({QUEUED_MESSAGE}.) Completed").as_str()],
        "the Continuation holds the resume's row and nothing else: {sibling_outline:#?}"
    );
    assert!(
        sibling_outline.iter().all(|line| !records_a_send(line)),
        "the sibling's send adds no row: {sibling_outline:#?}"
    );
    let [(_, resumed)] = children(&sibling)[..] else {
        panic!(
            "the sibling holds the one resume row, got {:?}",
            sibling.activities
        );
    };
    assert_eq!(
        resumed, b,
        "the row leads into the resumed Subagent's own Session"
    );

    let resumed = settled_session(client, b, 1).await;
    assert_eq!(
        resumed
            .turns
            .iter()
            .map(|turn| turn.status)
            .collect::<Vec<_>>(),
        [TurnStatus::Completed, TurnStatus::Completed]
    );
    let resumed_outline = outline(&resumed, Some(session_id));
    assert_eq!(
        in_turn(&resumed_outline, 1).first().copied(),
        Some(format!("turn 1: delegation from agent-A: {QUEUED_MESSAGE}.").as_str()),
        "the resumed Turn opens with the sibling's Delegation, naming it: {resumed_outline:#?}"
    );
    assert_eq!(
        subagents_section(client, session_id).await,
        [
            ("agent-B".to_owned(), session_id),
            ("agent-A".to_owned(), session_id),
        ],
        "B is listed once, where it spawned, not again under the sibling that resumed it"
    );

    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}
