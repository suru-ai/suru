//! Copilot compacting a conversation's context, in the Transcript: `session.compaction_start`
//! opens a Compaction and `session.compaction_complete` Settles it — completed with the
//! `preCompactionTokens` and `postCompactionTokens` Copilot measured, or failed with its `error` —
//! so a failed attempt and the retry after it stand as two. One attributed to a sub-agent is the
//! Subagent's Compaction and stands in its own Session. The `model.*` events Copilot emits while
//! it summarises are the compaction's own model call, and record nothing.
//!
//! Copilot compacts in the background of its Session rather than in a turn of its loop, so the
//! loop going idle while it still compacts holds the Turn open until the compaction Settles, and
//! a compaction Copilot begins while no Turn runs begins a Continuation its own completion
//! settles. Stopping either — by interrupt, or by the next Prompt — cancels the compaction.
//!
//! The wire shapes follow github-copilot-sdk 1.0.15-preview.3's `SessionCompactionStartData` and
//! `SessionCompactionCompleteData`.

use crate::support::{
    Opened, ScriptedCopilot, abort_arm, agent_messages, conversation_arms, conversation_fixture,
    opened_session, send_arm, session_where, settled_session,
};
use suru::managed_client::SessionSubscription;
use suru::protocol::{
    Activity, ActivityStatus, AdmitPromptRequest, CompactionTrigger, ContextFill, InitialPrompt,
    PromptDelivery, PromptId, SessionSnapshot, SessionStatus, TurnStatus,
};

/// Copilot starting to compact the main conversation once its context crossed the background
/// threshold.
const STARTED: &str = r#"      event c-start session.compaction_start '{"conversationTokens":150000,"currentTokens":182000,"systemTokens":12000,"toolDefinitionsTokens":20000,"tokenLimit":200000,"trigger":"threshold"}'
"#;

/// The compaction `STARTED` began completing, with the context it measured before and after.
const COMPLETED: &str = r#"      event c-complete session.compaction_complete '{"success":true,"trigger":"threshold","preCompactionTokens":182000,"postCompactionTokens":31000,"messagesRemoved":40,"summaryContent":"<overview>The parser work is half done.</overview>","compactionTokensUsed":{"inputTokens":150000,"outputTokens":2000,"model":"claude-fixture"}}'
"#;

/// The compaction `STARTED` began failing.
const FAILED: &str = r#"      event c-failed session.compaction_complete '{"success":false,"trigger":"threshold","error":"Compaction failed: the model returned an empty summary","statusCode":500}'
"#;

/// What Copilot answers once a background compaction Suru cancelled has stopped.
const CANCELLED: &str = r#"      event c-cancelled session.compaction_complete '{"success":false,"trigger":"threshold","error":"Compaction cancelled"}'
"#;

/// The model call Copilot makes to summarise, which is the compaction's and no Agent output.
const SUMMARISING: &str = r#"      event model-start model.call_start '{"turnId":"compaction-1","model":"claude-fixture"}'
      event model-failed model.call_failure '{"turnId":"compaction-1","model":"claude-fixture","statusCode":429,"errorMessage":"rate limited"}'
      event model-retry model.call_start '{"turnId":"compaction-1","model":"claude-fixture"}'
      event model-finished model.call_finished '{"turnId":"compaction-1","dispatchDurationMs":40000,"editClassifierVersion":1,"outcome":"success"}'
"#;

const ANSWER: &str = r#"      event answer assistant.message '{"messageId":"m-answer","content":"Carrying on."}'
"#;

const IDLE: &str = r#"      event idle session.idle '{}'
"#;

/// A context reading the timeline reports after the event a test must know was read: readings
/// keep their place in the timeline, so once the Session shows this one, everything before it
/// has been projected.
fn reading(id: &str, occupied: u64) -> String {
    format!(
        r#"      reply '{{"jsonrpc":"2.0","method":"session.event","params":{{"sessionId":"'"$sid"'","event":{{"id":"{id}","timestamp":"2026-01-01T00:00:00Z","ephemeral":true,"type":"session.usage_info","data":{{"currentTokens":{occupied},"tokenLimit":200000,"messagesLength":3}}}}}}}}'
"#
    )
}

fn fill(occupied_tokens: u64) -> Option<ContextFill> {
    Some(ContextFill {
        occupied_tokens,
        capacity_tokens: None,
    })
}

/// `timeline` played in the background once the test lets the fixture holding at `gate` carry on,
/// so the CLI keeps reading what Suru sends meanwhile.
fn after(gate: &str, timeline: &str) -> String {
    format!(
        "      (\n        while [ ! -e \"{gate}\" ]; do sleep 0.01; done\n{timeline}      ) &\n"
    )
}

