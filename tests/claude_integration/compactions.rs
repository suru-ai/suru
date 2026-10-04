//! Claude compacting a conversation's context, in the Transcript: `system/status` reading
//! `compacting` opens a Compaction — restated every half minute while the CLI summarises, which is
//! still the one occasion — `system/compact_boundary` completes it with the `pre_tokens` and
//! `post_tokens` the CLI measured and the summary in the synthetic user message the boundary
//! anchors, which is never a Message of the user's, and a `status` reporting
//! `compact_result: "failed"` fails it with the CLI's `compact_error`. A child's boundary,
//! attributed by `parent_tool_use_id`, is the Subagent's Compaction and stands in its own Session.
//! The failure the CLI reports for a compaction Suru interrupted is the stop Suru asked for, and a
//! compaction that starts after the Turn settled runs in a loop of its own, which an interrupt or
//! the next Prompt stops.
//!
//! A boundary's completion waits for the summary that follows it, so nothing the CLI reports after
//! the boundary — a reading of the compacted context, an intervention, the loop's end, a failure —
//! reaches the reader ahead of the completed Compaction.
//!
//! A Compaction the user asks for is Claude's own `/compact`, sent as a user message in a Turn of
//! its own (ADR 0041), with the user's instructions for the summary as its argument. Its closing `result` reads success whatever happened, so the Turn Settles as
//! the compaction's `status` and boundary, or the failed `local_command_outcome` the CLI answers a
//! refusal with, say. The `<local-command-stdout>` replay and the synthetic assistant message the
//! CLI writes for a local command are its plumbing, recorded as nothing. Interrupting it sends
//! Claude's own `interrupt`, and the failure the CLI then reports is the stop Suru asked for: the
//! Compaction and its Turn Settle interrupted, measuring nothing, whatever the `result` says.
//!
//! The wire shapes mirror the 2.1.283 CLI's own schema for these messages, and what the 2.1.283
//! CLI was seen to write for `/compact` (docs/validation/0462-claude-manual-compaction.md).

use crate::server_support::PROGRESS_DEADLINE;
use crate::support::{
    CLAUDE_MODELS, ScriptedClaude, discovery_arms, interrupt_arm, opened_session, session_where,
    settled_session, user_turn_arm,
};
use suru::protocol::{
    Activity, ActivityStatus, AdmitPromptRequest, CompactSessionRequest, CompactionTrigger,
    ContextFill, Cost, InitialPrompt, MessageRole, PromptDelivery, PromptId, SessionError,
    SessionErrorCode, SessionSnapshot, TurnStatus, Usage,
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
/// loop as a synthetic user message — the one the boundary anchors — wrapped in the lead-in,
/// heading, and trailers the 2.1.283 CLI writes around what its summariser wrote. It is the
/// Compaction's summary, and no Message of the user's.
const AFTER_BOUNDARY: &str = r#"      emit '{"type":"system","subtype":"status","status":null,"compact_result":"success","uuid":"status-success","session_id":"prov-session"}'
      emit '{"type":"user","isSynthetic":true,"uuid":"summary-1","message":{"role":"user","content":"This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.\n\nSummary:\n1. Primary Request and Intent:\n   Finish the parser.\n\n2. Pending Tasks:\n   - The lexer\n\nIf you need specific details from before compaction (like exact code snippets, error messages, or content you generated), read the full transcript at: /home/user/.claude/projects/work/prov-session.jsonl\nContinue the conversation from where it left off without asking the user any further questions. Resume directly \u2014 do not acknowledge the summary, do not recap what was happening, do not preface with \"I'\''ll continue\" or similar. Pick up the last task as if the break never happened."},"parent_tool_use_id":null,"session_id":"prov-session"}'
"#;

/// The summary in [`AFTER_BOUNDARY`], as the reader meets it.
const SUMMARY: &str =
    "1. Primary Request and Intent:\n   Finish the parser.\n\n2. Pending Tasks:\n   - The lexer";

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
    format!("      (\n        wait_for \"{gate}\"\n{timeline}      ) &\n")
}

/// A user-message arm playing `first` for the Session's first Prompt and `later` for each after.
fn prompts_arm(first: &str, later: &str) -> String {
    format!(
        r#"    *'"type":"user"'*)
      prompts=$(( ${{prompts:-0}} + 1 ))
      if [ "$prompts" -eq 1 ]; then
{first}      else
{later}      fi
      ;;
"#
    )
}

/// A Session whose first Turn settles before Claude, once released, starts compacting on its own;
/// an interrupt stops that compaction, and every later Prompt is simply answered.
fn compacting_after_the_turn() -> ScriptedClaude {
    ScriptedClaude::new(&format!(
        "{}{}{}{CONTEXT_ARM}",
        discovery_arms(CLAUDE_MODELS),
        prompts_arm(
            &format!(
                "{INIT}{ANSWER}{RESULT}{}",
                after("$CLAUDE_FIXTURE_RELEASE", COMPACTING)
            ),
            &format!("{ANSWER}{RESULT}"),
        ),
        interrupt_arm(CANCELLED),
    ))
}

/// Waits for the Compaction Claude began after the first Turn settled to stand Active.
async fn compacting_in_a_continuation(
    claude: &ScriptedClaude,
    opened: &crate::support::OpenedSession,
    feed: &mut suru::managed_client::SessionSubscription,
) -> SessionSnapshot {
    settled_session(&opened.client, opened.session_id, 0).await;
    claude.release();
    session_where(
        &opened.client,
        feed,
        opened.session_id,
        "Claude compacts after the Turn settled",
        |snapshot| compaction_statuses(snapshot) == [ActivityStatus::Active],
    )
    .await
}

