//! Codex compacting a thread's context on its own, in the Transcript: the `contextCompaction` item
//! opens a Compaction at `item/started` in the Turn it fell in and completes it at
//! `item/completed`. Codex measures nothing on the item, so the Compaction's Context Fill before and
//! after is the Session's own, read from the thread's token usage either side of it. A compaction
//! that fails ends its native turn failed, with Codex's account of why, and the Compaction fails
//! with it; one an interrupt cuts off is stopped with its Turn. A child thread's compaction stands
//! in its Subagent's Session. The deprecated `thread/compacted` notification and the `warning`
//! Codex sends after compacting record nothing.
//!
//! A Compaction the user asks for is Codex's own `thread/compact/start`, which Codex answers with
//! nothing and runs as a native turn of its own: that turn's `turn/started`, item and
//! `turn/completed` are the Turn Suru opened for the request, never a second Turn or a
//! Continuation, and interrupting it is the ordinary `turn/interrupt` of that turn.
//!
//! The wire shapes and their order follow Codex's app-server: the token usage it reads during a
//! compaction — its summarising call's, then the rebuilt context's — arrives before the item
//! completes, and the turn a requested compaction runs as may be announced before the request is
//! answered or after.

use crate::support::{
    ScriptedCodex, conversation_codex, conversation_codex_with_arms, opened_session,
    opened_session_on, session_where, settled_session,
};
use serde_json::{Value, json};
use suru::protocol::{
    Activity, ActivityStatus, AdmitPromptRequest, CompactSessionRequest, CompactionTrigger,
    InitialPrompt, MessageRole, PromptDelivery, PromptId, SessionError, SessionErrorCode,
    SessionSnapshot, TurnStatus,
};
use suru::provider::CodexRuntime;
use tokio::time::Duration;

/// One scripted line printing the notification `method` with `params`.
fn notify(method: &str, params: Value) -> String {
    format!(
        "      printf '%s\\n' '{}'\n",
        json!({ "method": method, "params": params })
    )
}

/// The `contextCompaction` item reaching `stage` on `thread`'s native `turn`.
fn compaction_item(stage: &str, thread: &str, turn: &str) -> String {
    notify(
        &format!("item/{stage}"),
        json!({
            "threadId": thread,
            "turnId": turn,
            "item": { "type": "contextCompaction", "id": format!("{thread}-compaction") },
        }),
    )
}

/// `thread`'s token usage under `turn`: a running `total`, and the `context` its last request
/// occupied, which is what Context Fill reads.
fn token_usage(thread: &str, turn: &str, total: u64, context: u64) -> String {
    let breakdown = |tokens: u64| {
        json!({
            "totalTokens": tokens,
            "inputTokens": tokens,
            "cachedInputTokens": 0,
            "cacheWriteInputTokens": 0,
            "outputTokens": 0,
            "reasoningOutputTokens": 0,
        })
    };
    notify(
        "thread/tokenUsage/updated",
        json!({
            "threadId": thread,
            "turnId": turn,
            "tokenUsage": {
                "total": breakdown(total),
                "last": breakdown(context),
                "modelContextWindow": 272_000,
            },
        }),
    )
}

/// What Codex says once it has compacted, beside the item: the deprecated notification the item
/// replaced, and its advice to start a new thread.
fn compaction_aftermath(thread: &str, turn: &str) -> String {
    [
        notify(
            "thread/compacted",
            json!({ "threadId": thread, "turnId": turn }),
        ),
        notify(
            "warning",
            json!({
                "threadId": thread,
                "message": "Heads up: Long threads and multiple compactions can cause the model to \
                            be less accurate. Start a new thread when possible to keep threads \
                            small and targeted.",
            }),
        ),
    ]
    .concat()
}

fn agent_message(thread: &str, turn: &str, text: &str) -> String {
    let item = |text: &str| {
        json!({
            "threadId": thread,
            "turnId": turn,
            "item": { "type": "agentMessage", "id": format!("{thread}-answer"), "text": text },
        })
    };
    [
        notify("item/started", item("")),
        notify("item/completed", item(text)),
    ]
    .concat()
}

fn turn_completed(thread: &str, turn: &str) -> String {
    notify(
        "turn/completed",
        json!({ "threadId": thread, "turn": { "id": turn, "status": "completed", "items": [] } }),
    )
}

/// `thread`'s native `turn` failing for `message`, as Codex reports a compaction that failed: an
/// `error`, then the failed turn carrying the same error.
fn turn_failed(thread: &str, turn: &str, message: &str) -> String {
    let error = json!({ "message": message, "codexErrorInfo": null, "additionalDetails": null });
    [
        notify(
            "error",
            json!({ "error": error, "willRetry": false, "threadId": thread, "turnId": turn }),
        ),
        notify(
            "turn/completed",
            json!({
                "threadId": thread,
                "turn": { "id": turn, "status": "failed", "error": error, "items": [] },
            }),
        ),
    ]
    .concat()
}

