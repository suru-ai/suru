//! Copilot compacting a conversation's context, in the Transcript: `session.compaction_start`
//! opens a Compaction and `session.compaction_complete` Settles it — completed with the
//! `preCompactionTokens` and `postCompactionTokens` Copilot measured and the `summaryContent` it
//! left, or failed with its `error` — so a failed attempt and the retry after it stand as two. One attributed to a sub-agent is the
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
    opened_session, opened_session_on, permission_decision_arm, send_arm, session_where,
    settled_session,
};
use suru::managed_client::SessionSubscription;
use suru::protocol::{
    Activity, ActivityStatus, AdmitPromptRequest, CompactionTrigger, ContextFill, Decision,
    InitialPrompt, PromptDelivery, PromptId, SessionSnapshot, SessionStatus, TurnStatus,
};
use suru::provider::CopilotRuntime;
use tokio::time::Duration;

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

/// Delivers `text` as the next Prompt — steered into the Turn the Session is working on, or
/// queued behind it, as `delivery` says.
async fn deliver(opened: &Opened, text: &str, delivery: PromptDelivery) {
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
                delivery,
            },
        )
        .await
        .expect("deliver the next Prompt");
}

/// Where in the CLI's requests the first `method` stands, and where the `nth` `session.send`
/// does.
fn before_send(copilot: &ScriptedCopilot, method: &str, nth: usize) -> (usize, usize) {
    let methods = copilot.methods();
    let position = methods
        .iter()
        .position(|requested| requested == method)
        .unwrap_or_else(|| panic!("the CLI receives {method}: {methods:?}"));
    let sent = methods
        .iter()
        .enumerate()
        .filter(|(_, requested)| *requested == "session.send")
        .nth(nth)
        .map(|(position, _)| position)
        .unwrap_or_else(|| panic!("the CLI receives session.send {nth}: {methods:?}"));
    (position, sent)
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
    assert_eq!((*before_tokens, *after_tokens, error), (None, None, &None));
    assert_eq!(
        (summary, *summary_truncated),
        (&None, false),
        "nothing is known yet of the summary it will leave"
    );
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
            summary: Some("<overview>The parser work is half done.</overview>".to_owned()),
            summary_truncated: false,
        }],
        "the Compaction completes with Copilot's counts and the `summaryContent` it left, and its \
         summarising model calls record no other Activity"
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
            summary: None,
            ..
        },
        Activity::Compaction {
            turn_id: retried_turn,
            status: ActivityStatus::Completed,
            before_tokens: Some(182_000),
            after_tokens: Some(31_000),
            summary: Some(_),
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

    deliver(&opened, "Now the lexer", PromptDelivery::Steer).await;
    let answered = settled_session(&opened.client, opened.session_id, 2).await;

    let (cancelled, sent) = before_send(&copilot, "session.history.cancelBackgroundCompaction", 1);
    assert!(
        cancelled < sent,
        "Copilot's compaction is stopped before the Prompt reaches it"
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

#[tokio::test]
async fn a_steer_into_a_turn_a_compaction_holds_keeps_the_answer_the_woken_loop_gives_it() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}",
        conversation_arms(),
        prompts_arm(
            &format!("{STARTED}{ANSWER}{IDLE}{}", reading("held", 182_000)),
            // The compaction ends before the loop the steer woke says anything.
            &format!(
                r#"{COMPLETED}      event steered assistant.message '{{"messageId":"m-steered","content":"Steered answer."}}'
      event steered-idle session.idle '{{}}'
"#
            ),
        ),
    ));
    let opened = opened_session(&copilot, "copilot-compaction-held-steer", "Keep going").await;
    let mut feed = feed(&opened).await;
    session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "Copilot's loop goes idle while it compacts",
        |snapshot| snapshot.session.context_fill == fill(182_000),
    )
    .await;

    deliver(&opened, "Also check the lexer", PromptDelivery::Steer).await;
    let settled = settled_session(&opened.client, opened.session_id, 0).await;

    assert_eq!(settled.turns.len(), 1, "{:?}", settled.turns);
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    assert_eq!(
        agent_messages(&settled)
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        ["Carrying on.", "Steered answer."],
        "the loop the steer woke answers in the Turn it steered, which its idle settles"
    );
    assert_eq!(compaction_statuses(&settled), [ActivityStatus::Completed]);
    shutdown(opened).await;
}