/// Where in the CLI's input Suru's interrupt stands, and where the user message at `index` does.
fn interrupt_and_user_message(claude: &ScriptedClaude, index: usize) -> (usize, usize) {
    let requests = claude.requests();
    let interrupt = requests
        .iter()
        .position(|request| request["request"]["subtype"] == "interrupt")
        .unwrap_or_else(|| panic!("Suru interrupts the CLI: {requests:?}"));
    let message = requests
        .iter()
        .enumerate()
        .filter(|(_, request)| request["type"] == "user")
        .nth(index)
        .map(|(position, _)| position)
        .unwrap_or_else(|| panic!("the CLI receives user message {index}: {requests:?}"));
    (interrupt, message)
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
            instructions,
            before_tokens,
            after_tokens,
            error,
            summary,
            summary_truncated,
        },
    ] = compactions(&compacting)[..]
    else {
        unreachable!()
    };
    assert_eq!(*turn_id, compacting.turns[0].id);
    assert_eq!(*status, ActivityStatus::Active, "the Compaction runs");
    assert_eq!(*trigger, CompactionTrigger::Automatic);
    assert_eq!(instructions, &None, "no one asked anything of its summary");
    assert_eq!((*before_tokens, *after_tokens, error), (None, None, &None));
    assert_eq!(
        (summary, *summary_truncated),
        (&None, false),
        "nothing is known yet of the summary it will leave"
    );
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
            instructions: None,
            before_tokens: Some(182_000),
            after_tokens: Some(31_000),
            error: None,
            summary: Some(SUMMARY.to_owned()),
            summary_truncated: false,
        }],
        "the restated status is the same Compaction, which the boundary completes with its counts \
         and the summary it anchors, stripped of the CLI's wrapping"
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

#[tokio::test]
async fn interrupting_a_compaction_begun_after_the_turn_settled_stops_claudes_loop() {
    let claude = compacting_after_the_turn();
    let opened = opened_session(&claude, "claude-compaction-idle-interrupt", "Keep going").await;
    let mut feed = opened
        .client
        .subscribe_session(opened.session_id)
        .await
        .expect("subscribe to Session SSE");
    let compacting = compacting_in_a_continuation(&claude, &opened, &mut feed).await;
    assert!(compacting.turns[1].is_continuation());

    opened
        .client
        .interrupt_session(opened.session_id)
        .await
        .expect("Claude acknowledges the interrupt");
    let settled = settled_session(&opened.client, opened.session_id, 1).await;

    interrupt_and_user_message(&claude, 0);
    assert_eq!(
        settled.turns[1].status,
        TurnStatus::Interrupted,
        "the Continuation Settles on the result of the loop the interrupt stopped"
    );
    assert_eq!(compaction_statuses(&settled), [ActivityStatus::Interrupted]);
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn a_prompt_delivered_during_a_compaction_begun_after_the_turn_settled_stops_it_first() {
    let claude = compacting_after_the_turn();
    let opened = opened_session(&claude, "claude-compaction-idle-prompt", "Keep going").await;
    let mut feed = opened
        .client
        .subscribe_session(opened.session_id)
        .await
        .expect("subscribe to Session SSE");
    compacting_in_a_continuation(&claude, &opened, &mut feed).await;

    opened
        .client
        .admit_prompt(
            opened.session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: "Now the lexer".to_owned(),
                    skill_invocations: Vec::new(),
                    attachments: Vec::new(),
                },
                delivery: PromptDelivery::Steer,
            },
        )
        .await
        .expect("deliver the next Prompt");
    let answered = settled_session(&opened.client, opened.session_id, 2).await;

    let (interrupt, prompt) = interrupt_and_user_message(&claude, 1);
    assert!(
        interrupt < prompt,
        "Claude's compaction is stopped before the Prompt reaches it"
    );
    assert_eq!(
        answered.turns[1].status,
        TurnStatus::Interrupted,
        "the Continuation Settles on the result of the loop the interrupt stopped"
    );
    let [
        Activity::Compaction {
            turn_id, status, ..
        },
    ] = compactions(&answered)[..]
    else {
        panic!("one Compaction is recorded: {:?}", answered.activities);
    };
    assert_eq!(*turn_id, answered.turns[1].id);
    assert_eq!(*status, ActivityStatus::Interrupted);
    assert_eq!(answered.turns[2].status, TurnStatus::Completed);
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

/// What the CLI writes for `/compact` up to the boundary it leaves once it has compacted: the
/// status going `compacting` and then reporting success, a fresh `init`, and the boundary with what
/// it measured. Like the boundary the 2.1.283 CLI was seen to leave, it names no preserved segment.
const COMPACTING_ON_REQUEST: &str = r#"      emit '{"type":"system","subtype":"status","status":"compacting","uuid":"status-compacting","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"status","status":null,"compact_result":"success","uuid":"status-success","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"init","session_id":"prov-session","model":"claude-fixture-1"}'
      emit '{"type":"system","subtype":"compact_boundary","uuid":"boundary-1","compact_metadata":{"trigger":"manual","pre_tokens":182000,"post_tokens":31000,"cumulative_dropped_tokens":151000,"duration_ms":12045},"logical_parent_uuid":"parent-1","session_id":"prov-session"}'
"#;

/// What the CLI writes for `/compact` after its boundary: the summary it hands the loop, the
/// replay of the command's own output, and a `result` that metered no loop call but carries the
/// running Cost the summarising added to.
const COMPACTED_AFTER_BOUNDARY: &str = r#"      emit '{"type":"user","isSynthetic":true,"isReplay":false,"uuid":"summary-1","message":{"role":"user","content":"This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.\n\nThe parser work is half done."},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"user","isReplay":true,"uuid":"replay-1","message":{"role":"user","content":"<local-command-stdout>Compacted </local-command-stdout>"},"parent_tool_use_id":null,"session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":12055,"duration_api_ms":0,"num_turns":0,"result":"","local_command":"compact","total_cost_usd":0.0162725,"usage":{"input_tokens":0,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0},"session_id":"prov-session"}'
"#;

/// The first Turn's `result`, which leaves the running Cost a compaction's adds to.
const RESULT_COSTING: &str = r#"      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":1200,"num_turns":1,"result":"Carrying on.","total_cost_usd":0.009345,"usage":{"input_tokens":10,"output_tokens":51},"session_id":"prov-session"}'
"#;