/// The arm answering Suru's `turn/interrupt` the way Codex does: acknowledging it, then ending the
/// native turn interrupted.
fn interrupt_arm(thread: &str, turn: &str) -> String {
    let interrupted = notify(
        "turn/completed",
        json!({
            "threadId": thread,
            "turn": { "id": turn, "status": "interrupted", "items": [] },
        }),
    );
    format!(
        r#"    *'"method":"turn/interrupt"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
      printf '%s\n' '{{"id":'"$id"',"result":{{}}}}'
{interrupted}      ;;
"#
    )
}

/// The lines that hold the scripted Codex until the test releases gate `index`.
fn gate(index: usize) -> String {
    format!("      while [ ! -e \"$CODEX_FIXTURE_RELEASE-{index}\" ]; do sleep 0.01; done\n")
}

fn compactions(snapshot: &SessionSnapshot) -> Vec<&Activity> {
    snapshot
        .activities
        .iter()
        .filter(|activity| matches!(activity, Activity::Compaction { .. }))
        .collect()
}

#[tokio::test]
async fn an_automatic_compaction_mid_turn_stands_active_then_completes_measured_either_side() {
    let (thread, turn) = ("native-thread", "native-turn");
    let codex = conversation_codex(
        &[
            token_usage(thread, turn, 182_000, 182_000),
            compaction_item("started", thread, turn),
            // Codex's summarising call, then its estimate of the context it rebuilt.
            token_usage(thread, turn, 372_000, 190_000),
            token_usage(thread, turn, 372_000, 30_000),
            gate(0),
            compaction_item("completed", thread, turn),
            compaction_aftermath(thread, turn),
            agent_message(thread, turn, "Carrying on with the parser."),
            token_usage(thread, turn, 407_000, 35_000),
            token_usage(thread, turn, 445_000, 38_000),
            turn_completed(thread, turn),
        ]
        .concat(),
    );
    let opened = opened_session(&codex, "codex-compaction", "Keep going on the parser").await;

    let compacting = session_where(
        &opened.client,
        opened.session_id,
        "Codex starts compacting",
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
    assert_eq!(
        (*before_tokens, *after_tokens, error),
        (Some(182_000), None, &None),
        "it begins from the Context Fill last read before it"
    );
    assert_eq!((summary, *summary_truncated), (&None, false));
    let id = *id;

    codex.release_turn(0);
    let settled = settled_session(&opened.client, opened.session_id, 0).await;
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    assert_eq!(
        settled.activities,
        [Activity::Compaction {
            id,
            turn_id: settled.turns[0].id,
            status: ActivityStatus::Completed,
            trigger: CompactionTrigger::Automatic,
            instructions: None,
            before_tokens: Some(182_000),
            after_tokens: Some(35_000),
            error: None,
            summary: None,
            summary_truncated: false,
        }],
        "the item completes the one Compaction where it stood, measured after by the first \
         reading once it settled; the deprecated notification and the warning record nothing"
    );
    assert_eq!(
        settled
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::Agent)
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        ["Carrying on with the parser."],
        "the Turn works on past its Compaction"
    );
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn a_compaction_with_nothing_read_either_side_of_it_carries_no_counts() {
    let (thread, turn) = ("native-thread", "native-turn");
    let codex = conversation_codex(
        &[
            compaction_item("started", thread, turn),
            compaction_item("completed", thread, turn),
            agent_message(thread, turn, "Carrying on."),
            turn_completed(thread, turn),
        ]
        .concat(),
    );
    let opened = opened_session(&codex, "codex-compaction-unmeasured", "Keep going").await;
    let settled = settled_session(&opened.client, opened.session_id, 0).await;

    let [
        Activity::Compaction {
            status,
            before_tokens,
            after_tokens,
            ..
        },
    ] = compactions(&settled)[..]
    else {
        panic!("one Compaction is recorded: {:?}", settled.activities);
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(
        (*before_tokens, *after_tokens),
        (None, None),
        "a side nothing was read for is absent rather than guessed"
    );
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn codexs_compaction_notices_beside_the_item_record_nothing() {
    let (thread, turn) = ("native-thread", "native-turn");
    let codex = conversation_codex(
        &[
            compaction_aftermath(thread, turn),
            agent_message(thread, turn, "Done."),
            turn_completed(thread, turn),
        ]
        .concat(),
    );
    let opened = opened_session(&codex, "codex-compaction-notices", "Keep going").await;
    let settled = settled_session(&opened.client, opened.session_id, 0).await;

    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    assert!(
        settled.activities.is_empty(),
        "neither `thread/compacted` nor the warning is a Compaction, or anything else: {:?}",
        settled.activities
    );
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

/// A root Turn that spawns the V2 agent `/root/scout`, whose thread compacts while it works, read
/// either side, before the root Turn completes.
fn compacting_child() -> ScriptedCodex {
    let child = ("child-thread", "child-turn");
    child_compacting(
        &[
            compaction_item("completed", child.0, child.1),
            compaction_aftermath(child.0, child.1),
            agent_message(child.0, child.1, "Scouted."),
            token_usage(child.0, child.1, 102_000, 12_000),
            turn_completed(child.0, child.1),
        ]
        .concat(),
    )
}

/// A root Turn that spawns the V2 agent `/root/scout`, whose thread, read once, starts compacting
/// and then plays `ending`, ending its native turn, before the root Turn completes.
fn child_compacting(ending: &str) -> ScriptedCodex {
    let (root, child) = (("root-thread", "root-turn"), ("child-thread", "child-turn"));
    let activity = |kind: &str| {
        notify(
            "item/completed",
            json!({
                "threadId": root.0,
                "turnId": root.1,
                "item": {
                    "type": "subAgentActivity",
                    "id": format!("activity-{kind}"),
                    "kind": kind,
                    "agentThreadId": child.0,
                    "agentPath": "/root/scout",
                },
            }),
        )
    };
    let child_work = [
        token_usage(child.0, child.1, 90_000, 90_000),
        compaction_item("started", child.0, child.1),
        ending.to_owned(),
        activity("completed"),
        turn_completed(root.0, root.1),
    ]
    .concat();
    ScriptedCodex::new_multiprocess(&format!(
        r#"
    *'"method":"initialize"'*)
      printf '%s\n' '{{"id":1,"result":{{}}}}'
      ;;
    *'"method":"config/read"'*)
      printf '%s\n' '{{"id":2,"result":{{"config":{{}},"origins":{{}}}}}}'
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' '{{"id":3,"result":{{"thread":{{"id":"root-thread"}},"model":"gpt-fixture"}}}}'
      ;;
    *'"method":"turn/start"'*)
      printf '%s\n' '{{"id":4,"result":{{"turn":{{"id":"root-turn"}}}}}}'
{root_reading}{spawn}      ;;
    *'"method":"thread/resume"'*)
      printf '%s\n' '{{"id":5,"result":{{"thread":{{"id":"child-thread","parentThreadId":"root-thread"}},"model":"gpt-fixture"}}}}'
{child_work}      ;;
"#,
        root_reading = token_usage(root.0, root.1, 150_000, 150_000),
        spawn = activity("started"),
    ))
}