/// A `session.history.cancelBackgroundCompaction` arm confirming the cancel and then playing
/// `timeline`.
fn cancel_compaction_arm(timeline: &str) -> String {
    format!(
        r#"    *'"method":"session.history.cancelBackgroundCompaction"'*)
      reply '{{"jsonrpc":"2.0","id":'"$id"',"result":{{"cancelled":true}}}}'
{timeline}      ;;
"#
    )
}

/// A `session.send` arm playing `first` for the Session's first Prompt and `later` for each after.
fn prompts_arm(first: &str, later: &str) -> String {
    format!(
        r#"    *'"method":"session.send"'*)
      sid=$(printf '%s' "$body" | sed -n 's/.*"sessionId":"\([^"]*\)".*/\1/p')
      reply '{{"jsonrpc":"2.0","id":'"$id"',"result":{{"messageId":"fixture-message"}}}}'
      prompts=$(( ${{prompts:-0}} + 1 ))
      if [ "$prompts" -eq 1 ]; then
{first}      else
{later}      fi
      ;;
"#
    )
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

async fn feed(opened: &Opened) -> SessionSubscription {
    opened
        .client
        .subscribe_session(opened.session_id)
        .await
        .expect("subscribe to Session SSE")
}

async fn shutdown(opened: Opened) {
    opened
        .server
        .shutdown()
        .await
        .expect("shut the server down");
}

/// How many requests the CLI received for `method`.
fn requested(copilot: &ScriptedCopilot, method: &str) -> usize {
    copilot
        .methods()
        .iter()
        .filter(|requested| *requested == method)
        .count()
}

#[tokio::test]
async fn an_automatic_compaction_mid_turn_goes_active_then_completes_with_copilots_counts() {
    let copilot = conversation_fixture(&format!(
        "{STARTED}{}",
        after(
            "$COPILOT_FIXTURE_RELEASE",
            &format!("{SUMMARISING}{COMPLETED}{ANSWER}{IDLE}")
        )
    ));
    let opened = opened_session(&copilot, "copilot-compaction", "Keep going on the parser").await;
    let mut feed = feed(&opened).await;

    let compacting = session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "Copilot starts compacting",
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

    copilot.release();
    let settled = settled_session(&opened.client, opened.session_id, 0).await;
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    assert_eq!(
        settled.activities,
        vec![Activity::Compaction {
            id,
            turn_id: settled.turns[0].id,
            status: ActivityStatus::Completed,
            trigger: CompactionTrigger::Automatic,
            before_tokens: Some(182_000),
            after_tokens: Some(31_000),
            error: None,
        }],
        "the Compaction completes with Copilot's counts, and its summarising model calls record \
         no other Activity"
    );
    assert_eq!(
        agent_messages(&settled)
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        ["Carrying on."],
        "the model calls Copilot summarised with are no Message of the Agent's"
    );
    shutdown(opened).await;
}

#[tokio::test]
async fn a_failed_compaction_settles_failed_with_copilots_error_and_leaves_the_turn_to_copilot() {
    let copilot = conversation_fixture(&format!("{STARTED}{FAILED}{ANSWER}{IDLE}"));
    let opened = opened_session(&copilot, "copilot-compaction-failed", "Keep going").await;
    let settled = settled_session(&opened.client, opened.session_id, 0).await;

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
        Some("Compaction failed: the model returned an empty summary")
    );
    assert_eq!((*before_tokens, *after_tokens), (None, None));
    assert_eq!(
        settled.turns[0].status,
        TurnStatus::Completed,
        "the Turn Settles as Copilot's loop says"
    );
    shutdown(opened).await;
}

#[tokio::test]
async fn a_failed_attempt_then_a_successful_retry_leaves_two_compactions() {
    let copilot = conversation_fixture(&format!(
        "{STARTED}{FAILED}{}{COMPLETED}{ANSWER}{IDLE}",
        STARTED.replace("c-start", "c-retry")
    ));
    let opened = opened_session(&copilot, "copilot-compaction-retry", "Keep going").await;
    let settled = settled_session(&opened.client, opened.session_id, 0).await;

    let [
        Activity::Compaction {
            turn_id: failed_turn,
            status: ActivityStatus::Failed,
            error: Some(_),
            ..
        },
        Activity::Compaction {
            turn_id: retried_turn,
            status: ActivityStatus::Completed,
            before_tokens: Some(182_000),
            after_tokens: Some(31_000),
            ..
        },
    ] = compactions(&settled)[..]
    else {
        panic!(
            "the failed attempt and its retry are two Compactions: {:?}",
            settled.activities
        );
    };
    assert_eq!(*failed_turn, settled.turns[0].id);
    assert_eq!(*retried_turn, settled.turns[0].id);
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    shutdown(opened).await;
}

