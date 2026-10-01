//! Claude compacting a conversation's context, in the Transcript: `system/status` reading
//! `compacting` opens a Compaction — restated every half minute while the CLI summarises, which is
//! still the one occasion — `system/compact_boundary` completes it with the `pre_tokens` and
//! `post_tokens` the CLI measured, and a `status` reporting `compact_result: "failed"` fails it
//! with the CLI's `compact_error`. A child's boundary, attributed by `parent_tool_use_id`, is the
//! Subagent's Compaction and stands in its own Session. The failure the CLI reports for a
//! compaction Suru interrupted is the stop Suru asked for.
//!
//! The wire shapes mirror the 2.1.283 CLI's own schema for these messages.

use crate::support::{
    CLAUDE_MODELS, ScriptedClaude, discovery_arms, interrupt_arm, opened_session, session_where,
    settled_session, user_turn_arm,
};
use suru::protocol::{
    Activity, ActivityStatus, CompactionTrigger, ContextFill, MessageRole, SessionSnapshot,
    TurnStatus,
};

const INIT: &str = r#"      emit '{"type":"system","subtype":"init","session_id":"prov-session","model":"claude-fixture-1"}'
"#;

/// The CLI saying it is compacting the loop's context, as it does when it starts and every half
/// minute after for as long as it summarises.
const COMPACTING: &str = r#"      emit '{"type":"system","subtype":"status","status":"compacting","uuid":"status-compacting","session_id":"prov-session"}'
"#;

/// The boundary a completed automatic compaction leaves, with the context it measured before and
/// after.
const BOUNDARY: &str = r#"      emit '{"type":"system","subtype":"compact_boundary","uuid":"boundary-1","compact_metadata":{"trigger":"auto","pre_tokens":182000,"post_tokens":31000,"duration_ms":41000,"preserved_segment":{"head_uuid":"head-1","anchor_uuid":"summary-1","tail_uuid":"tail-1"}},"session_id":"prov-session"}'
"#;

/// What follows a completed compaction: the status settling, and the summary the CLI hands the
/// loop as a synthetic user message, which is no Message of the user's.
const AFTER_BOUNDARY: &str = r#"      emit '{"type":"system","subtype":"status","status":null,"compact_result":"success","uuid":"status-success","session_id":"prov-session"}'
      emit '{"type":"user","isSynthetic":true,"uuid":"summary-1","message":{"role":"user","content":"This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.\n\nThe parser work is half done."},"parent_tool_use_id":null,"session_id":"prov-session"}'
"#;

/// What the CLI writes when an interrupt stops a compaction: the compaction's own failure, then
/// the aborted loop's result.
const CANCELLED: &str = r#"      emit '{"type":"system","subtype":"status","status":null,"compact_result":"failed","compact_error":"Request was aborted.","uuid":"status-cancelled","session_id":"prov-session"}'
      emit '{"type":"result","subtype":"error_during_execution","is_error":true,"duration_ms":11,"num_turns":1,"terminal_reason":"aborted_streaming","session_id":"prov-session"}'
"#;

const FAILED: &str = r#"      emit '{"type":"system","subtype":"status","status":null,"compact_result":"failed","compact_error":"Conversation too long to summarise","uuid":"status-failed","session_id":"prov-session"}'
"#;

/// The loop answering once it is past the compaction.
const ANSWER: &str = r#"      emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":"Carrying on."}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null,"session_id":"prov-session"}'
"#;

const RESULT: &str = r#"      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":1200,"num_turns":1,"result":"Carrying on.","session_id":"prov-session"}'
"#;

/// A `get_context_usage` arm measuring the compacted context.
const CONTEXT_ARM: &str = r#"    *'"subtype":"get_context_usage"'*)
      emit '{"type":"control_response","response":{"subtype":"success","request_id":"'"$request_id"'","response":{"totalTokens":31000,"rawMaxTokens":200000,"maxTokens":180000,"model":"claude-fixture-1"}}}'
      ;;
"#;

/// `timeline` played in the background once the test lets the fixture holding at `gate` carry on,
/// so the loop keeps reading what Suru sends meanwhile.
fn after(gate: &str, timeline: &str) -> String {
    format!(
        "      (\n        while [ ! -e \"{gate}\" ]; do sleep 0.01; done\n{timeline}      ) &\n"
    )
}

fn fixture(timeline: &str) -> ScriptedClaude {
    ScriptedClaude::new(&format!(
        "{}{}{CONTEXT_ARM}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(timeline)
    ))
}

fn compactions(snapshot: &SessionSnapshot) -> Vec<&Activity> {
    snapshot
        .activities
        .iter()
        .filter(|activity| matches!(activity, Activity::Compaction { .. }))
        .collect()
}