#[tokio::test]
async fn declining_an_approval_and_interrupting_while_copilot_compacts_cancels_the_compaction() {
    let arms = conversation_arms().replace(
        &permission_decision_arm(),
        r#"    *'"method":"session.permissions.handlePendingPermissionRequest"'*)
      reply '{"jsonrpc":"2.0","id":'"$id"',"result":{"success":true}}'
      ;;
"#,
    );
    let copilot = ScriptedCopilot::new(&format!(
        "{arms}{}{}{}",
        send_arm(&format!(
            r#"{STARTED}      event p permission.requested '{{"requestId":"stop","permissionRequest":{{"kind":"shell","toolCallId":"t-stop","fullCommandText":"danger","intention":"Stop this"}}}}'
"#
        )),
        cancel_compaction_arm(CANCELLED),
        // The abort stops the loop and leaves the compaction running.
        abort_arm(
            r#"      event aborted session.idle '{"aborted":true}'
"#
        ),
    ));
    let opened = opened_session(&copilot, "copilot-compaction-decline", "Try then stop").await;
    let mut feed = feed(&opened).await;
    let pending = session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "an Approval is pending while Copilot compacts",
        |snapshot| {
            snapshot.pending_approvals.len() == 1
                && compaction_statuses(snapshot) == [ActivityStatus::Active]
        },
    )
    .await;

    opened
        .client
        .submit_decision(
            opened.session_id,
            pending.pending_approvals[0],
            Decision::DeclineAndInterrupt,
        )
        .await
        .expect("Copilot takes the Decision and the interrupt");
    let stopped = settled_session(&opened.client, opened.session_id, 0).await;

    assert_eq!(stopped.turns[0].status, TurnStatus::Interrupted);
    assert_eq!(stopped.session.status, SessionStatus::Idle);
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
async fn a_subagent_compaction_its_stretch_outlived_records_nothing_in_the_stretch_resuming_it() {
    let copilot = conversation_fixture(
        r#"      agent_event s1 agent-1 subagent.started '{"toolCallId":"t-spawn","agentName":"researcher","agentDisplayName":"Researcher","agentDescription":"Scout the workspace"}'
      agent_event s2 agent-1 session.compaction_start '{"currentTokens":90000,"trigger":"threshold"}'
      agent_event s3 agent-1 subagent.completed '{"toolCallId":"t-spawn","agentName":"researcher","agentDisplayName":"Researcher"}'
      agent_event s4 agent-1 user.message '{"content":"Run it again.","delivery":"idle"}'
      agent_event s5 agent-1 session.compaction_complete '{"success":true,"preCompactionTokens":90000,"postCompactionTokens":12000}'
      agent_event s6 agent-1 assistant.message '{"messageId":"r1","content":"Ran it again.","toolRequests":[]}'
      agent_event s7 agent-1 assistant.turn_end '{"turnId":"1"}'
      event e1 assistant.message '{"messageId":"m1","content":"Done."}'
      event e2 session.idle '{}'
"#,
    );
    let opened = opened_session(&copilot, "copilot-compaction-child-resume", "Scout").await;
    let parent = settled_session(&opened.client, opened.session_id, 0).await;
    let child_id = parent
        .activities
        .iter()
        .find_map(|activity| match activity {
            Activity::Subagent { session_id, .. } => Some(*session_id),
            _ => None,
        })
        .expect("the spawn opens a child Session");
    let child = settled_session(&opened.client, child_id, 1).await;

    assert_eq!(
        child.turns.len(),
        2,
        "the Subagent resumed: {:?}",
        child.turns
    );
    let [
        Activity::Compaction {
            turn_id, status, ..
        },
    ] = compactions(&child)[..]
    else {
        panic!(
            "the compaction's end belongs to the stretch it started in, not the resume: {:?}",
            child.activities
        );
    };
    assert_eq!(*turn_id, child.turns[0].id);
    assert_eq!(
        *status,
        ActivityStatus::Failed,
        "the Compaction settled with the stretch it stood in"
    );
    shutdown(opened).await;
}

#[tokio::test]
async fn a_prompt_delivered_while_copilot_compacts_in_late_output_cancels_the_compaction_first() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}{}",
        conversation_arms(),
        prompts_arm(
            &format!(
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
            ),
            r#"      event next assistant.message '{"messageId":"m-next","content":"On to the lexer."}'
      event next-idle session.idle '{}'
"#,
        ),
        cancel_compaction_arm(CANCELLED),
        abort_arm(
            r#"      event aborted session.idle '{"aborted":true}'
"#
        ),
    ));
    let opened = opened_session(&copilot, "copilot-compaction-late-prompt", "Scout").await;
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

    deliver(&opened, "Now the lexer", PromptDelivery::Steer).await;
    let answered = settled_session(&opened.client, opened.session_id, 2).await;

    let (cancelled, sent) = before_send(&copilot, "session.history.cancelBackgroundCompaction", 1);
    assert!(
        cancelled < sent,
        "Copilot's compaction is stopped before the Prompt reaches it"
    );
    let (aborted, sent) = before_send(&copilot, "session.abort", 1);
    assert!(aborted < sent, "and so is the loop the late output runs in");
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
    assert_eq!(
        agent_messages(&answered)
            .last()
            .map(|message| message.content.as_str()),
        Some("On to the lexer.")
    );
    shutdown(opened).await;
}