/// What the CLI writes for `/compact` when there is nothing to compact: no `status` at all, only the
/// synthetic assistant message carrying the command's error output and its failed outcome, then a
/// `result` reading success.
const NOTHING_TO_COMPACT: &str = r#"      emit '{"type":"assistant","message":{"id":"local-1","model":"<synthetic>","role":"assistant","type":"message","stop_reason":"end_turn","usage":{"input_tokens":0,"output_tokens":0},"content":[{"type":"text","text":"Error: No messages to compact"}]},"parent_tool_use_id":null,"local_command_source":"<local-command-stderr>Error: No messages to compact</local-command-stderr>","local_command_run":{"command":"compact","args":""},"local_command_outcome":{"kind":"failed"},"uuid":"local-1","session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":16,"duration_api_ms":0,"num_turns":0,"result":"","local_command":"compact","total_cost_usd":0.009345,"usage":{"input_tokens":0,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0},"session_id":"prov-session"}'
"#;

/// What the CLI writes for `/compact` when summarising fails: the compaction's own failed `status`,
/// then the command's error output, which restates the failure, and a `result` reading success.
const COMPACTION_FAILED_ON_REQUEST: &str = r#"      emit '{"type":"system","subtype":"status","status":"compacting","uuid":"status-compacting","session_id":"prov-session"}'
      emit '{"type":"system","subtype":"status","status":null,"compact_result":"failed","compact_error":"Conversation too long to summarise","uuid":"status-failed","session_id":"prov-session"}'
      emit '{"type":"assistant","message":{"id":"local-1","model":"<synthetic>","role":"assistant","type":"message","stop_reason":"end_turn","usage":{"input_tokens":0,"output_tokens":0},"content":[{"type":"text","text":"Error: Error during compaction: Conversation too long to summarise"}]},"parent_tool_use_id":null,"local_command_source":"<local-command-stderr>Error: Error during compaction: Conversation too long to summarise</local-command-stderr>","local_command_run":{"command":"compact","args":""},"local_command_outcome":{"kind":"failed"},"uuid":"local-1","session_id":"prov-session"}'
      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":900,"num_turns":0,"result":"","local_command":"compact","usage":{"input_tokens":0,"output_tokens":0},"session_id":"prov-session"}'
"#;

/// A Session whose first Prompt Claude simply answers, and which answers `/compact` — with
/// instructions or without — with `compaction`.
fn compacting_on_request(compaction: &str) -> ScriptedClaude {
    ScriptedClaude::new(&format!(
        r#"{}    *'"text":"/compact'*)
{compaction}      ;;
{}{CONTEXT_ARM}"#,
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(&format!("{INIT}{ANSWER}{RESULT_COSTING}")),
    ))
}

/// Opens a Session on `claude`, lets its first Turn settle, asks for a Compaction, and answers the
/// Session once the Turn that request began has Settled.
async fn compacted_on_request(
    claude: &ScriptedClaude,
    name: &'static str,
) -> (
    crate::support::OpenedSession,
    SessionSnapshot,
    SessionSnapshot,
) {
    let opened = opened_session(claude, name, "Keep going on the parser").await;
    let before = settled_session(&opened.client, opened.session_id, 0).await;
    opened
        .client
        .compact_session(opened.session_id, CompactSessionRequest::default())
        .await
        .expect("the idle Session takes the request");
    let settled = settled_session(&opened.client, opened.session_id, 1).await;
    (opened, before, settled)
}

fn user_messages_sent(claude: &ScriptedClaude) -> Vec<String> {
    claude
        .requests()
        .into_iter()
        .filter(|request| request["type"] == "user")
        .map(|request| {
            request["message"]["content"]
                .as_array()
                .and_then(|blocks| blocks.last())
                .and_then(|block| block["text"].as_str())
                .unwrap_or_default()
                .to_owned()
        })
        .collect()
}

/// Every Message the Session holds, by who said it and what.
fn messages(snapshot: &SessionSnapshot) -> Vec<(MessageRole, &str)> {
    snapshot
        .messages
        .iter()
        .map(|message| (message.role.clone(), message.content.as_str()))
        .collect()
}