#[tokio::test]
async fn an_after_count_larger_than_the_before_count_is_recorded_as_reported() {
    let copilot = conversation_fixture(&format!(
        r#"{STARTED}      event c-grew session.compaction_complete '{{"success":true,"preCompactionTokens":20000,"postCompactionTokens":24000}}'
{ANSWER}{IDLE}"#
    ));
    let opened = opened_session(&copilot, "copilot-compaction-grew", "Keep going").await;
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
        (Some(20_000), Some(24_000)),
        "a short history's summary can outgrow it, and is recorded as Copilot measured it"
    );
    shutdown(opened).await;
}

/// A spawn whose sub-agent's conversation compacts before it completes, every event stamped with
/// the sub-agent's envelope attribution.
const SUBAGENT_COMPACTS_TURN: &str = r#"      agent_event s1 agent-1 subagent.started '{"toolCallId":"t-spawn","agentName":"researcher","agentDisplayName":"Researcher","agentDescription":"Scout the workspace"}'
      agent_event s2 agent-1 session.compaction_start '{"currentTokens":90000,"trigger":"threshold"}'
      agent_event s3 agent-1 model.call_start '{"turnId":"compaction-child","model":"claude-fixture"}'
      agent_event s4 agent-1 session.compaction_complete '{"success":true,"preCompactionTokens":90000,"postCompactionTokens":12000}'
      agent_event s5 agent-1 assistant.message '{"messageId":"sub-m1","content":"Found one file."}'
      agent_event s6 agent-1 subagent.completed '{"toolCallId":"t-spawn","agentName":"researcher","agentDisplayName":"Researcher"}'
      event e1 assistant.message '{"messageId":"m1","content":"One file."}'
      event e2 session.idle '{}'
"#;

#[tokio::test]
async fn a_subagents_compaction_stands_in_its_own_session_and_never_its_parents() {
    let copilot = conversation_fixture(SUBAGENT_COMPACTS_TURN);
    let opened = opened_session(&copilot, "copilot-compaction-child", "Scout").await;
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
    assert_eq!(
        agent_messages(&child)
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        ["Found one file."],
        "the Subagent's summarising model call is no Message of its own"
    );
    shutdown(opened).await;
}

#[tokio::test]
async fn copilots_loop_going_idle_mid_compaction_holds_the_turn_until_the_compaction_settles() {
    let copilot = conversation_fixture(&format!(
        "{STARTED}{ANSWER}{IDLE}{}{}",
        reading("held", 182_000),
        after(
            "$COPILOT_FIXTURE_RELEASE",
            &format!("{COMPLETED}{}", reading("compacted", 31_000))
        )
    ));
    let opened = opened_session(&copilot, "copilot-compaction-held", "Keep going").await;
    let mut feed = feed(&opened).await;

    let held = session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "Copilot's loop goes idle while it compacts",
        |snapshot| snapshot.session.context_fill == fill(182_000),
    )
    .await;
    assert_eq!(
        held.turns[0].status,
        TurnStatus::Active,
        "Copilot is still compacting the Session's context, so the Turn is not over"
    );
    assert_eq!(held.session.status, SessionStatus::Active);
    assert_eq!(compaction_statuses(&held), [ActivityStatus::Active]);

    copilot.release();
    let settled = settled_session(&opened.client, opened.session_id, 0).await;
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    assert_eq!(compaction_statuses(&settled), [ActivityStatus::Completed]);
    assert_eq!(settled.session.status, SessionStatus::Idle);
    shutdown(opened).await;
}