/// The arms every Copilot conversation needs, with the CLI accepting whatever Decision Suru answers
/// a permission request with.
fn deciding_arms() -> String {
    conversation_arms().replace(
        &permission_decision_arm(),
        r#"    *'"method":"session.permissions.handlePendingPermissionRequest"'*)
      reply '{"jsonrpc":"2.0","id":'"$id"',"result":{"success":true}}'
      ;;
"#,
    )
}

/// A Turn in which Copilot compacts and asks permission to run a command.
const COMPACTING_AND_ASKING: &str = r#"      event c-start session.compaction_start '{"currentTokens":182000,"trigger":"threshold"}'
      event p permission.requested '{"requestId":"stop","permissionRequest":{"kind":"shell","toolCallId":"t-stop","fullCommandText":"danger","intention":"Stop this"}}'
"#;

/// Waits for the Approval `COMPACTING_AND_ASKING` asks for, while Copilot compacts.
async fn asking_while_compacting(
    opened: &Opened,
    feed: &mut SessionSubscription,
) -> suru::protocol::ApprovalId {
    session_where(
        &opened.client,
        feed,
        opened.session_id,
        "an Approval is pending while Copilot compacts",
        |snapshot| {
            snapshot.pending_approvals.len() == 1
                && compaction_statuses(snapshot) == [ActivityStatus::Active]
        },
    )
    .await
    .pending_approvals[0]
}

#[tokio::test]
async fn an_approval_interrupt_whose_cancel_is_slow_never_reaches_the_turn_after_it() {
    // Copilot's loop finishes on its own while the cancel is still on its way.
    let slow_cancel = format!(
        r#"    *'"method":"session.history.cancelBackgroundCompaction"'*)
      (
        event own-idle session.idle '{{}}'
{}        while [ ! -e "$COPILOT_FIXTURE_RELEASE" ]; do sleep 0.01; done
        reply '{{"jsonrpc":"2.0","id":'"$id"',"result":{{"cancelled":true}}}}'
{CANCELLED}      ) &
      ;;
"#,
        reading("own-idle", 150_000)
    );
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{slow_cancel}{}",
        deciding_arms(),
        prompts_arm(
            COMPACTING_AND_ASKING,
            r#"      event queued assistant.message '{"messageId":"m-queued","content":"Queued answer."}'
      event queued-idle session.idle '{}'
"#,
        ),
        abort_arm(
            r#"      event aborted session.idle '{"aborted":true}'
"#
        ),
    ));
    let opened = opened_session(&copilot, "copilot-compaction-slow-cancel", "Try then stop").await;
    let mut feed = feed(&opened).await;
    let approval = asking_while_compacting(&opened, &mut feed).await;
    deliver(&opened, "Then the lexer", PromptDelivery::Queue).await;

    let (decided, ()) = tokio::join!(
        opened
            .client
            .submit_decision(opened.session_id, approval, Decision::DeclineAndInterrupt,),
        async {
            session_where(
                &opened.client,
                &mut feed,
                opened.session_id,
                "Copilot's loop goes idle while the cancel is on its way",
                |snapshot| snapshot.session.context_fill == fill(150_000),
            )
            .await;
            copilot.release();
        }
    );
    decided.expect("Copilot takes the Decision and the interrupt");
    let answered = settled_session(&opened.client, opened.session_id, 1).await;

    assert_eq!(
        requested(&copilot, "session.abort"),
        0,
        "the loop the interrupt was for had stopped, and the next Turn's is no interrupt's to stop"
    );
    assert_eq!(
        answered.turns[0].status,
        TurnStatus::Interrupted,
        "the compaction held the Turn until the interrupt cancelled it"
    );
    assert_eq!(
        compaction_statuses(&answered),
        [ActivityStatus::Interrupted]
    );
    assert_eq!(answered.turns[1].status, TurnStatus::Completed);
    assert_eq!(
        agent_messages(&answered)
            .last()
            .map(|message| message.content.as_str()),
        Some("Queued answer.")
    );
    shutdown(opened).await;
}

