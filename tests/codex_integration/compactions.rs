//! Codex compacting a thread's context on its own, in the Transcript: the `contextCompaction` item
//! opens a Compaction at `item/started` in the Turn it fell in and completes it at
//! `item/completed`. Codex measures nothing on the item, so the Compaction's Context Fill before and
//! after is the Session's own, read from the thread's token usage either side of it. A compaction
//! that fails ends its native turn failed, with Codex's account of why, and the Compaction fails
//! with it; one an interrupt cuts off is stopped with its Turn. A child thread's compaction stands
//! in its Subagent's Session. The deprecated `thread/compacted` notification and the `warning`
//! Codex sends after compacting record nothing.
//!
//! The wire shapes and their order follow Codex's app-server: the token usage it reads during a
//! compaction — its summarising call's, then the rebuilt context's — arrives before the item
//! completes.

use crate::support::{
    ScriptedCodex, conversation_codex, conversation_codex_with_arms, opened_session, session_where,
    settled_session,
};
use serde_json::{Value, json};
use suru::protocol::{
    Activity, ActivityStatus, CompactionTrigger, MessageRole, SessionSnapshot, TurnStatus,
};

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
    assert_eq!(
        (*before_tokens, *after_tokens, error),
        (Some(182_000), None, &None),
        "it begins from the Context Fill last read before it"
    );
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
            before_tokens: Some(182_000),
            after_tokens: Some(35_000),
            error: None,
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