#[tokio::test]
async fn a_compaction_copilot_begins_between_turns_runs_in_a_continuation_it_settles_itself() {
    let copilot = conversation_fixture(&format!(
        "{ANSWER}{IDLE}{}",
        after(
            "$COPILOT_FIXTURE_RELEASE",
            &format!(
                "{STARTED}{SUMMARISING}{}",
                after("$COPILOT_FIXTURE_RELEASE-done", COMPLETED)
            )
        )
    ));
    let opened = opened_session(&copilot, "copilot-compaction-between", "Keep going").await;
    let mut feed = feed(&opened).await;
    let first = settled_session(&opened.client, opened.session_id, 0).await;
    assert!(compactions(&first).is_empty());

    copilot.release();
    let compacting = session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "Copilot compacts after the Turn settled",
        |snapshot| compaction_statuses(snapshot) == [ActivityStatus::Active],
    )
    .await;
    assert!(
        compacting.turns[1].is_continuation(),
        "the compaction began a Continuation: {:?}",
        compacting.turns
    );
    assert_eq!(compacting.session.status, SessionStatus::Active);

    copilot.release_gate("done");
    let settled = settled_session(&opened.client, opened.session_id, 1).await;
    assert_eq!(
        settled.turns[1].status,
        TurnStatus::Completed,
        "the compaction completing settles the Continuation, since no loop ran to go idle"
    );
    assert_eq!(settled.session.status, SessionStatus::Idle);
    let [
        Activity::Compaction {
            turn_id,
            status,
            before_tokens,
            after_tokens,
            ..
        },
    ] = compactions(&settled)[..]
    else {
        panic!("one Compaction is recorded: {:?}", settled.activities);
    };
    assert_eq!(*turn_id, settled.turns[1].id);
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(
        (*before_tokens, *after_tokens),
        (Some(182_000), Some(31_000))
    );
    assert_eq!(
        settled.activities.len(),
        1,
        "the summarising model calls record nothing: {:?}",
        settled.activities
    );
    shutdown(opened).await;
}

#[tokio::test]
async fn interrupting_a_turn_only_a_compaction_holds_cancels_the_compaction_and_stops_the_turn() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}{}",
        conversation_arms(),
        send_arm(&format!(
            "{STARTED}{ANSWER}{IDLE}{}",
            reading("held", 182_000)
        )),
        cancel_compaction_arm(&format!("{CANCELLED}{}", reading("cancelled", 182_500))),
        abort_arm(
            r#"      event aborted session.idle '{"aborted":true}'
"#
        ),
    ));
    let opened = opened_session(&copilot, "copilot-compaction-held-interrupt", "Keep going").await;
    let mut feed = feed(&opened).await;
    session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "Copilot's loop goes idle while it compacts",
        |snapshot| snapshot.session.context_fill == fill(182_000),
    )
    .await;

    opened
        .client
        .interrupt_session(opened.session_id)
        .await
        .expect("Copilot cancels the compaction");
    let stopped = session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "the cancelled compaction's own report has been read",
        |snapshot| snapshot.session.context_fill == fill(182_500),
    )
    .await;

    assert_eq!(stopped.turns.len(), 1, "{:?}", stopped.turns);
    assert_eq!(stopped.turns[0].status, TurnStatus::Interrupted);
    assert_eq!(stopped.session.status, SessionStatus::Idle);
    let [Activity::Compaction { status, error, .. }] = compactions(&stopped)[..] else {
        panic!(
            "the cancellation Copilot reports records no second Compaction: {:?}",
            stopped.activities
        );
    };
    assert_eq!(*status, ActivityStatus::Interrupted);
    assert_eq!(*error, None);
    assert_eq!(
        requested(&copilot, "session.history.cancelBackgroundCompaction"),
        1
    );
    assert_eq!(
        requested(&copilot, "session.abort"),
        0,
        "Copilot's loop already stopped, so there is no loop to abort"
    );
    shutdown(opened).await;
}

#[tokio::test]
async fn interrupting_a_working_loop_while_copilot_compacts_cancels_both_without_waiting_on_it() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}{}",
        conversation_arms(),
        send_arm(&format!(
            r#"{STARTED}      event working assistant.message_start '{{"messageId":"m-working"}}'
"#
        )),
        cancel_compaction_arm(""),
        abort_arm(&format!(
            r#"{CANCELLED}      event aborted session.idle '{{"aborted":true}}'
"#
        )),
    ));
    let opened = opened_session(&copilot, "copilot-compaction-loop-interrupt", "Keep going").await;
    let mut feed = feed(&opened).await;
    session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "Copilot compacts while its loop works",
        |snapshot| {
            compaction_statuses(snapshot) == [ActivityStatus::Active]
                && !agent_messages(snapshot).is_empty()
        },
    )
    .await;

    opened
        .client
        .interrupt_session(opened.session_id)
        .await
        .expect("Copilot acknowledges the interrupt");
    let stopped = settled_session(&opened.client, opened.session_id, 0).await;

    assert_eq!(
        stopped.turns[0].status,
        TurnStatus::Interrupted,
        "the aborted idle settles the Turn without waiting on the cancelled compaction"
    );
    let [Activity::Compaction { status, error, .. }] = compactions(&stopped)[..] else {
        panic!("one Compaction is recorded: {:?}", stopped.activities);
    };
    assert_eq!(*status, ActivityStatus::Interrupted);
    assert_eq!(*error, None);
    assert_eq!(
        requested(&copilot, "session.history.cancelBackgroundCompaction"),
        1
    );
    assert_eq!(requested(&copilot, "session.abort"), 1);
    shutdown(opened).await;
}