/// A `session.history.cancelBackgroundCompaction` arm failing the first cancel and playing
/// `after_failure`, then confirming every later one and playing `after_success`.
fn failing_cancel_arm(after_failure: &str, after_success: &str) -> String {
    format!(
        r#"    *'"method":"session.history.cancelBackgroundCompaction"'*)
      cancels=$(( ${{cancels:-0}} + 1 ))
      if [ "$cancels" -eq 1 ]; then
        reply '{{"jsonrpc":"2.0","id":'"$id"',"error":{{"code":-32603,"message":"compaction processor busy"}}}}'
{after_failure}      else
        reply '{{"jsonrpc":"2.0","id":'"$id"',"result":{{"cancelled":true}}}}'
{after_success}      fi
      ;;
"#
    )
}

#[tokio::test]
async fn a_compaction_copilot_failed_to_cancel_is_still_followed_to_its_completion() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}{}",
        deciding_arms(),
        send_arm(COMPACTING_AND_ASKING),
        failing_cancel_arm(
            &after(
                "$COPILOT_FIXTURE_RELEASE",
                &format!("{COMPLETED}{ANSWER}{IDLE}")
            ),
            ""
        ),
        abort_arm(
            r#"      event aborted session.idle '{"aborted":true}'
"#
        ),
    ));
    let opened = opened_session(
        &copilot,
        "copilot-compaction-cancel-failed",
        "Try then stop",
    )
    .await;
    let mut feed = feed(&opened).await;
    let approval = asking_while_compacting(&opened, &mut feed).await;

    opened
        .client
        .submit_decision(opened.session_id, approval, Decision::DeclineAndInterrupt)
        .await
        .expect_err("the interrupt Copilot could not carry out is reported");
    let still = opened
        .client
        .read_session(opened.session_id)
        .await
        .expect("read the Session");
    assert_eq!(still.turns[0].status, TurnStatus::Active);
    assert_eq!(compaction_statuses(&still), [ActivityStatus::Active]);
    assert_eq!(
        requested(&copilot, "session.abort"),
        0,
        "an interrupt that could not stop the compaction stops nothing"
    );

    copilot.release();
    let settled = settled_session(&opened.client, opened.session_id, 0).await;
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
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
    assert_eq!(
        *status,
        ActivityStatus::Completed,
        "the compaction Suru failed to cancel is still followed"
    );
    assert_eq!(
        (*before_tokens, *after_tokens),
        (Some(182_000), Some(31_000))
    );
    shutdown(opened).await;
}

#[tokio::test]
async fn a_cancel_copilot_failed_is_tried_again_by_the_next_interrupt() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}{}",
        deciding_arms(),
        send_arm(COMPACTING_AND_ASKING),
        failing_cancel_arm("", CANCELLED),
        abort_arm(
            r#"      event aborted session.idle '{"aborted":true}'
"#
        ),
    ));
    let opened = opened_session(&copilot, "copilot-compaction-cancel-retry", "Try then stop").await;
    let mut feed = feed(&opened).await;
    let approval = asking_while_compacting(&opened, &mut feed).await;
    opened
        .client
        .submit_decision(opened.session_id, approval, Decision::DeclineAndInterrupt)
        .await
        .expect_err("the interrupt Copilot could not carry out is reported");

    opened
        .client
        .interrupt_session(opened.session_id)
        .await
        .expect("Copilot cancels the compaction this time");
    let stopped = settled_session(&opened.client, opened.session_id, 0).await;

    assert_eq!(stopped.turns[0].status, TurnStatus::Interrupted);
    let [Activity::Compaction { status, error, .. }] = compactions(&stopped)[..] else {
        panic!("one Compaction is recorded: {:?}", stopped.activities);
    };
    assert_eq!(*status, ActivityStatus::Interrupted);
    assert_eq!(*error, None);
    assert_eq!(
        requested(&copilot, "session.history.cancelBackgroundCompaction"),
        2,
        "the compaction Copilot failed to cancel is still running, so the next interrupt cancels it"
    );
    assert_eq!(requested(&copilot, "session.abort"), 1);
    shutdown(opened).await;
}