fn compaction_statuses(snapshot: &SessionSnapshot) -> Vec<ActivityStatus> {
    compactions(snapshot)
        .into_iter()
        .filter_map(Activity::status)
        .collect()
}

#[tokio::test]
async fn an_automatic_compaction_mid_turn_goes_active_then_completes_with_the_boundarys_counts() {
    let measured = "$CLAUDE_FIXTURE_RELEASE-measured";
    let claude = fixture(&format!(
        "{INIT}{COMPACTING}{}",
        after(
            "$CLAUDE_FIXTURE_RELEASE",
            &format!(
                "{COMPACTING}{BOUNDARY}{AFTER_BOUNDARY}{}",
                after(measured, &format!("{ANSWER}{RESULT}"))
            ),
        )
    ));
    let opened = opened_session(&claude, "claude-compaction", "Keep going on the parser").await;
    let mut feed = opened
        .client
        .subscribe_session(opened.session_id)
        .await
        .expect("subscribe to Session SSE");

    let compacting = session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "Claude starts compacting",
        |snapshot| !compactions(snapshot).is_empty(),
    )
    .await;
    let [
        Activity::Compaction {
            id,
            turn_id,
            status,
            trigger,
            before_tokens,
            after_tokens,
            error,
        },
    ] = compactions(&compacting)[..]
    else {
        unreachable!()
    };
    assert_eq!(*turn_id, compacting.turns[0].id);
    assert_eq!(*status, ActivityStatus::Active, "the Compaction runs");
    assert_eq!(*trigger, CompactionTrigger::Automatic);
    assert_eq!((*before_tokens, *after_tokens, error), (None, None, &None));
    let id = *id;

    claude.release();
    let compacted = session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "the compaction completes and Context Fill is read again",
        |snapshot| {
            compaction_statuses(snapshot) == [ActivityStatus::Completed]
                && snapshot.session.context_fill.is_some()
        },
    )
    .await;
    assert_eq!(
        compactions(&compacted),
        vec![&Activity::Compaction {
            id,
            turn_id: compacted.turns[0].id,
            status: ActivityStatus::Completed,
            trigger: CompactionTrigger::Automatic,
            before_tokens: Some(182_000),
            after_tokens: Some(31_000),
            error: None,
        }],
        "the restated status is the same Compaction, which the boundary completes with its counts"
    );
    assert_eq!(
        compacted.turns[0].status,
        TurnStatus::Active,
        "the Turn the Compaction fell in works on past it"
    );
    assert_eq!(
        compacted.session.context_fill,
        Some(ContextFill {
            occupied_tokens: 31_000,
            capacity_tokens: Some(200_000),
        }),
        "Context Fill is read again after the compaction, before the Turn settles"
    );

    claude.release_gate("measured");
    let settled = settled_session(&opened.client, opened.session_id, 0).await;
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    assert_eq!(compaction_statuses(&settled), [ActivityStatus::Completed]);
    assert_eq!(
        settled
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::User)
            .count(),
        1,
        "the summary the CLI hands the loop is no Message of the user's"
    );
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn a_failed_compaction_settles_failed_with_claudes_error_and_leaves_the_turn_to_claude() {
    let claude = fixture(&format!("{INIT}{COMPACTING}{FAILED}{ANSWER}{RESULT}"));
    let opened = opened_session(&claude, "claude-compaction-failed", "Keep going").await;
    let settled = settled_session(&opened.client, opened.session_id, 0).await;

    let [Activity::Compaction { status, error, .. }] = compactions(&settled)[..] else {
        panic!("one Compaction is recorded: {:?}", settled.activities);
    };
    assert_eq!(*status, ActivityStatus::Failed);
    assert_eq!(error.as_deref(), Some("Conversation too long to summarise"));
    assert_eq!(
        settled.turns[0].status,
        TurnStatus::Completed,
        "the Turn Settles as Claude's result says"
    );
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn a_compaction_cut_off_by_a_failed_loop_settles_failed_with_its_turn() {
    let claude = fixture(&format!(
        r#"{INIT}{COMPACTING}      emit '{{"type":"result","subtype":"error_during_execution","is_error":true,"duration_ms":300,"num_turns":1,"session_id":"prov-session"}}'
"#
    ));
    let opened = opened_session(&claude, "claude-compaction-cut-off", "Keep going").await;
    let settled = settled_session(&opened.client, opened.session_id, 0).await;

    assert_eq!(settled.turns[0].status, TurnStatus::Failed);
    assert_eq!(
        compaction_statuses(&settled),
        [ActivityStatus::Failed],
        "a Compaction still Active when its Turn Settles Settles with it"
    );
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn a_compaction_claude_reports_failing_after_suru_interrupted_it_settles_interrupted() {
    let claude = ScriptedClaude::new(&format!(
        "{}{}{}{CONTEXT_ARM}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(&format!("{INIT}{COMPACTING}")),
        interrupt_arm(CANCELLED),
    ));
    let opened = opened_session(&claude, "claude-compaction-interrupted", "Keep going").await;
    let mut feed = opened
        .client
        .subscribe_session(opened.session_id)
        .await
        .expect("subscribe to Session SSE");
    session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "Claude starts compacting",
        |snapshot| compaction_statuses(snapshot) == [ActivityStatus::Active],
    )
    .await;

    opened
        .client
        .interrupt_session(opened.session_id)
        .await
        .expect("Claude acknowledges the interrupt");
    let settled = settled_session(&opened.client, opened.session_id, 0).await;

    assert_eq!(settled.turns[0].status, TurnStatus::Interrupted);
    let [Activity::Compaction { status, error, .. }] = compactions(&settled)[..] else {
        panic!("one Compaction is recorded: {:?}", settled.activities);
    };
    assert_eq!(
        *status,
        ActivityStatus::Interrupted,
        "the failure Claude reports for the compaction Suru stopped is the stop it asked for"
    );
    assert_eq!(*error, None);
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

/// A spawn whose subagent's conversation compacts before its task settles. Claude reports a
/// subagent's compaction by its boundary alone, attributed to the spawning tool use.
const SUBAGENT_COMPACTS_TURN: &str = r#"      emit '{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"task_1","name":"Agent","input":{"description":"Scout the workspace","prompt":"scout the workspace","subagent_type":"Explore"}}},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"content_block_stop","index":0},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"stream_event","event":{"type":"message_stop"},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_started","task_id":"agent-task-1","tool_use_id":"task_1","description":"Scout the workspace","task_type":"local_agent","subagent_type":"Explore","session_id":"prov-session"}'
      emit '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Scouting."}]},"parent_tool_use_id":"task_1","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"compact_boundary","uuid":"child-boundary","compact_metadata":{"trigger":"auto","pre_tokens":90000,"post_tokens":12000},"parent_tool_use_id":"task_1","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"task_notification","task_id":"agent-task-1","status":"completed","summary":"Found one file.","session_id":"prov-session"}'
      emit '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"task_1","content":"Found one file.","is_error":false}]},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":2100,"num_turns":1,"result":"One file.","session_id":"prov-session"}'
"#;

#[tokio::test]
async fn a_subagents_compaction_stands_in_its_own_session_and_never_its_parents() {
    let claude = fixture(SUBAGENT_COMPACTS_TURN);
    let opened = opened_session(&claude, "claude-compaction-child", "Scout").await;
    let parent = settled_session(&opened.client, opened.session_id, 0).await;
    let child_id = parent
        .activities
        .iter()
        .find_map(|activity| match activity {
            Activity::Subagent { session_id, .. } => Some(*session_id),
            _ => None,
        })
        .expect("the spawn opens a child Session");

    assert!(
        compactions(&parent).is_empty(),
        "the parent's Transcript carries nothing of its Subagent's Compaction: {:?}",
        parent.activities
    );
    let child = settled_session(&opened.client, child_id, 0).await;
    let [
        Activity::Compaction {
            turn_id,
            status,
            before_tokens,
            after_tokens,
            ..
        },
    ] = compactions(&child)[..]
    else {
        panic!(
            "the Subagent's Compaction stands in its own Session: {:?}",
            child.activities
        );
    };
    assert_eq!(*turn_id, child.turns[0].id);
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(
        (*before_tokens, *after_tokens),
        (Some(90_000), Some(12_000))
    );
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn a_compaction_after_the_turn_settled_begins_a_continuation() {
    let claude = fixture(&format!(
        "{INIT}{ANSWER}{RESULT}{}",
        after(
            "$CLAUDE_FIXTURE_RELEASE",
            &format!("{COMPACTING}{BOUNDARY}{AFTER_BOUNDARY}{ANSWER}{RESULT}")
        )
    ));
    let opened = opened_session(&claude, "claude-compaction-continuation", "Keep going").await;
    let first = settled_session(&opened.client, opened.session_id, 0).await;
    assert!(compactions(&first).is_empty());

    claude.release();
    let continued = settled_session(&opened.client, opened.session_id, 1).await;
    assert!(
        continued.turns[1].is_continuation(),
        "the compaction began a Continuation: {:?}",
        continued.turns
    );
    assert_eq!(continued.turns[1].status, TurnStatus::Completed);
    let [
        Activity::Compaction {
            turn_id, status, ..
        },
    ] = compactions(&continued)[..]
    else {
        panic!("one Compaction is recorded: {:?}", continued.activities);
    };
    assert_eq!(*turn_id, continued.turns[1].id);
    assert_eq!(*status, ActivityStatus::Completed);
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}