#[tokio::test]
async fn a_subagents_compaction_stands_in_its_own_session_measured_by_its_own_context_fill() {
    let codex = compacting_child();
    let opened = opened_session(&codex, "codex-compaction-subagent", "Scout the seams").await;
    let parent = settled_session(&opened.client, opened.session_id, 0).await;

    let [
        Activity::Subagent {
            session_id: child_id,
            ..
        },
    ] = &parent.activities[..]
    else {
        panic!(
            "the Subagent row is all the parent's Transcript carries of the child's work: {:?}",
            parent.activities
        );
    };
    assert_eq!(
        parent.session.context_fill.map(|fill| fill.occupied_tokens),
        Some(150_000),
        "the child's readings are no Context Fill of the parent's"
    );

    let child = session_where(
        &opened.client,
        *child_id,
        "the Subagent's Turn settles",
        |snapshot| {
            snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.status != TurnStatus::Active)
        },
    )
    .await;
    let [
        Activity::Compaction {
            turn_id,
            status,
            trigger,
            before_tokens,
            after_tokens,
            ..
        },
    ] = compactions(&child)[..]
    else {
        panic!(
            "the child's Compaction stands in its own Session: {:?}",
            child.activities
        );
    };
    assert_eq!(*turn_id, child.turns[0].id);
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(*trigger, CompactionTrigger::Automatic);
    assert_eq!(
        (*before_tokens, *after_tokens),
        (Some(90_000), Some(12_000)),
        "measured by the Subagent's own Context Fill either side of it"
    );
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn a_compaction_that_fails_settles_failed_with_codexs_account_of_why() {
    let (thread, turn) = ("native-thread", "native-turn");
    let codex = conversation_codex(
        &[
            token_usage(thread, turn, 182_000, 182_000),
            compaction_item("started", thread, turn),
            turn_failed(thread, turn, "Context window exceeded while compacting"),
        ]
        .concat(),
    );
    let opened = opened_session(&codex, "codex-compaction-failed", "Keep going").await;
    let settled = settled_session(&opened.client, opened.session_id, 0).await;

    assert_eq!(settled.turns[0].status, TurnStatus::Failed);
    let [
        Activity::Compaction {
            status,
            before_tokens,
            after_tokens,
            error,
            ..
        },
    ] = compactions(&settled)[..]
    else {
        panic!("one Compaction is recorded: {:?}", settled.activities);
    };
    assert_eq!(*status, ActivityStatus::Failed);
    assert_eq!(
        error.as_deref(),
        Some("Context window exceeded while compacting"),
        "the Compaction keeps Codex's account of why it failed"
    );
    assert_eq!(
        (*before_tokens, *after_tokens),
        (Some(182_000), None),
        "a failed Compaction freed nothing to measure after it"
    );
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn a_compaction_an_interrupt_cuts_off_is_stopped_with_its_turn() {
    let (thread, turn) = ("native-thread", "native-turn");
    let codex = conversation_codex_with_arms(
        &compaction_item("started", thread, turn),
        &interrupt_arm(thread, turn),
    );
    let opened = opened_session(&codex, "codex-compaction-interrupted", "Keep going").await;
    session_where(
        &opened.client,
        opened.session_id,
        "Codex starts compacting",
        |snapshot| !compactions(snapshot).is_empty(),
    )
    .await;

    opened
        .client
        .interrupt_session(opened.session_id)
        .await
        .expect("Codex acknowledges the interrupt");
    let settled = settled_session(&opened.client, opened.session_id, 0).await;

    assert_eq!(settled.turns[0].status, TurnStatus::Interrupted);
    let [Activity::Compaction { status, error, .. }] = compactions(&settled)[..] else {
        panic!("one Compaction is recorded: {:?}", settled.activities);
    };
    assert_eq!(
        (*status, error),
        (ActivityStatus::Interrupted, &None),
        "the Compaction Suru stopped is stopped, with no failure to explain"
    );
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn a_subagents_failed_compaction_keeps_codexs_account_of_why_in_its_own_session() {
    let codex = child_compacting(&turn_failed(
        "child-thread",
        "child-turn",
        "Context window exceeded while compacting",
    ));
    let opened = opened_session(&codex, "codex-compaction-subagent-failed", "Scout").await;
    let parent = settled_session(&opened.client, opened.session_id, 0).await;
    let [
        Activity::Subagent {
            session_id: child_id,
            ..
        },
    ] = &parent.activities[..]
    else {
        panic!(
            "the parent holds only the Subagent row: {:?}",
            parent.activities
        );
    };

    let child = session_where(
        &opened.client,
        *child_id,
        "the Subagent's Turn settles",
        |snapshot| {
            snapshot
                .turns
                .first()
                .is_some_and(|turn| turn.status != TurnStatus::Active)
        },
    )
    .await;
    let [Activity::Compaction { status, error, .. }] = compactions(&child)[..] else {
        panic!(
            "the child's Compaction stands in its own Session: {:?}",
            child.activities
        );
    };
    assert_eq!(*status, ActivityStatus::Failed);
    assert_eq!(
        error.as_deref(),
        Some("Context window exceeded while compacting")
    );
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

/// The native turn Codex runs a requested compaction as.
const COMPACTION_TURN: &str = "compact-turn";

/// Codex answering `thread/compact/start`, with nothing.
const ACCEPT_COMPACTION: &str = r#"      printf '%s\n' '{"id":'"$id"',"result":{}}'
"#;

/// A scripted Codex holding one conversation on `native-thread`. Each Prompt begins a native turn
/// that reads the context it occupies — 182,000 tokens for the first, 35,000 for any after — and
/// answers at once, unless `first_turn_gate` holds the first one until the test releases it.
/// `thread/compact/start` plays `compaction`: [`ACCEPT_COMPACTION`], and the native turn Codex runs
/// the compaction as, in whichever order the test has Codex write them. `arms` answers anything
/// else.
fn compacting_on_request(compaction: &str, arms: &str, first_turn_gate: bool) -> ScriptedCodex {
    let held = if first_turn_gate {
        gate(0)
    } else {
        "        :\n".to_owned()
    };
    ScriptedCodex::new_multiprocess(&format!(
        r#"
    *'"method":"initialize"'*)
      printf '%s\n' '{{"id":1,"result":{{}}}}'
      ;;
    *'"method":"config/read"'*)
      id=$(printf '%s' "$line" | sed -n 's/^{{"id":\([0-9]*\),.*/\1/p')
      printf '%s\n' '{{"id":'"$id"',"result":{{"config":{{}},"origins":{{}}}}}}'
      ;;
    *'"method":"thread/start"'*)
      id=$(printf '%s' "$line" | sed -n 's/^{{"id":\([0-9]*\),.*/\1/p')
      printf '%s\n' '{{"id":'"$id"',"result":{{"thread":{{"id":"native-thread"}},"model":"gpt-fixture"}}}}'
      ;;
    *'"method":"turn/start"'*)
      id=$(printf '%s' "$line" | sed -n 's/^{{"id":\([0-9]*\),.*/\1/p')
      prompts=$(( ${{prompts:-0}} + 1 ))
      turn="prompt-turn-$prompts"
      if [ "$prompts" -eq 1 ]; then context=182000; else context=35000; fi
      printf '%s\n' '{{"id":'"$id"',"result":{{"turn":{{"id":"'"$turn"'"}}}}}}'
      printf '%s\n' '{{"method":"thread/tokenUsage/updated","params":{{"threadId":"native-thread","turnId":"'"$turn"'","tokenUsage":{{"total":{{"totalTokens":'"$((prompts * 200000))"',"inputTokens":'"$((prompts * 200000))"',"cachedInputTokens":0,"cacheWriteInputTokens":0,"outputTokens":0,"reasoningOutputTokens":0}},"last":{{"totalTokens":'"$context"',"inputTokens":'"$context"',"cachedInputTokens":0,"cacheWriteInputTokens":0,"outputTokens":0,"reasoningOutputTokens":0}},"modelContextWindow":272000}}}}}}'
      printf '%s\n' '{{"method":"item/started","params":{{"threadId":"native-thread","turnId":"'"$turn"'","item":{{"type":"agentMessage","id":"'"$turn"'-answer","text":""}}}}}}'
      printf '%s\n' '{{"method":"item/completed","params":{{"threadId":"native-thread","turnId":"'"$turn"'","item":{{"type":"agentMessage","id":"'"$turn"'-answer","text":"Answered."}}}}}}'
      if [ "$prompts" -eq 1 ]; then
{held}      fi
      printf '%s\n' '{{"method":"turn/completed","params":{{"threadId":"native-thread","turn":{{"id":"'"$turn"'","status":"completed","items":[]}}}}}}'
      ;;
    *'"method":"thread/resume"'*)
      id=$(printf '%s' "$line" | sed -n 's/^{{"id":\([0-9]*\),.*/\1/p')
      printf '%s\n' '{{"id":'"$id"',"result":{{"thread":{{"id":"native-thread"}},"model":"gpt-fixture"}}}}'
      ;;
    *'"method":"thread/compact/start"'*)
      id=$(printf '%s' "$line" | sed -n 's/^{{"id":\([0-9]*\),.*/\1/p')
{compaction}      ;;
{arms}"#
    ))
}