#[tokio::test]
async fn a_compaction_copilot_begins_for_a_settled_subagent_runs_in_a_continuation_of_its_own() {
    let copilot = conversation_fixture(&format!(
        r#"      agent_event s1 agent-1 subagent.started '{{"toolCallId":"t-spawn","agentName":"researcher","agentDisplayName":"Researcher","agentDescription":"Scout the workspace"}}'
      agent_event s2 agent-1 assistant.message '{{"messageId":"sub-m1","content":"Found one file."}}'
      agent_event s3 agent-1 subagent.completed '{{"toolCallId":"t-spawn","agentName":"researcher","agentDisplayName":"Researcher"}}'
      event e1 assistant.message '{{"messageId":"m1","content":"One file."}}'
{IDLE}{}"#,
        after(
            "$COPILOT_FIXTURE_RELEASE",
            &format!(
                r#"        agent_event s4 agent-1 session.compaction_start '{{"currentTokens":90000,"trigger":"memory_pressure"}}'
{}"#,
                after(
                    "$COPILOT_FIXTURE_RELEASE-done",
                    r#"          agent_event s5 agent-1 session.compaction_complete '{"success":true,"preCompactionTokens":90000,"postCompactionTokens":12000}'
"#
                )
            )
        )
    ));
    let opened = opened_session(&copilot, "copilot-compaction-settled-child", "Scout").await;
    let parent = settled_session(&opened.client, opened.session_id, 0).await;
    let child_id = parent
        .activities
        .iter()
        .find_map(|activity| match activity {
            Activity::Subagent { session_id, .. } => Some(*session_id),
            _ => None,
        })
        .expect("the spawn opens a child Session");
    let mut child_feed = opened
        .client
        .subscribe_session(child_id)
        .await
        .expect("subscribe to the child Session");
    settled_session(&opened.client, child_id, 0).await;

    copilot.release();
    let compacting = session_where(
        &opened.client,
        &mut child_feed,
        child_id,
        "Copilot compacts the settled Subagent's context",
        |snapshot| compaction_statuses(snapshot) == [ActivityStatus::Active],
    )
    .await;
    assert_eq!(compacting.turns.len(), 2, "{:?}", compacting.turns);
    assert_eq!(compacting.turns[1].status, TurnStatus::Active);
    assert_eq!(
        compacting.messages.len(),
        1,
        "nothing delegated the Continuation the compaction began: {:?}",
        compacting.messages
    );

    copilot.release_gate("done");
    let child = settled_session(&opened.client, child_id, 1).await;
    assert_eq!(
        child.turns[1].status,
        TurnStatus::Completed,
        "the compaction completing settles the Subagent's Continuation"
    );
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
        panic!("one Compaction is recorded: {:?}", child.activities);
    };
    assert_eq!(*turn_id, child.turns[1].id);
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(
        (*before_tokens, *after_tokens),
        (Some(90_000), Some(12_000))
    );
    let parent = opened
        .client
        .read_session(opened.session_id)
        .await
        .expect("read the parent Session");
    assert_eq!(parent.turns.len(), 1, "{:?}", parent.turns);
    assert!(compactions(&parent).is_empty());
    shutdown(opened).await;
}

/// A `session.history.cancelBackgroundCompaction` arm playing `timeline` in order, which answers
/// the cancel only if the timeline does.
fn cancel_arm_playing(timeline: &[&str]) -> String {
    format!(
        r#"    *'"method":"session.history.cancelBackgroundCompaction"'*)
{}      ;;
"#,
        timeline.concat()
    )
}

/// Copilot answering a cancel with nothing left to cancel.
const NOTHING_TO_CANCEL: &str = r#"        reply '{"jsonrpc":"2.0","id":'"$id"',"result":{"cancelled":false}}'
"#;

/// Copilot failing the cancel it was asked for.
const CANCEL_FAILS: &str = r#"      reply '{"jsonrpc":"2.0","id":'"$id"',"error":{"code":-32603,"message":"compaction processor busy"}}'
"#;

/// A Turn whose Subagent works on past the main loop going idle while Copilot compacts, and which
/// then asks permission to run a command.
const HELD_WHILE_A_SUBAGENT_ASKS: &str = r#"      agent_event s1 agent-1 subagent.started '{"toolCallId":"t-spawn","agentName":"researcher","agentDisplayName":"Researcher","agentDescription":"Scout the workspace"}'
      event c-start session.compaction_start '{"currentTokens":182000,"trigger":"threshold"}'
      event answer assistant.message '{"messageId":"m-answer","content":"Carrying on."}'
      event idle session.idle '{}'
      agent_event p agent-1 permission.requested '{"requestId":"child-stop","permissionRequest":{"kind":"shell","toolCallId":"t-child","fullCommandText":"danger","intention":"Stop this"}}'
"#;

const QUEUED_TURN: &str = r#"      event queued assistant.message '{"messageId":"m-queued","content":"Queued answer."}'
      event queued-idle session.idle '{}'
"#;