#[tokio::test]
async fn a_prompt_delivered_while_copilot_compacts_between_turns_cancels_the_compaction_first() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}",
        conversation_arms(),
        prompts_arm(
            &format!(
                "{ANSWER}{IDLE}{}",
                after("$COPILOT_FIXTURE_RELEASE", STARTED)
            ),
            r#"      event next assistant.message '{"messageId":"m-next","content":"On to the lexer."}'
      event next-idle session.idle '{}'
"#,
        ),
        cancel_compaction_arm(CANCELLED),
    ));
    let opened = opened_session(&copilot, "copilot-compaction-between-prompt", "Keep going").await;
    let mut feed = feed(&opened).await;
    settled_session(&opened.client, opened.session_id, 0).await;
    copilot.release();
    session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "Copilot compacts after the Turn settled",
        |snapshot| compaction_statuses(snapshot) == [ActivityStatus::Active],
    )
    .await;

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

    let methods = copilot.methods();
    let cancelled = methods
        .iter()
        .position(|method| method == "session.history.cancelBackgroundCompaction")
        .unwrap_or_else(|| panic!("Suru cancels the compaction: {methods:?}"));
    let sent = methods
        .iter()
        .enumerate()
        .filter(|(_, method)| *method == "session.send")
        .nth(1)
        .map(|(position, _)| position)
        .unwrap_or_else(|| panic!("the next Prompt reaches Copilot: {methods:?}"));
    assert!(
        cancelled < sent,
        "Copilot's compaction is stopped before the Prompt reaches it: {methods:?}"
    );
    assert!(answered.turns[1].is_continuation());
    assert_eq!(answered.turns[1].status, TurnStatus::Interrupted);
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
    shutdown(opened).await;
}

#[tokio::test]
async fn interrupting_late_output_while_copilot_compacts_in_it_cancels_the_compaction_too() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}{}",
        conversation_arms(),
        send_arm(&format!(
            r#"      agent_event s1 agent-1 subagent.started '{{"toolCallId":"t-spawn","agentName":"researcher","agentDisplayName":"Researcher","agentDescription":"Scout the workspace"}}'
{IDLE}{}"#,
            after(
                "$COPILOT_FIXTURE_RELEASE",
                &format!(
                    r#"        agent_event s2 agent-1 subagent.completed '{{"toolCallId":"t-spawn","agentName":"researcher","agentDisplayName":"Researcher"}}'
        event late assistant.message_start '{{"messageId":"m-late"}}'
{STARTED}"#
                )
            )
        )),
        cancel_compaction_arm(""),
        abort_arm(&format!(
            r#"{CANCELLED}      event aborted session.idle '{{"aborted":true}}'
{}"#,
            reading("stopped", 182_500)
        )),
    ));
    let opened = opened_session(&copilot, "copilot-compaction-late-interrupt", "Scout").await;
    let mut feed = feed(&opened).await;
    settled_session(&opened.client, opened.session_id, 0).await;
    copilot.release();
    let compacting = session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "Copilot compacts in the Continuation the Subagent's settle provoked",
        |snapshot| compaction_statuses(snapshot) == [ActivityStatus::Active],
    )
    .await;
    assert!(compacting.turns[1].is_continuation());

    opened
        .client
        .interrupt_session(opened.session_id)
        .await
        .expect("Copilot acknowledges the interrupt");
    let stopped = session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "the aborted loop's idle has been read",
        |snapshot| snapshot.session.context_fill == fill(182_500),
    )
    .await;

    assert_eq!(stopped.turns.len(), 2, "{:?}", stopped.turns);
    assert_eq!(stopped.turns[1].status, TurnStatus::Interrupted);
    let [Activity::Compaction { status, .. }] = compactions(&stopped)[..] else {
        panic!(
            "the cancelled compaction's end records no second Compaction: {:?}",
            stopped.activities
        );
    };
    assert_eq!(*status, ActivityStatus::Interrupted);
    assert_eq!(
        requested(&copilot, "session.history.cancelBackgroundCompaction"),
        1
    );
    assert_eq!(requested(&copilot, "session.abort"), 1);
    shutdown(opened).await;
}