#[tokio::test]
async fn a_requested_compaction_is_claudes_compact_command_in_a_turn_that_settles_as_it_did() {
    let claude = compacting_on_request(&format!(
        "{COMPACTING_ON_REQUEST}{COMPACTED_AFTER_BOUNDARY}"
    ));
    let (opened, before, settled) =
        compacted_on_request(&claude, "claude-compaction-requested").await;

    assert_eq!(
        user_messages_sent(&claude),
        ["Keep going on the parser", "/compact"],
        "Claude is asked through its own command"
    );
    let turn = &settled.turns[1];
    assert!(turn.compaction_requested);
    assert_eq!(turn.status, TurnStatus::Completed);
    assert_eq!(
        compactions(&settled),
        vec![&Activity::Compaction {
            id: compactions(&settled)[0].id(),
            turn_id: turn.id,
            status: ActivityStatus::Completed,
            trigger: CompactionTrigger::Manual,
            instructions: None,
            before_tokens: Some(182_000),
            after_tokens: Some(31_000),
            error: None,
            summary: Some("The parser work is half done.".to_owned()),
            summary_truncated: false,
        }],
        "one manual Compaction completes with what the boundary measured, and the summary the \
         synthetic message after it carries — a boundary that kept no messages names none"
    );
    assert_eq!(
        messages(&settled),
        messages(&before),
        "neither the summary, the command's replayed output, nor the command itself is a Message"
    );
    assert_eq!(
        (turn.cost, settled.total_cost.map(|total| total.cost)),
        (Cost::from_usd(0.0162725), Cost::from_usd(0.0162725)),
        "the Turn records the running Cost Claude reports, so the Session's counts what the \
         summarising added to it once"
    );
    assert_eq!(
        turn.usage,
        Some(Usage::default()),
        "the result metered no loop call, so the Turn states no token count"
    );
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn instructions_for_a_requested_compaction_are_claudes_compact_argument_and_stay_on_it() {
    const INSTRUCTIONS: &str = "Keep the parser notes\nand the lexer plan";
    let claude = compacting_on_request(&format!(
        "{COMPACTING_ON_REQUEST}{COMPACTED_AFTER_BOUNDARY}"
    ));
    let opened = opened_session(
        &claude,
        "claude-compaction-instructions",
        "Keep going on the parser",
    )
    .await;
    settled_session(&opened.client, opened.session_id, 0).await;
    opened
        .client
        .compact_session(
            opened.session_id,
            CompactSessionRequest {
                instructions: Some(INSTRUCTIONS.to_owned()),
            },
        )
        .await
        .expect("Claude takes instructions, so the idle Session takes the request");
    let settled = settled_session(&opened.client, opened.session_id, 1).await;

    assert_eq!(
        user_messages_sent(&claude),
        [
            "Keep going on the parser",
            "/compact Keep the parser notes\nand the lexer plan"
        ],
        "the instructions are `/compact`'s argument, whole across their lines"
    );
    let turn = &settled.turns[1];
    assert_eq!(turn.status, TurnStatus::Completed);
    assert_eq!(
        compactions(&settled),
        vec![&Activity::Compaction {
            id: compactions(&settled)[0].id(),
            turn_id: turn.id,
            status: ActivityStatus::Completed,
            trigger: CompactionTrigger::Manual,
            instructions: Some(INSTRUCTIONS.to_owned()),
            before_tokens: Some(182_000),
            after_tokens: Some(31_000),
            error: None,
            summary: Some("The parser work is half done.".to_owned()),
            summary_truncated: false,
        }],
        "the manual Compaction keeps what it was asked to keep beside the summary it left"
    );
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

/// The most of a Session command's body the server reads: 64 KiB, which a Compaction request's
/// instructions stand under as a Prompt's text does.
const SESSION_COMMAND_LIMIT: usize = 64 * 1024;

/// The longest instructions a Compaction request's body has room for, `over` bytes past it, in
/// characters JSON writes as they are and with no whitespace for the server to trim.
fn instructions_filling_the_request(over: usize) -> String {
    let envelope = serde_json::to_vec(&CompactSessionRequest {
        instructions: Some(String::new()),
    })
    .expect("encode a Compaction request")
    .len();
    "keep-the-parser-notes;"
        .chars()
        .cycle()
        .take(SESSION_COMMAND_LIMIT - envelope + over)
        .collect()
}

/// The content of every user message the CLI was sent, in the order it arrived.
fn user_message_contents(claude: &ScriptedClaude) -> Vec<serde_json::Value> {
    claude
        .requests()
        .into_iter()
        .filter(|request| request["type"] == "user")
        .map(|request| request["message"]["content"].clone())
        .collect()
}

#[tokio::test]
async fn instructions_reading_as_a_command_or_a_flag_or_filling_the_request_reach_claude_whole() {
    for (name, instructions) in [
        // The CLI reads a command only from the head of a message's text, so a second slash is
        // part of `/compact`'s argument rather than a command of its own.
        (
            "claude-compaction-instructions-slash",
            "/clear the lexer notes, keep the parser plan".to_owned(),
        ),
        // `/compact` has no flags: the 2.1.283 CLI hands its whole argument to the summariser,
        // where commands such as `/model` and `/config` answer `--help` with their usage.
        ("claude-compaction-instructions-flag", "--help".to_owned()),
        (
            "claude-compaction-instructions-largest",
            instructions_filling_the_request(0),
        ),
    ] {
        let claude = compacting_on_request(&format!(
            "{COMPACTING_ON_REQUEST}{COMPACTED_AFTER_BOUNDARY}"
        ));
        let opened = opened_session(&claude, name, "Keep going on the parser").await;
        let before = settled_session(&opened.client, opened.session_id, 0).await;
        opened
            .client
            .compact_session(
                opened.session_id,
                CompactSessionRequest {
                    instructions: Some(instructions.clone()),
                },
            )
            .await
            .unwrap_or_else(|error| {
                panic!("{name}: the idle Session takes the request: {error:#}")
            });
        let settled = settled_session(&opened.client, opened.session_id, 1).await;

        let sent = user_message_contents(&claude);
        let command =
            serde_json::json!([{"type": "text", "text": format!("/compact {instructions}")}]);
        assert!(
            sent.len() == 2 && sent[1] == command,
            "{name}: Claude is sent `/compact` with the {} bytes of instructions as its argument, \
             whole and alone in the message: {}",
            instructions.len(),
            sent.last()
                .map(|content| content.to_string().chars().take(200).collect::<String>())
                .unwrap_or_default()
        );
        assert_eq!(settled.turns[1].status, TurnStatus::Completed, "{name}");
        let [
            Activity::Compaction {
                status,
                instructions: kept,
                ..
            },
        ] = compactions(&settled)[..]
        else {
            panic!(
                "{name}: one Compaction is recorded: {:?}",
                settled.activities
            );
        };
        assert_eq!(*status, ActivityStatus::Completed, "{name}");
        assert!(
            kept.as_deref() == Some(instructions.as_str()),
            "{name}: the Compaction keeps the {} bytes of instructions as given, not {:?} bytes",
            instructions.len(),
            kept.as_ref().map(String::len)
        );
        assert_eq!(
            messages(&settled),
            messages(&before),
            "{name}: the instructions are no Message of the user's"
        );
        opened
            .server
            .shutdown()
            .await
            .expect("shut the server down");
    }
}

#[tokio::test]
async fn instructions_overfilling_the_request_are_refused_before_claude_is_asked() {
    let claude = compacting_on_request(&format!(
        "{COMPACTING_ON_REQUEST}{COMPACTED_AFTER_BOUNDARY}"
    ));
    let opened = opened_session(
        &claude,
        "claude-compaction-instructions-overfilled",
        "Keep going on the parser",
    )
    .await;
    settled_session(&opened.client, opened.session_id, 0).await;

    let refused = opened
        .client
        .compact_session(
            opened.session_id,
            CompactSessionRequest {
                instructions: Some(instructions_filling_the_request(1)),
            },
        )
        .await
        .expect_err("a request one byte past what the server reads is refused");
    assert_eq!(
        refused
            .downcast_ref::<SessionError>()
            .map(|error| error.code),
        Some(SessionErrorCode::InvalidCommand),
        "{refused:#}"
    );
    let unchanged = opened
        .client
        .read_session(opened.session_id)
        .await
        .expect("read the Session");
    assert_eq!(
        unchanged.turns.len(),
        1,
        "no Turn begins for a refused request: {:?}",
        unchanged.turns
    );
    assert_eq!(
        user_messages_sent(&claude),
        ["Keep going on the parser"],
        "Claude is never asked, so no part of the instructions is sent in their place"
    );
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn claudes_refusal_to_compact_fails_the_compaction_and_its_turn_whatever_the_result_says() {
    let claude = compacting_on_request(NOTHING_TO_COMPACT);
    let (opened, before, settled) =
        compacted_on_request(&claude, "claude-compaction-nothing").await;

    let turn = &settled.turns[1];
    assert_eq!(
        turn.status,
        TurnStatus::Failed,
        "the result reads success, but nothing was compacted"
    );
    let [
        Activity::Compaction {
            turn_id,
            status,
            trigger,
            before_tokens,
            after_tokens,
            error,
            ..
        },
    ] = compactions(&settled)[..]
    else {
        panic!("one Compaction is recorded: {:?}", settled.activities);
    };
    assert_eq!(*turn_id, turn.id);
    assert_eq!(*status, ActivityStatus::Failed);
    assert_eq!(*trigger, CompactionTrigger::Manual);
    assert_eq!((*before_tokens, *after_tokens), (None, None));
    assert_eq!(
        error.as_deref(),
        Some("No messages to compact"),
        "the Compaction fails with the CLI's own words"
    );
    assert_eq!(
        messages(&settled),
        messages(&before),
        "the synthetic message carrying the command's output is no Agent Message"
    );
    assert_eq!(
        (
            turn.usage.clone(),
            turn.cost,
            settled.total_cost.map(|total| total.cost)
        ),
        (
            Some(Usage::default()),
            Cost::from_usd(0.009345),
            Cost::from_usd(0.009345)
        ),
        "the running Cost Claude reports unchanged is a refusal that spent nothing, and its \
         zero usage states no token count"
    );
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn a_requested_compaction_claude_fails_settles_failed_with_claudes_error_once() {
    let claude = compacting_on_request(COMPACTION_FAILED_ON_REQUEST);
    let (opened, before, settled) =
        compacted_on_request(&claude, "claude-compaction-requested-failed").await;

    assert_eq!(settled.turns[1].status, TurnStatus::Failed);
    let [Activity::Compaction { status, error, .. }] = compactions(&settled)[..] else {
        panic!(
            "the command's restated failure is the same Compaction: {:?}",
            settled.activities
        );
    };
    assert_eq!(*status, ActivityStatus::Failed);
    assert_eq!(error.as_deref(), Some("Conversation too long to summarise"));
    assert_eq!(messages(&settled), messages(&before));
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

/// What the CLI writes once Suru's interrupt stops its `/compact`: the compaction's own failed
/// `status`, carrying the error the aborted summarising call threw.
const COMPACTION_CANCELLED_STATUS: &str = r#"      emit '{"type":"system","subtype":"status","status":null,"compact_result":"failed","compact_error":"API Error: Request was aborted.","uuid":"status-cancelled","session_id":"prov-session"}'
"#;

/// The command's own account of the cancellation, which restates it as a failure.
const COMPACT_COMMAND_CANCELLED: &str = r#"      emit '{"type":"assistant","message":{"id":"local-1","model":"<synthetic>","role":"assistant","type":"message","stop_reason":"end_turn","usage":{"input_tokens":0,"output_tokens":0},"content":[{"type":"text","text":"Error: Compaction canceled."}]},"parent_tool_use_id":null,"local_command_source":"<local-command-stderr>Error: Compaction canceled.</local-command-stderr>","local_command_run":{"command":"compact","args":""},"local_command_outcome":{"kind":"failed"},"uuid":"local-1","session_id":"prov-session"}'
"#;

/// The `result` closing a `/compact` loop the interrupt stopped, which reads success with zero
/// usage all the same.
const COMPACT_RESULT_AFTER_CANCEL: &str = r#"      emit '{"type":"result","subtype":"success","is_error":false,"duration_ms":2100,"duration_api_ms":0,"num_turns":0,"result":"","local_command":"compact","total_cost_usd":0.009345,"usage":{"input_tokens":0,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0},"session_id":"prov-session"}'
"#;

/// The Standing inputs' latest Turn for `session_id` in the Session listing, from which a Client
/// reads whether the Session stands Failed.
async fn listed_latest_turn(
    client: &suru::managed_client::ManagedClient,
    session_id: suru::protocol::SessionId,
) -> Option<TurnStatus> {
    client
        .list_sessions(None)
        .await
        .expect("list Sessions")
        .into_iter()
        .find_map(|item| match item {
            suru::protocol::SessionListItem::Readable(summary)
                if summary.session.id == session_id =>
            {
                summary
                    .standing_inputs
                    .latest_turn
                    .map(|latest| latest.status)
            }
            _ => None,
        })
}

#[tokio::test]
async fn interrupting_a_requested_compaction_stops_claudes_compact_and_settles_both_interrupted() {
    for (described, cancellation) in [
        (
            "the failed status, then a result reading success",
            format!("{COMPACTION_CANCELLED_STATUS}{COMPACT_RESULT_AFTER_CANCEL}"),
        ),
        (
            "the failed status, the command restating it, then a result reading success",
            format!(
                "{COMPACTION_CANCELLED_STATUS}{COMPACT_COMMAND_CANCELLED}{COMPACT_RESULT_AFTER_CANCEL}"
            ),
        ),
        (
            "the failed status, then the aborted loop's result",
            CANCELLED.to_owned(),
        ),
    ] {
        let claude = ScriptedClaude::new(&format!(
            r#"{}    *'"text":"/compact"'*)
{COMPACTING}      ;;
{}{}{CONTEXT_ARM}"#,
            discovery_arms(CLAUDE_MODELS),
            interrupt_arm(&cancellation),
            user_turn_arm(&format!("{INIT}{ANSWER}{RESULT_COSTING}")),
        ));
        let opened = opened_session(
            &claude,
            "claude-compaction-requested-interrupted",
            "Keep going on the parser",
        )
        .await;
        let mut feed = opened
            .client
            .subscribe_session(opened.session_id)
            .await
            .expect("subscribe to Session SSE");
        settled_session(&opened.client, opened.session_id, 0).await;
        let before = session_where(
            &opened.client,
            &mut feed,
            opened.session_id,
            "Claude's context is read after the first Turn",
            |snapshot| snapshot.session.context_fill.is_some(),
        )
        .await;
        opened
            .client
            .compact_session(opened.session_id, CompactSessionRequest::default())
            .await
            .expect("the idle Session takes the request");
        let compacting = session_where(
            &opened.client,
            &mut feed,
            opened.session_id,
            "Claude starts compacting on request",
            |snapshot| compaction_statuses(snapshot) == [ActivityStatus::Active],
        )
        .await;
        assert!(
            matches!(
                compactions(&compacting)[..],
                [Activity::Compaction {
                    before_tokens: Some(31_000),
                    ..
                }]
            ),
            "{described}: while it runs, the Compaction holds the reading before it: {:?}",
            compacting.activities
        );

        opened
            .client
            .interrupt_session(opened.session_id)
            .await
            .expect("Claude acknowledges the interrupt");
        let settled = settled_session(&opened.client, opened.session_id, 1).await;

        let (interrupt, compact) = interrupt_and_user_message(&claude, 1);
        assert!(
            compact < interrupt,
            "{described}: Suru stops the `/compact` with Claude's own interrupt"
        );
        let turn = &settled.turns[1];
        assert!(turn.compaction_requested);
        assert_eq!(
            turn.status,
            TurnStatus::Interrupted,
            "{described}: the Turn Settles as its Compaction did"
        );
        assert_eq!(
            compactions(&settled),
            vec![&Activity::Compaction {
                id: compactions(&settled)[0].id(),
                turn_id: turn.id,
                status: ActivityStatus::Interrupted,
                trigger: CompactionTrigger::Manual,
                instructions: None,
                before_tokens: None,
                after_tokens: None,
                error: None,
                summary: None,
                summary_truncated: false,
            }],
            "{described}: Claude's failed compaction is the stop Suru asked for, and a stopped \
             Compaction left the context as it was, measuring nothing"
        );
        assert!(
            !settled.activities.iter().any(|activity| matches!(
                activity,
                Activity::Error { turn_id, .. } if *turn_id == turn.id
            )),
            "{described}: nothing is recorded as a failure: {:?}",
            settled.activities
        );
        assert_eq!(
            messages(&settled),
            messages(&before),
            "{described}: neither the command nor its account of the stop is a Message"
        );
        assert_eq!(
            listed_latest_turn(&opened.client, opened.session_id).await,
            Some(TurnStatus::Interrupted),
            "{described}: the Session is left no Failed Standing"
        );

        opened
            .client
            .admit_prompt(
                opened.session_id,
                AdmitPromptRequest {
                    prompt: InitialPrompt {
                        id: PromptId::new(),
                        text: "Now the lexer".to_owned(),
                        skill_invocations: Vec::new(),
                        attachments: Vec::new(),
                    },
                    delivery: PromptDelivery::Queue,
                },
            )
            .await
            .expect("the idle Session takes the next Prompt");
        let answered = settled_session(&opened.client, opened.session_id, 2).await;
        assert_eq!(
            user_messages_sent(&claude),
            ["Keep going on the parser", "/compact", "Now the lexer"]
        );
        assert_eq!(
            answered.turns[2].status,
            TurnStatus::Completed,
            "{described}: Claude answers the next Prompt in a Turn of its own"
        );
        assert_eq!(
            compactions(&answered)
                .into_iter()
                .map(|compaction| match compaction {
                    Activity::Compaction {
                        status,
                        before_tokens,
                        after_tokens,
                        ..
                    } => (*status, *before_tokens, *after_tokens),
                    _ => unreachable!(),
                })
                .collect::<Vec<_>>(),
            [(ActivityStatus::Interrupted, None, None)],
            "{described}: no later reading of Claude's context is the stopped Compaction's after"
        );
        assert_eq!(answered.session.working_since, None);
        opened
            .server
            .shutdown()
            .await
            .expect("shut the server down");
    }
}

/// The anchored boundary [`BOUNDARY`] stands for, measuring only the context before it, so what it
/// left is read from the Session's next Context Fill reading.
const UNMEASURED_BOUNDARY: &str = r#"      emit '{"type":"system","subtype":"compact_boundary","uuid":"boundary-1","compact_metadata":{"trigger":"auto","pre_tokens":182000,"preserved_segment":{"head_uuid":"head-1","anchor_uuid":"summary-1","tail_uuid":"tail-1"}},"session_id":"prov-session"}'
"#;

/// A second compaction's boundary, anchoring the summary [`SECOND_SUMMARY`] carries.
const SECOND_BOUNDARY: &str = r#"      emit '{"type":"system","subtype":"compact_boundary","uuid":"boundary-2","compact_metadata":{"trigger":"auto","pre_tokens":90000,"post_tokens":20000,"preserved_segment":{"head_uuid":"head-2","anchor_uuid":"summary-2","tail_uuid":"tail-2"}},"session_id":"prov-session"}'
"#;

const SECOND_SUMMARY: &str = r#"      emit '{"type":"user","isSynthetic":true,"uuid":"summary-2","message":{"role":"user","content":"This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.\n\nSummary:\nThe lexer is next."},"parent_tool_use_id":null,"session_id":"prov-session"}'
"#;

/// A `get_context_usage` arm measuring the compacted context at 31K the first time it is asked,
/// and at 40K — the work done since — every time after.
const GROWING_CONTEXT_ARM: &str = r#"    *'"subtype":"get_context_usage"'*)
      context_reads=$(( ${context_reads:-0} + 1 ))
      if [ "$context_reads" -eq 1 ]; then tokens=31000; else tokens=40000; fi
      emit '{"type":"control_response","response":{"subtype":"success","request_id":"'"$request_id"'","response":{"totalTokens":'"$tokens"',"rawMaxTokens":200000,"maxTokens":180000,"model":"claude-fixture-1"}}}'
      ;;
"#;

/// Waits for the CLI to have been sent `count` control requests of `subtype`.
async fn requested(claude: &ScriptedClaude, subtype: &str, count: usize) {
    tokio::time::timeout(PROGRESS_DEADLINE, async {
        while claude
            .requests()
            .iter()
            .filter(|request| request["request"]["subtype"] == subtype)
            .count()
            < count
        {
            tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("Suru sends the CLI {count} {subtype} requests"));
}

/// Claude asking the user a question under the control request `request_id`, or asking with no
/// questions at all, which Suru cannot present.
fn question(request_id: &str, questions: &str) -> String {
    format!(
        r#"      emit '{{"type":"control_request","request_id":"{request_id}","request":{{"subtype":"can_use_tool","tool_name":"AskUserQuestion","tool_use_id":"question-tool","input":{{"questions":{questions}}}}}}}'
"#
    )
}

const ONE_QUESTION: &str = r#"[{"question":"Which environment?","header":"Environment","options":[{"label":"Local","description":"On this machine"},{"label":"Remote","description":"A separate machine"}],"multiSelect":false}]"#;

/// Claude asking whether its Bash use may run.
const BASH_APPROVAL: &str = r#"      emit '{"type":"control_request","request_id":"approval-1","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"bash-1","input":{"command":"cargo test"}}}'
"#;

fn the_summary(snapshot: &SessionSnapshot) -> Vec<(ActivityStatus, Option<&str>)> {
    compactions(snapshot)
        .into_iter()
        .map(|compaction| match compaction {
            Activity::Compaction {
                status, summary, ..
            } => (*status, summary.as_deref()),
            _ => unreachable!(),
        })
        .collect()
}

#[tokio::test]
async fn a_reading_the_boundary_prompts_waits_behind_the_completion_its_summary_holds_back() {
    let claude = ScriptedClaude::new(&format!(
        "{}{}{GROWING_CONTEXT_ARM}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(&format!(
            "{INIT}{COMPACTING}{UNMEASURED_BOUNDARY}{}",
            after(
                "$CLAUDE_FIXTURE_RELEASE",
                &format!("{AFTER_BOUNDARY}{ANSWER}{RESULT}")
            )
        )),
    ));
    let opened = opened_session(&claude, "claude-compaction-held-reading", "Keep going").await;
    requested(&claude, "get_context_usage", 1).await;
    // The CLI answers the reading as it reads the request; give the answer time to reach Suru,
    // which holds it behind the completion still waiting on its summary.
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    let held = opened
        .client
        .read_session(opened.session_id)
        .await
        .expect("read the Session");
    assert_eq!(
        compaction_statuses(&held),
        [ActivityStatus::Active],
        "the boundary's completion waits for its summary"
    );
    assert_eq!(
        held.session.context_fill, None,
        "and the reading the boundary prompted does not overtake it"
    );

    claude.release();
    let settled = settled_session(&opened.client, opened.session_id, 0).await;
    let [
        Activity::Compaction {
            status,
            before_tokens,
            after_tokens,
            summary,
            ..
        },
    ] = compactions(&settled)[..]
    else {
        panic!("one Compaction is recorded: {:?}", settled.activities);
    };
    assert_eq!(
        (*status, *before_tokens, *after_tokens),
        (ActivityStatus::Completed, Some(182_000), Some(31_000)),
        "the reading the boundary prompted is the first after the Compaction, not one taken once \
         the loop had worked on"
    );
    assert_eq!(summary.as_deref(), Some(SUMMARY));
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn a_compaction_awaiting_its_summary_completes_before_an_intervention_reaches_the_reader() {
    for (name, intervention) in [
        (
            "claude-compaction-then-question",
            question("question-1", ONE_QUESTION),
        ),
        ("claude-compaction-then-approval", BASH_APPROVAL.to_owned()),
    ] {
        let claude = fixture(&format!("{INIT}{COMPACTING}{BOUNDARY}{intervention}"));
        let opened = opened_session(&claude, name, "Keep going").await;
        let mut feed = opened
            .client
            .subscribe_session(opened.session_id)
            .await
            .expect("subscribe to Session SSE");
        let asked = session_where(
            &opened.client,
            &mut feed,
            opened.session_id,
            "Claude's intervention reaches the reader",
            |snapshot| {
                snapshot.activities.iter().any(|activity| {
                    matches!(
                        activity,
                        Activity::Questionnaire { .. } | Activity::Approval { .. }
                    )
                })
            },
        )
        .await;
        assert_eq!(
            the_summary(&asked),
            [(ActivityStatus::Completed, None)],
            "{name}: the Compaction the CLI completed before asking is complete, rather than \
             running while the reader answers"
        );
        opened
            .server
            .shutdown()
            .await
            .expect("shut the server down");
    }
}

#[tokio::test]
async fn a_compaction_awaiting_its_summary_completes_before_a_failure_after_it_ends_the_turn() {
    let claude = fixture(&format!(
        "{INIT}{COMPACTING}{BOUNDARY}{}",
        question("question-1", "[]")
    ));
    let opened = opened_session(&claude, "claude-compaction-then-failure", "Keep going").await;
    let settled = settled_session(&opened.client, opened.session_id, 0).await;

    assert_eq!(settled.turns[0].status, TurnStatus::Failed);
    assert_eq!(
        the_summary(&settled),
        [(ActivityStatus::Completed, None)],
        "the Compaction completed before the CLI went wrong, so the failure is the Turn's alone"
    );
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn a_compaction_awaiting_its_summary_completes_without_one_whatever_ends_the_wait() {
    for (name, ending, turn) in [
        (
            "claude-compaction-then-result",
            RESULT,
            TurnStatus::Completed,
        ),
        (
            "claude-compaction-then-exit",
            "      exit 0\n",
            TurnStatus::Failed,
        ),
        (
            "claude-compaction-then-garbage",
            "      printf '%s\\n' 'not stream-json'\n",
            TurnStatus::Failed,
        ),
    ] {
        let claude = fixture(&format!("{INIT}{COMPACTING}{BOUNDARY}{ending}"));
        let opened = opened_session(&claude, name, "Keep going").await;
        let settled = settled_session(&opened.client, opened.session_id, 0).await;

        assert_eq!(settled.turns[0].status, turn, "{name}");
        assert_eq!(
            the_summary(&settled),
            [(ActivityStatus::Completed, None)],
            "{name}: the Compaction completes without the summary nothing more will bring"
        );
        opened
            .server
            .shutdown()
            .await
            .expect("shut the server down");
    }
}

#[tokio::test]
async fn a_second_boundary_completes_the_first_compaction_without_a_summary() {
    let claude = fixture(&format!(
        "{INIT}{COMPACTING}{BOUNDARY}{SECOND_BOUNDARY}{SECOND_SUMMARY}{ANSWER}{RESULT}"
    ));
    let opened = opened_session(&claude, "claude-compaction-twice", "Keep going").await;
    let settled = settled_session(&opened.client, opened.session_id, 0).await;

    assert_eq!(
        the_summary(&settled),
        [
            (ActivityStatus::Completed, None),
            (ActivityStatus::Completed, Some("The lexer is next.")),
        ],
        "each boundary completes its own Compaction, and only the second's summary came"
    );
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn a_requested_compactions_turn_settles_only_once_its_completion_has_its_summary() {
    let claude = compacting_on_request(&format!(
        "{COMPACTING_ON_REQUEST}{}",
        after("$CLAUDE_FIXTURE_RELEASE", COMPACTED_AFTER_BOUNDARY)
    ));
    let opened = opened_session(&claude, "claude-compaction-requested-held", "Keep going").await;
    settled_session(&opened.client, opened.session_id, 0).await;
    opened
        .client
        .compact_session(opened.session_id, CompactSessionRequest::default())
        .await
        .expect("the idle Session takes the request");
    let mut feed = opened
        .client
        .subscribe_session(opened.session_id)
        .await
        .expect("subscribe to Session SSE");
    let compacting = session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "Claude compacts on request",
        |snapshot| compaction_statuses(snapshot) == [ActivityStatus::Active],
    )
    .await;
    assert_eq!(compacting.turns[1].status, TurnStatus::Active);

    claude.release();
    let settled = settled_session(&opened.client, opened.session_id, 1).await;
    assert_eq!(
        settled.turns[1].status,
        TurnStatus::Completed,
        "the Turn Settles as its Compaction did, which completed before the `result` closed it"
    );
    assert_eq!(
        the_summary(&settled),
        [(
            ActivityStatus::Completed,
            Some("The parser work is half done.")
        )]
    );
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

/// A `get_context_usage` arm measuring the context at 182K once the first Turn has settled — an
/// answer it holds back until the test lets `first-reading` through — at 31K once a compaction has
/// left it, answered only after the first, and at 40K, the work done since, every time after.
const SETTLED_THEN_COMPACTED_CONTEXT_ARM: &str = r#"    *'"subtype":"get_context_usage"'*)
      context_reads=$(( ${context_reads:-0} + 1 ))
      case "$context_reads" in
        1)
          (
            wait_for "$CLAUDE_FIXTURE_RELEASE-first-reading"
            emit '{"type":"control_response","response":{"subtype":"success","request_id":"'"$request_id"'","response":{"totalTokens":182000,"rawMaxTokens":200000,"maxTokens":180000,"model":"claude-fixture-1"}}}'
            : > "$CLAUDE_FIXTURE_RELEASE-first-answered"
          ) &
          ;;
        2)
          (
            wait_for "$CLAUDE_FIXTURE_RELEASE-first-answered"
            emit '{"type":"control_response","response":{"subtype":"success","request_id":"'"$request_id"'","response":{"totalTokens":31000,"rawMaxTokens":200000,"maxTokens":180000,"model":"claude-fixture-1"}}}'
          ) &
          ;;
        *) emit '{"type":"control_response","response":{"subtype":"success","request_id":"'"$request_id"'","response":{"totalTokens":40000,"rawMaxTokens":200000,"maxTokens":180000,"model":"claude-fixture-1"}}}' ;;
      esac
      ;;
"#;

#[tokio::test]
async fn a_reading_a_late_boundary_prompts_measures_the_continuation_its_completion_begins() {
    // The loop compacts once its Turn has settled, reporting the boundary with no `compacting`
    // before it, so nothing has begun a Continuation by the time the boundary prompts a reading,
    // and the summary is held back until that reading is in. The reading the settled Turn
    // prompted is answered after the boundary too, and ahead of the boundary's, though it
    // measures the context before.
    let claude = ScriptedClaude::new(&format!(
        "{}{}{SETTLED_THEN_COMPACTED_CONTEXT_ARM}",
        discovery_arms(CLAUDE_MODELS),
        user_turn_arm(&format!(
            "{INIT}{ANSWER}{RESULT}{}",
            after(
                "$CLAUDE_FIXTURE_RELEASE",
                &format!(
                    "{UNMEASURED_BOUNDARY}{}",
                    after(
                        "$CLAUDE_FIXTURE_RELEASE-summary",
                        &format!("{AFTER_BOUNDARY}{ANSWER}{RESULT}")
                    )
                )
            )
        )),
    ));
    let opened = opened_session(&claude, "claude-compaction-late-reading", "Keep going").await;
    let first = settled_session(&opened.client, opened.session_id, 0).await;
    assert!(compactions(&first).is_empty());
    requested(&claude, "get_context_usage", 1).await;

    claude.release();
    requested(&claude, "get_context_usage", 2).await;
    claude.release_gate("first-reading");
    // The CLI answers each reading as soon as it may; give both answers time to reach Suru
    // before the summary does.
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    claude.release_gate("summary");
    let continued = settled_session(&opened.client, opened.session_id, 1).await;

    assert!(
        continued.turns[1].is_continuation(),
        "the compaction began a Continuation: {:?}",
        continued.turns
    );
    let [
        Activity::Compaction {
            turn_id,
            status,
            before_tokens,
            after_tokens,
            summary,
            ..
        },
    ] = compactions(&continued)[..]
    else {
        panic!("one Compaction is recorded: {:?}", continued.activities);
    };
    assert_eq!(*turn_id, continued.turns[1].id);
    assert_eq!(
        (*status, *before_tokens, *after_tokens),
        (ActivityStatus::Completed, Some(182_000), Some(31_000)),
        "the reading the boundary prompted measures the Continuation the completion began, and is \
         the first after the Compaction — not the reading taken before it that arrived late, nor \
         one taken once the loop had worked on"
    );
    assert_eq!(summary.as_deref(), Some(SUMMARY));
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}