/// What Copilot answers an abort with when the main loop had already stopped and a Subagent
/// still worked: the acknowledgement first, then the Subagent cancelled and the aborted loop's
/// idle.
const ABORTED_SUBAGENT: &str = r#"      agent_event s9 agent-1 subagent.completed '{"toolCallId":"t-spawn","agentName":"researcher","agentDisplayName":"Researcher","cancelled":true}'
      event aborted session.idle '{"aborted":true}'
"#;

/// Waits for the Subagent's Approval in a Turn the compaction holds, returning the Subagent's
/// Session and the Approval.
async fn subagent_asking_while_held(
    opened: &Opened,
    feed: &mut SessionSubscription,
) -> (suru::protocol::SessionId, suru::protocol::ApprovalId) {
    let held = session_where(
        &opened.client,
        feed,
        opened.session_id,
        "the Subagent works on past the loop held on Copilot's compaction",
        |snapshot| {
            compaction_statuses(snapshot) == [ActivityStatus::Active]
                && agent_messages(snapshot).len() == 1
        },
    )
    .await;
    let child_id = held
        .activities
        .iter()
        .find_map(|activity| match activity {
            Activity::Subagent { session_id, .. } => Some(*session_id),
            _ => None,
        })
        .expect("the spawn opens a child Session");
    let mut child_feed = opened
        .client
        .subscribe_session(child_id)
        .await
        .expect("subscribe to the child Session");
    let asking = session_where(
        &opened.client,
        &mut child_feed,
        child_id,
        "the Subagent asks permission",
        |snapshot| snapshot.pending_approvals.len() == 1,
    )
    .await;
    (child_id, asking.pending_approvals[0])
}

#[tokio::test]
async fn an_abort_a_held_turns_subagents_need_settles_the_turn_on_its_own_idle() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}{}",
        deciding_arms(),
        prompts_arm(HELD_WHILE_A_SUBAGENT_ASKS, QUEUED_TURN),
        cancel_compaction_arm(CANCELLED),
        // The acknowledgement reaches Suru before the idle the abort ends in.
        abort_arm(ABORTED_SUBAGENT),
    ));
    let opened = opened_session(&copilot, "copilot-compaction-held-subagent", "Scout").await;
    let mut feed = feed(&opened).await;
    let (child_id, approval) = subagent_asking_while_held(&opened, &mut feed).await;
    deliver(&opened, "Then the lexer", PromptDelivery::Queue).await;

    opened
        .client
        .submit_decision(child_id, approval, Decision::DeclineAndInterrupt)
        .await
        .expect("Copilot takes the Decision and the interrupt");
    let answered = settled_session(&opened.client, opened.session_id, 1).await;

    assert_eq!(answered.turns[0].status, TurnStatus::Interrupted);
    assert_eq!(
        compaction_statuses(&answered),
        [ActivityStatus::Interrupted]
    );
    assert_eq!(
        answered.turns[1].status,
        TurnStatus::Completed,
        "the abort's idle settled the Turn it was for, not the queued Prompt's"
    );
    assert_eq!(
        agent_messages(&answered)
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        ["Carrying on.", "Queued answer."]
    );
    assert_eq!(requested(&copilot, "session.abort"), 1);
    shutdown(opened).await;
}

#[tokio::test]
async fn a_held_turn_whose_aborts_idle_never_comes_settles_once_the_wait_runs_out() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}{}",
        deciding_arms(),
        prompts_arm(HELD_WHILE_A_SUBAGENT_ASKS, QUEUED_TURN),
        cancel_compaction_arm(CANCELLED),
        abort_arm(""),
    ));
    let opened = opened_session_on(
        CopilotRuntime::new(copilot.executable())
            .with_interrupt_request_timeout(Duration::from_millis(200)),
        "copilot-compaction-held-no-idle",
        "Scout",
    )
    .await;
    let mut feed = feed(&opened).await;
    let (child_id, approval) = subagent_asking_while_held(&opened, &mut feed).await;
    deliver(&opened, "Then the lexer", PromptDelivery::Queue).await;

    opened
        .client
        .submit_decision(child_id, approval, Decision::DeclineAndInterrupt)
        .await
        .expect("Copilot takes the Decision and the interrupt");
    let answered = settled_session(&opened.client, opened.session_id, 1).await;

    assert_eq!(answered.turns[0].status, TurnStatus::Interrupted);
    assert_eq!(
        compaction_statuses(&answered),
        [ActivityStatus::Interrupted]
    );
    assert_eq!(answered.turns[1].status, TurnStatus::Completed);
    shutdown(opened).await;
}