/// Codex beginning the native turn it runs a requested compaction as, and starting the
/// compaction in it.
fn compaction_turn_started(thread: &str) -> String {
    [
        notify(
            "turn/started",
            json!({
                "threadId": thread,
                "turn": { "id": COMPACTION_TURN, "status": "inProgress", "items": [] },
            }),
        ),
        compaction_item("started", thread, COMPACTION_TURN),
    ]
    .concat()
}

/// Asks for a Compaction of the Session and waits for Codex to start it.
async fn compaction_requested(opened: &crate::support::OpenedSession) -> SessionSnapshot {
    opened
        .client
        .compact_session(opened.session_id, CompactSessionRequest::default())
        .await
        .expect("the idle Session takes the request");
    session_where(
        &opened.client,
        opened.session_id,
        "Codex starts compacting on request",
        |snapshot| !compactions(snapshot).is_empty(),
    )
    .await
}

/// Delivers `text` as the next Prompt to the idle Session.
async fn prompt(opened: &crate::support::OpenedSession, text: &str) {
    opened
        .client
        .admit_prompt(
            opened.session_id,
            AdmitPromptRequest {
                prompt: InitialPrompt {
                    id: PromptId::new(),
                    text: text.to_owned(),
                    skill_invocations: Vec::new(),
                    attachments: Vec::new(),
                },
                delivery: PromptDelivery::Queue,
            },
        )
        .await
        .expect("the idle Session takes the next Prompt");
}