#[tokio::test]
async fn a_compaction_failing_while_its_cancel_fails_keeps_copilots_error() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}{}",
        deciding_arms(),
        send_arm(COMPACTING_AND_ASKING),
        // The compaction fails on its own before Copilot fails the cancel.
        cancel_arm_playing(&[
            FAILED,
            CANCEL_FAILS,
            &after("$COPILOT_FIXTURE_RELEASE", &format!("{ANSWER}{IDLE}")),
        ]),
        abort_arm(""),
    ));
    let opened = opened_session(&copilot, "copilot-compaction-fails-cancel-fails", "Stop").await;
    let mut feed = feed(&opened).await;
    let approval = asking_while_compacting(&opened, &mut feed).await;

    opened
        .client
        .submit_decision(opened.session_id, approval, Decision::DeclineAndInterrupt)
        .await
        .expect_err("the interrupt Copilot could not carry out is reported");
    let failed = session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "the compaction settles as Copilot reported it",
        |snapshot| compaction_statuses(snapshot) == [ActivityStatus::Failed],
    )
    .await;
    let [Activity::Compaction { error, .. }] = compactions(&failed)[..] else {
        unreachable!()
    };
    assert_eq!(
        error.as_deref(),
        Some("Compaction failed: the model returned an empty summary"),
        "the cancel never took, so the failure was the compaction's own"
    );
    assert_eq!(failed.turns[0].status, TurnStatus::Active);
    assert_eq!(requested(&copilot, "session.abort"), 0);

    copilot.release();
    let settled = settled_session(&opened.client, opened.session_id, 0).await;
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    shutdown(opened).await;
}

#[tokio::test]
async fn a_compaction_failing_while_its_cancel_goes_unanswered_keeps_copilots_error() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}",
        deciding_arms(),
        send_arm(COMPACTING_AND_ASKING),
        cancel_arm_playing(&[FAILED]),
    ));
    let opened = opened_session_on(
        CopilotRuntime::new(copilot.executable())
            .with_interrupt_request_timeout(Duration::from_millis(200)),
        "copilot-compaction-fails-cancel-silent",
        "Stop",
    )
    .await;
    let mut feed = feed(&opened).await;
    let approval = asking_while_compacting(&opened, &mut feed).await;

    let error = opened
        .client
        .submit_decision(opened.session_id, approval, Decision::DeclineAndInterrupt)
        .await
        .expect_err("the cancel Copilot never answered fails the interrupt");
    assert!(
        error
            .to_string()
            .contains("timed out handling `session.history.cancelBackgroundCompaction`"),
        "{error:#}"
    );
    let failed = session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "the compaction settles as Copilot reported it",
        |snapshot| compaction_statuses(snapshot) == [ActivityStatus::Failed],
    )
    .await;
    let [Activity::Compaction { error, .. }] = compactions(&failed)[..] else {
        unreachable!()
    };
    assert_eq!(
        error.as_deref(),
        Some("Compaction failed: the model returned an empty summary")
    );
    assert_eq!(failed.turns[0].status, TurnStatus::Active);
    shutdown(opened).await;
}

#[tokio::test]
async fn the_next_turn_waits_out_an_interrupt_whose_cancel_goes_unanswered() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}",
        deciding_arms(),
        prompts_arm(COMPACTING_AND_ASKING, QUEUED_TURN),
        // Copilot finishes the compaction and the loop, and never answers the cancel.
        cancel_arm_playing(&[COMPLETED, ANSWER, IDLE]),
    ));
    let opened = opened_session_on(
        CopilotRuntime::new(copilot.executable())
            .with_interrupt_request_timeout(Duration::from_millis(400)),
        "copilot-compaction-cancel-silent-next-turn",
        "Stop",
    )
    .await;
    let mut feed = feed(&opened).await;
    let approval = asking_while_compacting(&opened, &mut feed).await;
    deliver(&opened, "Then the lexer", PromptDelivery::Queue).await;

    let (decided, ()) = tokio::join!(
        opened
            .client
            .submit_decision(opened.session_id, approval, Decision::DeclineAndInterrupt,),
        async {
            session_where(
                &opened.client,
                &mut feed,
                opened.session_id,
                "the Turn settles while the cancel is still unanswered",
                |snapshot| snapshot.turns[0].status != TurnStatus::Active,
            )
            .await;
            assert_eq!(
                requested(&copilot, "session.send"),
                1,
                "the queued Prompt waits for the interrupt still at work"
            );
        }
    );
    decided.expect_err("the cancel Copilot never answered fails the interrupt");
    let answered = settled_session(&opened.client, opened.session_id, 1).await;

    assert_eq!(answered.turns[0].status, TurnStatus::Completed);
    assert_eq!(compaction_statuses(&answered), [ActivityStatus::Completed]);
    assert_eq!(answered.turns[1].status, TurnStatus::Completed);
    assert_eq!(
        agent_messages(&answered)
            .last()
            .map(|message| message.content.as_str()),
        Some("Queued answer.")
    );
    assert_eq!(requested(&copilot, "session.abort"), 0);
    shutdown(opened).await;
}

#[tokio::test]
async fn an_abort_idle_arriving_after_the_wait_ran_out_leaves_the_next_turn_alone() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}{}",
        deciding_arms(),
        prompts_arm(
            HELD_WHILE_A_SUBAGENT_ASKS,
            &format!(
                r#"      event queued-start assistant.message_start '{{"messageId":"m-queued"}}'
{}{}"#,
                reading("queued-running", 160_000),
                after("$COPILOT_FIXTURE_RELEASE-answer", QUEUED_TURN)
            ),
        ),
        cancel_compaction_arm(CANCELLED),
        // The idle the abort ends in comes only once the next Turn has started.
        abort_arm(&after(
            "$COPILOT_FIXTURE_RELEASE",
            &format!("{ABORTED_SUBAGENT}{}", reading("late-idle", 170_000))
        )),
    ));
    let opened = opened_session_on(
        CopilotRuntime::new(copilot.executable())
            .with_interrupt_request_timeout(Duration::from_millis(200)),
        "copilot-compaction-late-abort-idle",
        "Scout",
    )
    .await;
    let mut feed = feed(&opened).await;
    let (child_id, approval) = subagent_asking_while_held(&opened, &mut feed).await;
    deliver(&opened, "Then the lexer", PromptDelivery::Queue).await;
    opened
        .client
        .submit_decision(child_id, approval, Decision::DeclineAndInterrupt)
        .await
        .expect("Copilot takes the Decision and the interrupt");
    session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "the queued Prompt's Turn runs",
        |snapshot| snapshot.session.context_fill == fill(160_000),
    )
    .await;

    copilot.release();
    let late = session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "the abort's late idle has been read",
        |snapshot| snapshot.session.context_fill == fill(170_000),
    )
    .await;
    assert_eq!(late.turns[0].status, TurnStatus::Interrupted);
    assert_eq!(
        late.turns[1].status,
        TurnStatus::Active,
        "the idle the earlier abort ended in is not the next Turn's"
    );

    copilot.release_gate("answer");
    let answered = settled_session(&opened.client, opened.session_id, 1).await;
    assert_eq!(answered.turns[1].status, TurnStatus::Completed);
    assert_eq!(
        agent_messages(&answered)
            .last()
            .map(|message| message.content.as_str()),
        Some("Queued answer.")
    );
    shutdown(opened).await;
}

#[tokio::test]
async fn a_compaction_failing_while_its_cancel_finds_nothing_to_cancel_keeps_copilots_error() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}{}",
        deciding_arms(),
        send_arm(COMPACTING_AND_ASKING),
        // The compaction fails on its own, so Copilot finds nothing left to cancel.
        cancel_arm_playing(&[
            FAILED,
            &reading("failed", 150_000),
            &after("$COPILOT_FIXTURE_RELEASE", NOTHING_TO_CANCEL),
        ]),
        abort_arm(
            r#"      event aborted session.idle '{"aborted":true}'
"#
        ),
    ));
    let opened = opened_session(&copilot, "copilot-compaction-nothing-to-cancel", "Stop").await;
    let mut feed = feed(&opened).await;
    let approval = asking_while_compacting(&opened, &mut feed).await;

    let (decided, ()) = tokio::join!(
        opened
            .client
            .submit_decision(opened.session_id, approval, Decision::DeclineAndInterrupt,),
        async {
            session_where(
                &opened.client,
                &mut feed,
                opened.session_id,
                "Copilot reports the compaction failing while the cancel is out",
                |snapshot| snapshot.session.context_fill == fill(150_000),
            )
            .await;
            copilot.release();
        }
    );
    decided.expect("Copilot takes the Decision and the interrupt");
    let stopped = settled_session(&opened.client, opened.session_id, 0).await;

    assert_eq!(stopped.turns[0].status, TurnStatus::Interrupted);
    let [Activity::Compaction { status, error, .. }] = compactions(&stopped)[..] else {
        panic!("one Compaction is recorded: {:?}", stopped.activities);
    };
    assert_eq!(
        (*status, error.as_deref()),
        (
            ActivityStatus::Failed,
            Some("Compaction failed: the model returned an empty summary")
        ),
        "Copilot cancelled nothing, so the failure it reported was the compaction's own"
    );
    assert_eq!(requested(&copilot, "session.abort"), 1);
    shutdown(opened).await;
}