fn user_messages(snapshot: &SessionSnapshot) -> usize {
    snapshot
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::User)
        .count()
}

/// Whether an Error stands in the Turn at `turn`.
fn turn_has_error(snapshot: &SessionSnapshot, turn: usize) -> bool {
    snapshot.activities.iter().any(|activity| {
        matches!(
            activity,
            Activity::Error { turn_id, .. } if *turn_id == snapshot.turns[turn].id
        )
    })
}

#[tokio::test]
async fn a_requested_compaction_is_codexs_own_compact_in_the_turn_suru_opened_for_it() {
    // Codex may announce the turn it compacts in before it answers the request, or after.
    for announced_first in [false, true] {
        a_requested_compaction_codex_announces(announced_first).await;
    }
}

async fn a_requested_compaction_codex_announces(announced_first: bool) {
    let thread = "native-thread";
    let opening = if announced_first {
        [
            compaction_turn_started(thread),
            ACCEPT_COMPACTION.to_owned(),
        ]
        .concat()
    } else {
        [
            ACCEPT_COMPACTION.to_owned(),
            compaction_turn_started(thread),
        ]
        .concat()
    };
    let codex = compacting_on_request(
        &[
            opening,
            // Codex's summarising call, then its estimate of the context it rebuilt.
            token_usage(thread, COMPACTION_TURN, 300_000, 190_000),
            token_usage(thread, COMPACTION_TURN, 300_000, 30_000),
            gate(1),
            compaction_item("completed", thread, COMPACTION_TURN),
            compaction_aftermath(thread, COMPACTION_TURN),
            turn_completed(thread, COMPACTION_TURN),
        ]
        .concat(),
        "",
        false,
    );
    let opened = opened_session(&codex, "codex-compaction-requested", "Keep going").await;
    let before = settled_session(&opened.client, opened.session_id, 0).await;

    let compacting = compaction_requested(&opened).await;
    let compact = codex
        .requests()
        .into_iter()
        .find(|request| request["method"] == "thread/compact/start")
        .expect("Codex is asked to compact");
    assert_eq!(compact["params"], json!({ "threadId": thread }));
    assert_eq!(
        compacting.turns.len(),
        2,
        "Codex's own turn/started, announced first: {announced_first}, opens no Turn beside the \
         one the request began: {:?}",
        compacting.turns
    );
    let turn = &compacting.turns[1];
    assert!(
        turn.compaction_requested && !turn.is_continuation(),
        "the request's own Turn holds the compaction, not a Continuation: {turn:?}"
    );
    assert_eq!(turn.status, TurnStatus::Active);
    assert!(
        compacting.session.working_since.is_some(),
        "the Session is Working"
    );
    let [
        Activity::Compaction {
            turn_id,
            status,
            trigger,
            before_tokens,
            ..
        },
    ] = compactions(&compacting)[..]
    else {
        unreachable!()
    };
    assert_eq!(*turn_id, turn.id);
    assert_eq!(
        (*status, *trigger, *before_tokens),
        (
            ActivityStatus::Active,
            CompactionTrigger::Manual,
            Some(182_000)
        ),
        "the manual Compaction runs, begun from the Context Fill last read before it"
    );

    codex.release_turn(1);
    let settled = settled_session(&opened.client, opened.session_id, 1).await;
    assert_eq!(settled.turns.len(), 2, "{:?}", settled.turns);
    assert_eq!(settled.turns[1].status, TurnStatus::Completed);
    assert!(
        settled.turns[1].usage.is_some(),
        "what the summarising spent is the Turn's Usage"
    );
    assert_eq!(
        user_messages(&settled),
        user_messages(&before),
        "no user Message is drawn for a Suru command"
    );
    assert!(
        settled.session.working_since.is_none(),
        "the Session is idle again"
    );

    prompt(&opened, "Now the lexer").await;
    let answered = settled_session(&opened.client, opened.session_id, 2).await;
    assert_eq!(answered.turns.len(), 3, "{:?}", answered.turns);
    assert_eq!(answered.turns[2].status, TurnStatus::Completed);
    assert_eq!(
        compactions(&answered)
            .into_iter()
            .map(|compaction| match compaction {
                Activity::Compaction {
                    status,
                    trigger,
                    before_tokens,
                    after_tokens,
                    error,
                    ..
                } => (
                    *status,
                    *trigger,
                    *before_tokens,
                    *after_tokens,
                    error.clone()
                ),
                _ => unreachable!(),
            })
            .collect::<Vec<_>>(),
        [(
            ActivityStatus::Completed,
            CompactionTrigger::Manual,
            Some(182_000),
            Some(35_000),
            None
        )],
        "the one Compaction completes, measured after by the first reading once it settled"
    );
    assert!(
        compactions(&answered)
            .into_iter()
            .all(|compaction| matches!(
                compaction,
                Activity::Compaction {
                    summary: None,
                    summary_truncated: false,
                    ..
                }
            )),
        "Codex reports no summary of what it compacted"
    );
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn a_requested_compaction_codex_fails_settles_it_and_its_turn_failed_with_codexs_error() {
    let thread = "native-thread";
    let codex = compacting_on_request(
        &[
            ACCEPT_COMPACTION.to_owned(),
            compaction_turn_started(thread),
            turn_failed(
                thread,
                COMPACTION_TURN,
                "Context window exceeded while compacting",
            ),
        ]
        .concat(),
        "",
        false,
    );
    let opened = opened_session(&codex, "codex-compaction-requested-failed", "Keep going").await;
    settled_session(&opened.client, opened.session_id, 0).await;
    opened
        .client
        .compact_session(opened.session_id, CompactSessionRequest::default())
        .await
        .expect("the idle Session takes the request");
    let settled = settled_session(&opened.client, opened.session_id, 1).await;

    assert_eq!(settled.turns.len(), 2, "{:?}", settled.turns);
    assert_eq!(settled.turns[1].status, TurnStatus::Failed);
    let [
        Activity::Compaction {
            turn_id,
            status,
            trigger,
            error,
            ..
        },
    ] = compactions(&settled)[..]
    else {
        panic!("one Compaction is recorded: {:?}", settled.activities);
    };
    assert_eq!(*turn_id, settled.turns[1].id);
    assert_eq!(
        (*status, *trigger, error.as_deref()),
        (
            ActivityStatus::Failed,
            CompactionTrigger::Manual,
            Some("Context window exceeded while compacting")
        ),
        "the Compaction fails with Codex's account of why"
    );
    assert!(
        !turn_has_error(&settled, 1),
        "the Compaction already says why, so nothing stands beside it: {:?}",
        settled.activities
    );
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn interrupting_a_requested_compaction_is_codexs_turn_interrupt_and_leaves_it_promptable() {
    let thread = "native-thread";
    let codex = compacting_on_request(
        &[
            compaction_turn_started(thread),
            ACCEPT_COMPACTION.to_owned(),
        ]
        .concat(),
        &interrupt_arm(thread, COMPACTION_TURN),
        false,
    );
    let opened = opened_session(&codex, "codex-compaction-requested-stop", "Keep going").await;
    settled_session(&opened.client, opened.session_id, 0).await;
    compaction_requested(&opened).await;

    opened
        .client
        .interrupt_session(opened.session_id)
        .await
        .expect("Codex acknowledges the interrupt");
    let settled = settled_session(&opened.client, opened.session_id, 1).await;

    let interrupt = codex
        .requests()
        .into_iter()
        .find(|request| request["method"] == "turn/interrupt")
        .expect("Codex is asked to interrupt");
    assert_eq!(
        interrupt["params"],
        json!({ "threadId": thread, "turnId": COMPACTION_TURN }),
        "the interrupt is the ordinary one, of the native turn Codex compacts in"
    );
    assert_eq!(settled.turns[1].status, TurnStatus::Interrupted);
    let [Activity::Compaction { status, error, .. }] = compactions(&settled)[..] else {
        panic!("one Compaction is recorded: {:?}", settled.activities);
    };
    assert_eq!((*status, error), (ActivityStatus::Interrupted, &None));
    assert!(
        !turn_has_error(&settled, 1),
        "nothing stands beside a stop: {:?}",
        settled.activities
    );
    assert!(
        settled.session.working_since.is_none(),
        "the Session is idle"
    );

    prompt(&opened, "Now the lexer").await;
    let answered = settled_session(&opened.client, opened.session_id, 2).await;
    assert_eq!(answered.turns[2].status, TurnStatus::Completed);
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn instructions_codex_takes_none_of_are_refused_before_it_is_asked_and_bare_compact_works() {
    let thread = "native-thread";
    let codex = compacting_on_request(
        &[
            ACCEPT_COMPACTION.to_owned(),
            compaction_turn_started(thread),
            compaction_item("completed", thread, COMPACTION_TURN),
            turn_completed(thread, COMPACTION_TURN),
        ]
        .concat(),
        "",
        false,
    );
    let opened = opened_session(&codex, "codex-compaction-instructions", "Keep going").await;
    settled_session(&opened.client, opened.session_id, 0).await;

    // A client that sends instructions anyway, whatever Codex declares.
    let refused = opened
        .client
        .compact_session(
            opened.session_id,
            CompactSessionRequest {
                instructions: Some("Keep the parser notes".to_owned()),
            },
        )
        .await
        .expect_err("Codex takes no instructions, so the request carrying them is refused");
    assert_eq!(
        refused
            .downcast_ref::<SessionError>()
            .map(|error| error.code),
        Some(SessionErrorCode::CompactionInstructionsUnsupported),
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
    assert!(
        !codex
            .methods()
            .iter()
            .any(|method| method == "thread/compact/start"),
        "Codex is never asked, so the instructions are refused rather than dropped: {:?}",
        codex.methods()
    );

    compaction_requested(&opened).await;
    let settled = settled_session(&opened.client, opened.session_id, 1).await;
    assert_eq!(settled.turns[1].status, TurnStatus::Completed);
    let compact = codex
        .requests()
        .into_iter()
        .find(|request| request["method"] == "thread/compact/start")
        .expect("Codex is asked to compact");
    assert_eq!(compact["params"], json!({ "threadId": thread }));
    assert!(
        matches!(
            compactions(&settled)[..],
            [Activity::Compaction {
                status: ActivityStatus::Completed,
                trigger: CompactionTrigger::Manual,
                instructions: None,
                ..
            }]
        ),
        "a bare request still compacts, asking nothing of the summary: {:?}",
        settled.activities
    );
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn codex_is_never_asked_to_compact_while_its_session_works() {
    let codex = compacting_on_request("", "", true);
    let opened = opened_session(&codex, "codex-compaction-requested-busy", "Keep going").await;
    session_where(
        &opened.client,
        opened.session_id,
        "the first Turn answers and works on",
        |snapshot| {
            snapshot
                .messages
                .iter()
                .any(|message| message.role == MessageRole::Agent)
        },
    )
    .await;

    let refused = opened
        .client
        .compact_session(opened.session_id, CompactSessionRequest::default())
        .await
        .expect_err("a Working Session refuses the request");
    assert_eq!(
        refused
            .downcast_ref::<SessionError>()
            .map(|error| error.code),
        Some(SessionErrorCode::WorkingSession),
        "{refused:#}"
    );
    codex.release_turn(0);
    let settled = settled_session(&opened.client, opened.session_id, 0).await;
    assert_eq!(settled.turns.len(), 1, "{:?}", settled.turns);
    assert!(
        !codex
            .methods()
            .iter()
            .any(|method| method == "thread/compact/start"),
        "Codex, which would abort its running turn to compact, is never asked: {:?}",
        codex.methods()
    );
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn an_interrupt_before_codex_names_the_compactions_turn_stops_that_turn_once_it_does() {
    let thread = "native-thread";
    let codex = compacting_on_request(
        &[
            ACCEPT_COMPACTION.to_owned(),
            gate(1),
            compaction_turn_started(thread),
        ]
        .concat(),
        &interrupt_arm(thread, COMPACTION_TURN),
        false,
    );
    let opened = opened_session(&codex, "codex-compaction-early-stop", "Keep going").await;
    settled_session(&opened.client, opened.session_id, 0).await;
    opened
        .client
        .compact_session(opened.session_id, CompactSessionRequest::default())
        .await
        .expect("the idle Session takes the request");
    codex.wait_for_method("thread/compact/start").await;

    opened
        .client
        .interrupt_session(opened.session_id)
        .await
        .expect("the interrupt is taken before Codex names the turn it compacts in");
    assert!(
        !codex
            .methods()
            .iter()
            .any(|method| method == "turn/interrupt"),
        "there is no turn to address yet: {:?}",
        codex.methods()
    );

    codex.release_turn(1);
    let settled = settled_session(&opened.client, opened.session_id, 1).await;
    let interrupt = codex
        .requests()
        .into_iter()
        .find(|request| request["method"] == "turn/interrupt")
        .expect("the interrupt is sent once Codex names the turn");
    assert_eq!(
        interrupt["params"],
        json!({ "threadId": thread, "turnId": COMPACTION_TURN })
    );
    assert_eq!(settled.turns.len(), 2, "{:?}", settled.turns);
    assert_eq!(settled.turns[1].status, TurnStatus::Interrupted);
    let [Activity::Compaction { status, error, .. }] = compactions(&settled)[..] else {
        panic!("one Compaction is recorded: {:?}", settled.activities);
    };
    assert_eq!((*status, error), (ActivityStatus::Interrupted, &None));
    assert!(
        !turn_has_error(&settled, 1),
        "nothing stands beside a stop: {:?}",
        settled.activities
    );

    prompt(&opened, "Now the lexer").await;
    let answered = settled_session(&opened.client, opened.session_id, 2).await;
    assert_eq!(answered.turns[2].status, TurnStatus::Completed);
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

#[tokio::test]
async fn a_compaction_codex_never_begins_a_turn_for_gives_the_connection_up() {
    let thread = "native-thread";
    let codex = compacting_on_request(
        &[
            ACCEPT_COMPACTION.to_owned(),
            // Codex begins the turn only after Suru has stopped waiting for it, if at all.
            gate(1),
            compaction_turn_started(thread),
        ]
        .concat(),
        "",
        false,
    );
    let opened = opened_session_on(
        CodexRuntime::new(codex.executable())
            .with_compaction_start_timeout(Duration::from_millis(100)),
        "codex-compaction-never-begun",
        "Keep going",
    )
    .await;
    settled_session(&opened.client, opened.session_id, 0).await;
    opened
        .client
        .compact_session(opened.session_id, CompactSessionRequest::default())
        .await
        .expect("the idle Session takes the request");
    let failed = settled_session(&opened.client, opened.session_id, 1).await;

    assert_eq!(failed.turns[1].status, TurnStatus::Failed);
    assert!(
        failed.activities.iter().any(|activity| matches!(
            activity,
            Activity::Error { turn_id, text, .. }
                if *turn_id == failed.turns[1].id && text.contains("began no turn for it")
        )),
        "the Turn says why: {:?}",
        failed.activities
    );
    assert!(compactions(&failed).is_empty(), "{:?}", failed.activities);

    // A turn Codex begins late reaches no connection Suru still reads, so it is never taken for
    // a turn of its own; the next Prompt begins a Turn on a connection of its own.
    codex.release_turn(1);
    prompt(&opened, "Now the lexer").await;
    let answered = settled_session(&opened.client, opened.session_id, 2).await;
    assert_eq!(answered.turns.len(), 3, "{:?}", answered.turns);
    assert!(
        !answered.turns[2].is_continuation() && answered.turns[2].prompt_id.is_some(),
        "{:?}",
        answered.turns[2]
    );
    assert_eq!(answered.turns[2].status, TurnStatus::Completed);
    assert!(
        codex
            .methods()
            .iter()
            .filter(|method| *method == "initialize")
            .count()
            >= 2,
        "the connection the compaction was left running on was given up: {:?}",
        codex.methods()
    );
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}
