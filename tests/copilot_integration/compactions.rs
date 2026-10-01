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
//! settles. Stopping either — by interrupt, or by the next Prompt — cancels the compaction. A
//! cancel that finds it already over stopped nothing of it: it ends as Copilot reports, even once
//! Copilot's answer to the cancel has reached Suru ahead of that report.
//!
//! A Compaction the user asks for is Copilot's `session.history.compact`, triggered as manual and
//! carrying the user's instructions for the summary as `customInstructions` where there are any,
//! which reports no turn of its own: Suru opens its Turn, the same events report the compaction in
//! it, the `compactionTokensUsed` its end carries is the Turn's Usage, and Copilot's answer to the
//! request settles it. Interrupting it is `session.history.abortManualCompaction`, after which
//! Copilot fails the compaction and the request as "Compaction Cancelled" — the stop Suru asked for.
//!
//! The wire shapes follow github-copilot-sdk 1.0.15-preview.3's `SessionCompactionStartData`,
//! `SessionCompactionCompleteData`, `HistoryCompactRequest`, `HistoryCompactResult` and
//! `HistoryAbortManualCompactionResult`. Copilot writes a manual compaction's events before its
//! answer to the request, as #457's live run saw, and under the manual trigger Suru asks with; an
//! answer that reaches Suru ahead of them anyway — a failure included — waits for them, so their
//! counts and Usage land on the Compaction and its Turn, and only reports under that trigger
//! decide anything of it. While Copilot still owes reports of a compaction it answered, the next
//! request is turned away rather than begun.

use crate::support::{
    Opened, ScriptedCopilot, abort_arm, agent_messages, conversation_arms, conversation_fixture,
    opened_session, opened_session_on, permission_decision_arm, send_arm, session_where,
    settled_session,
};
use suru::managed_client::SessionSubscription;
use suru::protocol::{
    Activity, ActivityStatus, AdmitPromptRequest, CompactSessionRequest, CompactionTrigger,
    ContextFill, Decision, InitialPrompt, MessageRole, PromptDelivery, PromptId, PromptStatus,
    SessionError, SessionErrorCode, SessionSnapshot, SessionStatus, TurnStatus, Usage,
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
            instructions: None,
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

/// `timeline` played a moment after what came before it, so that what Copilot reports next
/// reaches Suru after its answer to the request in hand rather than racing it.
fn a_moment_later(timeline: &str) -> String {
    format!("      sleep 0.1\n{timeline}")
}

/// A `session.history.cancelBackgroundCompaction` arm answering that Copilot found nothing left
/// to cancel, and only then reporting `end`: how the compaction it had already finished went.
fn cancel_arm_finding_it_ended(end: &str) -> String {
    cancel_arm_playing(&[NOTHING_TO_CANCEL, &a_moment_later(end)])
}

/// A Turn in which Copilot compacts while its loop works on.
const COMPACTING_WHILE_WORKING: &str = r#"      event c-start session.compaction_start '{"currentTokens":182000,"trigger":"threshold"}'
      event working assistant.message_start '{"messageId":"m-working"}'
"#;

/// The one Compaction a Session holds as a test reads it: the index of the Turn it stands in, its
/// status, its counts before and after, its summary, and its error.
type SettledCompaction<'a> = (
    usize,
    ActivityStatus,
    (Option<u64>, Option<u64>),
    Option<&'a str>,
    Option<&'a str>,
);

/// The one Compaction `snapshot` holds.
fn the_compaction(snapshot: &SessionSnapshot) -> SettledCompaction<'_> {
    let [
        Activity::Compaction {
            turn_id,
            status,
            before_tokens,
            after_tokens,
            summary,
            error,
            ..
        },
    ] = compactions(snapshot)[..]
    else {
        panic!("one Compaction is recorded: {:?}", snapshot.activities);
    };
    (
        snapshot
            .turns
            .iter()
            .position(|turn| turn.id == *turn_id)
            .expect("a Compaction stands in a Turn of its Session"),
        *status,
        (*before_tokens, *after_tokens),
        summary.as_deref(),
        error.as_deref(),
    )
}

#[tokio::test]
async fn a_compaction_whose_cancel_found_it_ended_completes_in_the_turn_it_held_as_copilot_says() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}",
        conversation_arms(),
        prompts_arm(
            &format!("{STARTED}{ANSWER}{IDLE}{}", reading("held", 182_000)),
            QUEUED_TURN
        ),
        cancel_arm_finding_it_ended(COMPLETED),
    ));
    let opened = opened_session(&copilot, "copilot-compaction-ended-before-cancel", "Go").await;
    let mut feed = feed(&opened).await;
    session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "Copilot's loop goes idle while it compacts",
        |snapshot| snapshot.session.context_fill == fill(182_000),
    )
    .await;
    deliver(&opened, "Then the lexer", PromptDelivery::Queue).await;

    opened
        .client
        .interrupt_session(opened.session_id)
        .await
        .expect("the interrupt stops nothing, which is no failure");
    let answered = settled_session(&opened.client, opened.session_id, 1).await;

    assert_eq!(
        the_compaction(&answered),
        (
            0,
            ActivityStatus::Completed,
            (Some(182_000), Some(31_000)),
            Some("<overview>The parser work is half done.</overview>"),
            None
        ),
        "Copilot finished compacting before the cancel reached it, so the Agent's context was \
         compacted, as its report says, in the Turn the compaction held"
    );
    assert_eq!(
        answered.turns[0].status,
        TurnStatus::Completed,
        "the interrupt stopped nothing, so the Turn settles as Copilot's loop said"
    );
    assert_eq!(answered.turns[1].status, TurnStatus::Completed);
    assert_eq!(
        agent_messages(&answered)
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        ["Carrying on.", "Queued answer."]
    );
    assert_eq!(requested(&copilot, "session.abort"), 0);
    shutdown(opened).await;
}

#[tokio::test]
async fn a_compaction_whose_cancel_found_it_ended_keeps_the_error_it_failed_with() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}",
        conversation_arms(),
        send_arm(&format!(
            "{STARTED}{ANSWER}{IDLE}{}",
            reading("held", 182_000)
        )),
        cancel_arm_finding_it_ended(FAILED),
    ));
    let opened = opened_session(&copilot, "copilot-compaction-failed-before-cancel", "Go").await;
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
        .expect("the interrupt stops nothing, which is no failure");
    let settled = settled_session(&opened.client, opened.session_id, 0).await;

    assert_eq!(
        the_compaction(&settled),
        (
            0,
            ActivityStatus::Failed,
            (None, None),
            None,
            Some("Compaction failed: the model returned an empty summary")
        ),
        "the failure Copilot reports after finding nothing to cancel is the compaction's own"
    );
    assert_eq!(
        settled.turns[0].status,
        TurnStatus::Completed,
        "a failed automatic Compaction leaves its Turn to Copilot's loop"
    );
    shutdown(opened).await;
}

#[tokio::test]
async fn a_compaction_whose_end_never_follows_its_cancel_finding_nothing_settles_with_its_turn() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}",
        conversation_arms(),
        send_arm(&format!(
            "{STARTED}{ANSWER}{IDLE}{}",
            reading("held", 182_000)
        )),
        cancel_arm_playing(&[
            NOTHING_TO_CANCEL,
            &after(
                "$COPILOT_FIXTURE_RELEASE",
                &format!("{COMPLETED}{}", reading("late", 31_000))
            ),
        ]),
    ));
    let opened = opened_session_on(
        CopilotRuntime::new(copilot.executable())
            .with_interrupt_request_timeout(Duration::from_millis(200)),
        "copilot-compaction-end-never-follows-cancel",
        "Go",
    )
    .await;
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
        .expect("Copilot answers the cancel");
    let stopped = settled_session(&opened.client, opened.session_id, 0).await;
    assert_eq!(
        stopped.turns[0].status,
        TurnStatus::Interrupted,
        "with no report of the compaction's end within the wait, the interrupt settles the Turn"
    );
    assert_eq!(
        the_compaction(&stopped).1,
        ActivityStatus::Interrupted,
        "and its Compaction with it, guessing at no outcome"
    );

    copilot.release();
    let late = session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "Copilot's late report has been read",
        |snapshot| snapshot.session.context_fill == fill(31_000),
    )
    .await;
    assert_eq!(
        late.turns.len(),
        1,
        "the late end begins no Continuation: {:?}",
        late.turns
    );
    assert_eq!(the_compaction(&late).1, ActivityStatus::Interrupted);
    shutdown(opened).await;
}

#[tokio::test]
async fn a_compaction_whose_cancel_found_it_ended_while_the_loop_works_completes_as_copilot_says() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}{}",
        conversation_arms(),
        send_arm(COMPACTING_WHILE_WORKING),
        cancel_arm_finding_it_ended(COMPLETED),
        abort_arm(
            r#"      event aborted session.idle '{"aborted":true}'
"#
        ),
    ));
    let opened = opened_session(&copilot, "copilot-compaction-ended-loop-works", "Go").await;
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
        "the abort stopped the working loop"
    );
    assert_eq!(
        the_compaction(&stopped),
        (
            0,
            ActivityStatus::Completed,
            (Some(182_000), Some(31_000)),
            Some("<overview>The parser work is half done.</overview>"),
            None
        ),
        "the compaction had already ended, so the Agent's context was compacted all the same"
    );
    let (cancelled, aborted) = (
        copilot
            .methods()
            .iter()
            .position(|method| method == "session.history.cancelBackgroundCompaction"),
        copilot
            .methods()
            .iter()
            .position(|method| method == "session.abort"),
    );
    assert!(
        cancelled < aborted && cancelled.is_some(),
        "the compaction is cancelled before the loop is aborted: {:?}",
        copilot.methods()
    );
    shutdown(opened).await;
}

#[tokio::test]
async fn a_compaction_whose_cancel_found_it_ended_while_the_loop_works_keeps_its_error() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}{}",
        deciding_arms(),
        send_arm(COMPACTING_AND_ASKING),
        cancel_arm_finding_it_ended(FAILED),
        abort_arm(
            r#"      event aborted session.idle '{"aborted":true}'
"#
        ),
    ));
    let opened = opened_session(&copilot, "copilot-compaction-failed-loop-works", "Stop").await;
    let mut feed = feed(&opened).await;
    let approval = asking_while_compacting(&opened, &mut feed).await;

    opened
        .client
        .submit_decision(opened.session_id, approval, Decision::DeclineAndInterrupt)
        .await
        .expect("Copilot takes the Decision and the interrupt");
    let stopped = settled_session(&opened.client, opened.session_id, 0).await;

    assert_eq!(stopped.turns[0].status, TurnStatus::Interrupted);
    assert_eq!(
        the_compaction(&stopped),
        (
            0,
            ActivityStatus::Failed,
            (None, None),
            None,
            Some("Compaction failed: the model returned an empty summary")
        ),
        "Copilot cancelled nothing, so the failure it reported after was the compaction's own"
    );
    assert_eq!(requested(&copilot, "session.abort"), 1);
    shutdown(opened).await;
}

#[tokio::test]
async fn a_prompt_cancelling_a_compaction_that_had_ended_leaves_it_to_settle_its_continuation() {
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
        cancel_arm_finding_it_ended(COMPLETED),
    ));
    let opened = opened_session(&copilot, "copilot-compaction-ended-before-prompt", "Go").await;
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

    assert!(answered.turns[1].is_continuation());
    assert_eq!(
        the_compaction(&answered),
        (
            1,
            ActivityStatus::Completed,
            (Some(182_000), Some(31_000)),
            Some("<overview>The parser work is half done.</overview>"),
            None
        ),
        "the compaction had ended before the cancel reached Copilot, and stands in its \
         Continuation as Copilot reports it"
    );
    assert_eq!(
        answered.turns[1].status,
        TurnStatus::Completed,
        "nothing was left to stop of the Continuation it settled"
    );
    assert_eq!(answered.turns[2].status, TurnStatus::Completed);
    assert_eq!(
        agent_messages(&answered)
            .last()
            .map(|message| message.content.as_str()),
        Some("On to the lexer.")
    );
    shutdown(opened).await;
}

#[tokio::test]
async fn a_held_turns_subagents_abort_settles_it_after_the_compaction_its_cancel_found_ended() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}{}",
        deciding_arms(),
        prompts_arm(HELD_WHILE_A_SUBAGENT_ASKS, QUEUED_TURN),
        cancel_arm_finding_it_ended(COMPLETED),
        abort_arm(ABORTED_SUBAGENT),
    ));
    let opened = opened_session(&copilot, "copilot-compaction-ended-held-subagent", "Scout").await;
    let mut feed = feed(&opened).await;
    let (child_id, approval) = subagent_asking_while_held(&opened, &mut feed).await;
    deliver(&opened, "Then the lexer", PromptDelivery::Queue).await;

    opened
        .client
        .submit_decision(child_id, approval, Decision::DeclineAndInterrupt)
        .await
        .expect("Copilot takes the Decision and the interrupt");
    let answered = settled_session(&opened.client, opened.session_id, 1).await;

    assert_eq!(
        answered.turns[0].status,
        TurnStatus::Interrupted,
        "the abort its Subagent needed stopped the Turn"
    );
    assert_eq!(
        the_compaction(&answered),
        (
            0,
            ActivityStatus::Completed,
            (Some(182_000), Some(31_000)),
            Some("<overview>The parser work is half done.</overview>"),
            None
        )
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

/// Copilot starting the manual compaction Suru asked for with `session.history.compact`.
const MANUAL_STARTED: &str = r#"      event m-start session.compaction_start '{"conversationTokens":150000,"currentTokens":182000,"systemTokens":12000,"toolDefinitionsTokens":20000,"tokenLimit":200000,"trigger":"manual"}'
"#;

/// The manual compaction completing, with the context it measured before and after and what its
/// summarising call spent.
const MANUAL_COMPLETED: &str = r#"      event m-complete session.compaction_complete '{"success":true,"trigger":"manual","preCompactionTokens":182000,"postCompactionTokens":31000,"messagesRemoved":40,"summaryContent":"<overview>The parser work is half done.</overview>","compactionTokensUsed":{"inputTokens":150000,"cacheReadTokens":20000,"outputTokens":2000,"duration":41000,"model":"claude-fixture"}}'
"#;

/// The manual compaction failing.
const MANUAL_FAILED: &str = r#"      event m-failed session.compaction_complete '{"success":false,"trigger":"manual","error":"Compaction failed: the model returned an empty summary","statusCode":500}'
"#;

/// The manual compaction ending cancelled, as Copilot reports one an abort stopped.
const MANUAL_CANCELLED: &str = r#"      event m-cancelled session.compaction_complete '{"success":false,"trigger":"manual","error":"Compaction Cancelled"}'
"#;

/// Copilot answering `session.history.compact` that it compacted.
const COMPACTED: &str = r#""result":{"success":true,"tokensRemoved":151000,"messagesRemoved":40,"summaryContent":"<overview>The parser work is half done.</overview>"}"#;

/// Copilot answering `session.history.compact` that the compaction failed, as an RPC failure.
const COMPACTION_ERROR: &str =
    r#""error":{"code":-32603,"message":"Compaction failed: the model returned an empty summary"}"#;

/// Copilot answering `session.history.compact` that it compacted nothing, as a result.
const NOT_COMPACTED: &str = r#""result":{"success":false,"tokensRemoved":0,"messagesRemoved":0}"#;

/// Copilot failing `session.history.compact` once the compaction was cancelled.
const CANCELLED_ERROR: &str = r#""error":{"code":-32603,"message":"Compaction Cancelled"}"#;

/// The line answering the `session.history.compact` request the compact arm held with `answer`, a
/// JSON-RPC `result` or `error` member.
fn answer_compaction(answer: &str) -> String {
    format!(
        r#"      reply '{{"jsonrpc":"2.0","id":'"$compact_id"',{answer}}}'
"#
    )
}

/// A `session.history.compact` arm playing `timeline`: the events of the compaction Copilot runs
/// for it, and the answer — or, with none, holding the request for an abort to answer.
fn compact_arm(timeline: &str) -> String {
    format!(
        r#"    *'"method":"session.history.compact"'*)
      sid=$(printf '%s' "$body" | sed -n 's/.*"sessionId":"\([^"]*\)".*/\1/p')
      compact_id=$id
{timeline}      ;;
"#
    )
}

/// A `session.history.abortManualCompaction` arm confirming the abort, then playing `timeline`
/// and failing the compaction it aborted as cancelled.
fn abort_manual_compaction_arm(timeline: &str) -> String {
    format!(
        r#"    *'"method":"session.history.abortManualCompaction"'*)
      reply '{{"jsonrpc":"2.0","id":'"$id"',"result":{{"aborted":true}}}}'
{timeline}{cancelled}      ;;
"#,
        cancelled = answer_compaction(CANCELLED_ERROR),
    )
}

/// A Session whose first Turn answers and settles, so the next thing it takes is a Compaction
/// request, which `compaction` answers; `arms` answers anything else.
fn compacting_on_request(compaction: &str, arms: &str) -> ScriptedCopilot {
    ScriptedCopilot::new(&format!(
        "{}{}{}{arms}",
        conversation_arms(),
        send_arm(&format!("{ANSWER}{IDLE}")),
        compact_arm(compaction),
    ))
}

/// Asks for a Compaction of the idle Session the fixture opened.
async fn request_compaction(opened: &Opened) {
    opened
        .client
        .compact_session(opened.session_id, CompactSessionRequest::default())
        .await
        .expect("the idle Session takes the request");
}

fn turn_has_error(snapshot: &SessionSnapshot, turn: usize) -> bool {
    snapshot.activities.iter().any(|activity| {
        matches!(
            activity,
            Activity::Error { turn_id, .. } if *turn_id == snapshot.turns[turn].id
        )
    })
}

fn user_messages(snapshot: &SessionSnapshot) -> usize {
    snapshot
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::User)
        .count()
}

#[tokio::test]
async fn a_requested_compaction_is_copilots_manual_compact_in_a_turn_settled_around_the_call() {
    let copilot = compacting_on_request(
        &format!(
            "{MANUAL_STARTED}{}",
            after(
                "$COPILOT_FIXTURE_RELEASE",
                &format!(
                    "{SUMMARISING}{MANUAL_COMPLETED}{}",
                    answer_compaction(COMPACTED)
                )
            )
        ),
        "",
    );
    let opened = opened_session(&copilot, "copilot-compaction-requested", "Keep going").await;
    let mut feed = feed(&opened).await;
    let before = settled_session(&opened.client, opened.session_id, 0).await;

    request_compaction(&opened).await;
    let compacting = session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "Copilot starts compacting on request",
        |snapshot| !compactions(snapshot).is_empty(),
    )
    .await;
    let compact = copilot.wait_for_request("session.history.compact").await;
    assert_eq!(
        compact["params"]["trigger"], "manual",
        "Copilot is asked for a manual compaction: {compact}"
    );
    assert!(
        compact["params"].get("customInstructions").is_none(),
        "nothing is asked of the summary: {compact}"
    );
    assert_eq!(compacting.turns.len(), 2, "{:?}", compacting.turns);
    let turn = &compacting.turns[1];
    assert!(
        turn.compaction_requested && !turn.is_continuation(),
        "the request begins a Turn of its own: {turn:?}"
    );
    assert_eq!(turn.status, TurnStatus::Active);
    assert!(
        compacting.session.working_since.is_some(),
        "the Session is Working"
    );
    assert_eq!(
        compaction_statuses(&compacting),
        [ActivityStatus::Active],
        "the Compaction runs"
    );

    copilot.release();
    let settled = settled_session(&opened.client, opened.session_id, 1).await;
    assert_eq!(settled.turns.len(), 2, "{:?}", settled.turns);
    assert_eq!(settled.turns[1].status, TurnStatus::Completed);
    let [
        Activity::Compaction {
            turn_id,
            status,
            trigger,
            before_tokens,
            after_tokens,
            error,
            summary,
            summary_truncated,
            ..
        },
    ] = compactions(&settled)[..]
    else {
        panic!("one Compaction is recorded: {:?}", settled.activities);
    };
    assert_eq!(*turn_id, settled.turns[1].id);
    assert_eq!(
        (*status, *trigger, *before_tokens, *after_tokens, error),
        (
            ActivityStatus::Completed,
            CompactionTrigger::Manual,
            Some(182_000),
            Some(31_000),
            &None
        ),
        "the manual Compaction completes with Copilot's counts"
    );
    assert_eq!(
        (summary.as_deref(), *summary_truncated),
        (
            Some("<overview>The parser work is half done.</overview>"),
            false
        ),
        "and the `summaryContent` it left"
    );
    assert_eq!(
        settled.turns[1].usage,
        Some(Usage {
            fresh_input_tokens: Some(130_000),
            cache_read_tokens: Some(20_000),
            output_tokens: Some(2_000),
            ..Usage::default()
        }),
        "what the summarising call spent is the Turn's Usage"
    );
    assert!(
        settled.turns[1].cost.is_some(),
        "and is priced like any Turn's"
    );
    assert_eq!(
        agent_messages(&settled).len(),
        agent_messages(&before).len(),
        "the summarising call is no Message of the Agent's"
    );
    assert_eq!(
        user_messages(&settled),
        user_messages(&before),
        "no user Message is drawn for a Suru command"
    );
    assert_eq!(settled.session.status, SessionStatus::Idle);

    deliver(&opened, "Now the lexer", PromptDelivery::Queue).await;
    let answered = settled_session(&opened.client, opened.session_id, 2).await;
    assert_eq!(answered.turns[2].status, TurnStatus::Completed);
    shutdown(opened).await;
}

#[tokio::test]
async fn instructions_for_a_requested_compaction_are_copilots_custom_instructions_and_stay_on_it() {
    const INSTRUCTIONS: &str = "Keep the parser notes\nand the lexer plan";
    let copilot = compacting_on_request(
        &format!(
            "{MANUAL_STARTED}{SUMMARISING}{MANUAL_COMPLETED}{}",
            answer_compaction(COMPACTED)
        ),
        "",
    );
    let opened = opened_session(&copilot, "copilot-compaction-instructions", "Keep going").await;
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
        .expect("Copilot takes instructions, so the idle Session takes the request");
    let compact = copilot.wait_for_request("session.history.compact").await;
    assert_eq!(
        (
            &compact["params"]["trigger"],
            &compact["params"]["customInstructions"]
        ),
        (
            &serde_json::json!("manual"),
            &serde_json::json!(INSTRUCTIONS)
        ),
        "Copilot is asked for a manual compaction keeping what the instructions say: {compact}"
    );
    let settled = settled_session(&opened.client, opened.session_id, 1).await;
    assert_eq!(settled.turns[1].status, TurnStatus::Completed);
    let [
        Activity::Compaction {
            status,
            trigger,
            instructions,
            summary,
            ..
        },
    ] = compactions(&settled)[..]
    else {
        panic!("one Compaction is recorded: {:?}", settled.activities);
    };
    assert_eq!(
        (
            *status,
            *trigger,
            instructions.as_deref(),
            summary.as_deref()
        ),
        (
            ActivityStatus::Completed,
            CompactionTrigger::Manual,
            Some(INSTRUCTIONS),
            Some("<overview>The parser work is half done.</overview>"),
        ),
        "the manual Compaction keeps what it was asked to keep beside the summary it left"
    );
    shutdown(opened).await;
}

#[tokio::test]
async fn a_requested_compaction_copilot_fails_settles_it_and_its_turn_failed_with_copilots_error() {
    for (described, timeline, error) in [
        (
            "failed, then the request failing",
            format!(
                "{MANUAL_STARTED}{MANUAL_FAILED}{}",
                answer_compaction(COMPACTION_ERROR)
            ),
            "Compaction failed: the model returned an empty summary",
        ),
        (
            "failed, then the request answering it compacted nothing",
            format!(
                "{MANUAL_STARTED}{MANUAL_FAILED}{}",
                answer_compaction(NOT_COMPACTED)
            ),
            "Compaction failed: the model returned an empty summary",
        ),
        // A cancellation Suru never asked for is Copilot's own failure.
        (
            "cancelled by no abort of Suru's",
            format!(
                "{MANUAL_STARTED}{MANUAL_CANCELLED}{}",
                answer_compaction(CANCELLED_ERROR)
            ),
            "Compaction Cancelled",
        ),
    ] {
        let copilot = compacting_on_request(&timeline, "");
        let opened = opened_session(
            &copilot,
            "copilot-compaction-requested-failed",
            "Keep going",
        )
        .await;
        settled_session(&opened.client, opened.session_id, 0).await;
        request_compaction(&opened).await;
        let settled = settled_session(&opened.client, opened.session_id, 1).await;

        assert_eq!(settled.turns.len(), 2, "{described}: {:?}", settled.turns);
        assert_eq!(
            settled.turns[1].status,
            TurnStatus::Failed,
            "{described}: the Turn fails as its Compaction did"
        );
        let [
            Activity::Compaction {
                status,
                trigger,
                error: recorded,
                ..
            },
        ] = compactions(&settled)[..]
        else {
            panic!(
                "{described}: one Compaction is recorded: {:?}",
                settled.activities
            );
        };
        assert_eq!(
            (*status, *trigger, recorded.as_deref()),
            (
                ActivityStatus::Failed,
                CompactionTrigger::Manual,
                Some(error)
            ),
            "{described}: the Compaction fails with Copilot's error"
        );
        assert!(
            !turn_has_error(&settled, 1),
            "{described}: the Compaction already says why, so nothing stands beside it: {:?}",
            settled.activities
        );
        assert_eq!(settled.session.status, SessionStatus::Idle);
        shutdown(opened).await;
    }
}

#[tokio::test]
async fn interrupting_a_requested_compaction_aborts_it_and_settles_both_interrupted() {
    let copilot = compacting_on_request(
        MANUAL_STARTED,
        &abort_manual_compaction_arm(MANUAL_CANCELLED),
    );
    let opened = opened_session(&copilot, "copilot-compaction-requested-stop", "Keep going").await;
    let mut feed = feed(&opened).await;
    settled_session(&opened.client, opened.session_id, 0).await;
    request_compaction(&opened).await;
    session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "Copilot starts compacting on request",
        |snapshot| compaction_statuses(snapshot) == [ActivityStatus::Active],
    )
    .await;

    opened
        .client
        .interrupt_session(opened.session_id)
        .await
        .expect("Copilot aborts the compaction");
    let settled = settled_session(&opened.client, opened.session_id, 1).await;

    assert_eq!(
        settled.turns[1].status,
        TurnStatus::Interrupted,
        "the cancellation Suru asked for is a stop, not a failure"
    );
    let [Activity::Compaction { status, error, .. }] = compactions(&settled)[..] else {
        panic!("one Compaction is recorded: {:?}", settled.activities);
    };
    assert_eq!((*status, error), (ActivityStatus::Interrupted, &None));
    assert!(
        !turn_has_error(&settled, 1),
        "nothing stands beside a stop: {:?}",
        settled.activities
    );
    assert_eq!(
        requested(&copilot, "session.history.abortManualCompaction"),
        1
    );
    assert_eq!(
        (
            requested(&copilot, "session.history.cancelBackgroundCompaction"),
            requested(&copilot, "session.abort")
        ),
        (0, 0),
        "a manual compaction is aborted as one, never cancelled as a background one or by \
         aborting a loop that is not running"
    );
    assert_eq!(settled.session.status, SessionStatus::Idle);

    deliver(&opened, "Now the lexer", PromptDelivery::Queue).await;
    let answered = settled_session(&opened.client, opened.session_id, 2).await;
    assert_eq!(answered.turns[2].status, TurnStatus::Completed);
    shutdown(opened).await;
}

#[tokio::test]
async fn copilot_is_never_asked_to_compact_while_its_session_works() {
    let copilot = ScriptedCopilot::new(&format!(
        "{}{}{}",
        conversation_arms(),
        send_arm(&format!(
            "{ANSWER}{}",
            after("$COPILOT_FIXTURE_RELEASE", IDLE)
        )),
        compact_arm(&answer_compaction(COMPACTED)),
    ));
    let opened = opened_session(&copilot, "copilot-compaction-requested-busy", "Keep going").await;
    let mut feed = feed(&opened).await;
    session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "the first Turn answers and works on",
        |snapshot| !agent_messages(snapshot).is_empty(),
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
    copilot.release();
    let settled = settled_session(&opened.client, opened.session_id, 0).await;
    assert_eq!(settled.turns.len(), 1, "{:?}", settled.turns);
    assert_eq!(
        requested(&copilot, "session.history.compact"),
        0,
        "Copilot, which would take the compaction mid-Turn and lose it, is never asked"
    );
    shutdown(opened).await;
}

#[tokio::test]
async fn a_requested_compaction_failing_while_its_abort_is_out_keeps_copilots_error() {
    // The compaction fails on its own, and Copilot answers the request with that failure, before
    // it reads the abort, which then finds nothing to abort.
    let copilot = compacting_on_request(
        MANUAL_STARTED,
        &format!(
            r#"    *'"method":"session.history.abortManualCompaction"'*)
{MANUAL_FAILED}{}      reply '{{"jsonrpc":"2.0","id":'"$id"',"result":{{"aborted":false}}}}'
      ;;
"#,
            answer_compaction(COMPACTION_ERROR)
        ),
    );
    let opened = opened_session(&copilot, "copilot-compaction-requested-ended", "Keep going").await;
    let mut feed = feed(&opened).await;
    settled_session(&opened.client, opened.session_id, 0).await;
    request_compaction(&opened).await;
    session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "Copilot starts compacting on request",
        |snapshot| compaction_statuses(snapshot) == [ActivityStatus::Active],
    )
    .await;

    opened
        .client
        .interrupt_session(opened.session_id)
        .await
        .expect("an abort finding the compaction ended is no failure of the interrupt");
    let settled = settled_session(&opened.client, opened.session_id, 1).await;

    assert_eq!(
        settled.turns[1].status,
        TurnStatus::Failed,
        "the abort stopped nothing, so the failure is the compaction's own"
    );
    let [Activity::Compaction { status, error, .. }] = compactions(&settled)[..] else {
        panic!("one Compaction is recorded: {:?}", settled.activities);
    };
    assert_eq!(
        (*status, error.as_deref()),
        (
            ActivityStatus::Failed,
            Some("Compaction failed: the model returned an empty summary")
        ),
        "Copilot's error is kept rather than taken for the stop"
    );
    assert!(
        !turn_has_error(&settled, 1),
        "the Compaction already says why: {:?}",
        settled.activities
    );
    assert_eq!(settled.session.status, SessionStatus::Idle);
    shutdown(opened).await;
}

/// What Copilot reports, late, of a background compaction Suru cancelled — what its summarising
/// call had spent included.
const STALE_CANCELLED: &str = r#"      event c-stale session.compaction_complete '{"success":false,"trigger":"threshold","error":"Compaction cancelled","compactionTokensUsed":{"inputTokens":9000,"outputTokens":10,"model":"claude-fixture"}}'
"#;

/// The same late end from a CLI that names no trigger.
const STALE_CANCELLED_UNTAGGED: &str = r#"      event c-stale session.compaction_complete '{"success":false,"error":"Compaction cancelled","compactionTokensUsed":{"inputTokens":9000,"outputTokens":10,"model":"claude-fixture"}}'
"#;

/// What `MANUAL_COMPLETED`'s summarising call spent, as the Turn's Usage.
fn manual_usage() -> Option<Usage> {
    Some(Usage {
        fresh_input_tokens: Some(130_000),
        cache_read_tokens: Some(20_000),
        output_tokens: Some(2_000),
        ..Usage::default()
    })
}

/// One Compaction as a test reads it: its status and trigger, the index of the Turn it stands in,
/// and its counts before and after.
type MeasuredCompaction = (
    ActivityStatus,
    CompactionTrigger,
    usize,
    Option<u64>,
    Option<u64>,
);

/// The status, Turn, and counts of each Compaction in `snapshot`, in Transcript order.
fn compactions_measured(snapshot: &SessionSnapshot) -> Vec<MeasuredCompaction> {
    compactions(snapshot)
        .into_iter()
        .map(|compaction| match compaction {
            Activity::Compaction {
                turn_id,
                status,
                trigger,
                before_tokens,
                after_tokens,
                ..
            } => (
                *status,
                *trigger,
                snapshot
                    .turns
                    .iter()
                    .position(|turn| turn.id == *turn_id)
                    .expect("a Compaction stands in a Turn of its Session"),
                *before_tokens,
                *after_tokens,
            ),
            _ => unreachable!(),
        })
        .collect()
}

#[tokio::test]
async fn a_cancelled_background_compactions_late_end_never_reaches_the_manual_compaction_after_it()
{
    for (stale_before_the_start, stale) in [
        (true, STALE_CANCELLED),
        (false, STALE_CANCELLED),
        // A report naming no trigger could be either compaction's, so it decides nothing of the
        // manual one.
        (false, STALE_CANCELLED_UNTAGGED),
    ] {
        let manual = if stale_before_the_start {
            format!("{stale}{MANUAL_STARTED}{MANUAL_COMPLETED}")
        } else {
            format!("{MANUAL_STARTED}{stale}{MANUAL_COMPLETED}")
        };
        let copilot = ScriptedCopilot::new(&format!(
            "{}{}{}{}",
            conversation_arms(),
            send_arm(&format!(
                "{STARTED}{ANSWER}{IDLE}{}",
                reading("held", 182_000)
            )),
            // Copilot confirms the cancel, and reports the cancelled compaction's end only later.
            cancel_compaction_arm(""),
            compact_arm(&format!("{manual}{}", answer_compaction(COMPACTED))),
        ));
        let opened = opened_session(
            &copilot,
            "copilot-compaction-requested-after-cancel",
            "Keep going",
        )
        .await;
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
        settled_session(&opened.client, opened.session_id, 0).await;

        request_compaction(&opened).await;
        let settled = settled_session(&opened.client, opened.session_id, 1).await;

        assert_eq!(settled.turns.len(), 2, "{:?}", settled.turns);
        assert_eq!(
            settled.turns[1].status,
            TurnStatus::Completed,
            "stale before the start: {stale_before_the_start}"
        );
        assert_eq!(
            compactions_measured(&settled),
            [
                (
                    ActivityStatus::Interrupted,
                    CompactionTrigger::Automatic,
                    0,
                    None,
                    None
                ),
                (
                    ActivityStatus::Completed,
                    CompactionTrigger::Manual,
                    1,
                    Some(182_000),
                    Some(31_000)
                ),
            ],
            "stale before the start: {stale_before_the_start}: the cancelled compaction's end \
             settles nothing of the manual one"
        );
        assert_eq!(
            settled.turns[1].usage,
            manual_usage(),
            "stale before the start: {stale_before_the_start}: the Turn's Usage is the manual \
             compaction's alone"
        );
        shutdown(opened).await;
    }
}

#[tokio::test]
async fn an_answer_ahead_of_copilots_compaction_reports_waits_for_them() {
    let copilot = compacting_on_request(
        &format!(
            "{}{MANUAL_STARTED}{SUMMARISING}{MANUAL_COMPLETED}",
            answer_compaction(COMPACTED)
        ),
        "",
    );
    let opened = opened_session(&copilot, "copilot-compaction-answer-first", "Keep going").await;
    settled_session(&opened.client, opened.session_id, 0).await;
    request_compaction(&opened).await;
    let settled = settled_session(&opened.client, opened.session_id, 1).await;

    assert_eq!(
        settled.turns.len(),
        2,
        "no Continuation: {:?}",
        settled.turns
    );
    assert_eq!(settled.turns[1].status, TurnStatus::Completed);
    assert_eq!(
        compactions_measured(&settled),
        [(
            ActivityStatus::Completed,
            CompactionTrigger::Manual,
            1,
            Some(182_000),
            Some(31_000)
        )],
        "the reports after the answer land on the one manual Compaction"
    );
    assert!(
        matches!(
            compactions(&settled)[..],
            [Activity::Compaction { summary: Some(summary), .. }]
                if summary == "<overview>The parser work is half done.</overview>"
        ),
        "with the summary its end left: {:?}",
        settled.activities
    );
    assert_eq!(settled.turns[1].usage, manual_usage());
    shutdown(opened).await;
}

#[tokio::test]
async fn a_compaction_copilot_answers_then_exits_keeps_its_counts_and_usage() {
    // Copilot writes the compaction's end and its answer, then closes its output as it exits:
    // whatever the SDK still has to hand on reaches the Turn before the Session is lost.
    let copilot = compacting_on_request(
        &format!(
            "{MANUAL_STARTED}{MANUAL_COMPLETED}{}      exit 0\n",
            answer_compaction(COMPACTED)
        ),
        "",
    );
    let opened = opened_session(&copilot, "copilot-compaction-answer-exit", "Keep going").await;
    settled_session(&opened.client, opened.session_id, 0).await;
    request_compaction(&opened).await;
    let settled = settled_session(&opened.client, opened.session_id, 1).await;

    assert_eq!(
        settled.turns.len(),
        2,
        "no Continuation: {:?}",
        settled.turns
    );
    assert_eq!(
        compactions_measured(&settled),
        [(
            ActivityStatus::Completed,
            CompactionTrigger::Manual,
            1,
            Some(182_000),
            Some(31_000)
        )],
        "the compaction's own end is never overtaken"
    );
    assert_eq!(settled.turns[1].usage, manual_usage());
    shutdown(opened).await;
}

/// What Copilot reports, late, of the compaction it answered first: counts and a summary of its
/// own, so that a report landing on another compaction shows.
const LATE_STARTED: &str = r#"      event late-start session.compaction_start '{"currentTokens":150000,"trigger":"manual"}'
"#;
const LATE_COMPLETED: &str = r#"      event late-complete session.compaction_complete '{"success":true,"trigger":"manual","preCompactionTokens":150000,"postCompactionTokens":40000,"summaryContent":"<overview>An older summary.</overview>","compactionTokensUsed":{"inputTokens":9000,"outputTokens":10,"model":"claude-fixture"}}'
"#;

#[tokio::test]
async fn an_answered_compaction_whose_reports_never_come_settles_once_overdue_and_owes_them_nothing()
 {
    // The first compaction is answered with nothing reported of it; its reports reach Suru only
    // ahead of the Agent's next answer. Every compaction after it reports as it runs.
    let later = format!(
        "{MANUAL_STARTED}{MANUAL_COMPLETED}{}",
        answer_compaction(COMPACTED)
    );
    let copilot = ScriptedCopilot::new(&format!(
        r#"{}{}    *'"method":"session.history.compact"'*)
      sid=$(printf '%s' "$body" | sed -n 's/.*"sessionId":"\([^"]*\)".*/\1/p')
      compact_id=$id
      compactions=$(( ${{compactions:-0}} + 1 ))
      if [ "$compactions" -eq 1 ]; then
{first}      else
{later}      fi
      ;;
"#,
        conversation_arms(),
        prompts_arm(
            &format!("{ANSWER}{IDLE}"),
            &format!("{LATE_STARTED}{LATE_COMPLETED}{ANSWER}{IDLE}")
        ),
        first = answer_compaction(COMPACTED),
    ));
    let opened = opened_session_on(
        CopilotRuntime::new(copilot.executable())
            .with_interrupt_request_timeout(Duration::from_millis(100)),
        "copilot-compaction-overdue",
        "Keep going",
    )
    .await;
    settled_session(&opened.client, opened.session_id, 0).await;
    request_compaction(&opened).await;
    let settled = settled_session(&opened.client, opened.session_id, 1).await;
    assert_eq!(
        settled.turns[1].status,
        TurnStatus::Completed,
        "the Turn settles on Copilot's answer once its reports are overdue"
    );
    assert_eq!(
        compaction_statuses(&settled),
        [ActivityStatus::Completed],
        "Copilot answered that it compacted"
    );
    assert!(
        matches!(
            compactions(&settled)[..],
            [Activity::Compaction { summary: Some(summary), .. }]
                if summary == "<overview>The parser work is half done.</overview>"
        ),
        "with the summary its answer carried: {:?}",
        settled.activities
    );

    // Asked again while those reports are still owed, Copilot is not asked: they could be taken
    // for the new compaction's own.
    request_compaction(&opened).await;
    let refused = settled_session(&opened.client, opened.session_id, 2).await;
    assert_eq!(refused.turns[2].status, TurnStatus::Failed);
    assert!(
        compactions(&refused)
            .iter()
            .all(|compaction| compaction.turn_id() != refused.turns[2].id),
        "{:?}",
        refused.activities
    );
    assert!(
        refused.activities.iter().any(|activity| matches!(
            activity,
            Activity::Error { turn_id, text, .. }
                if *turn_id == refused.turns[2].id && text.contains("earlier compaction")
        )),
        "the refusal says why: {:?}",
        refused.activities
    );
    assert_eq!(
        requested(&copilot, "session.history.compact"),
        1,
        "Copilot was never asked"
    );

    // The owed reports reach Suru at last, ahead of the Agent's next answer: they record nothing
    // in the Turn that answer stands in, and begin no Continuation.
    deliver(&opened, "Carry on", PromptDelivery::Queue).await;
    let answered = settled_session(&opened.client, opened.session_id, 3).await;
    assert_eq!(answered.turns.len(), 4, "{:?}", answered.turns);
    assert_eq!(answered.turns[3].status, TurnStatus::Completed);
    assert_eq!(
        compaction_statuses(&answered),
        [ActivityStatus::Completed],
        "the late reports record nothing"
    );

    request_compaction(&opened).await;
    let compacted = settled_session(&opened.client, opened.session_id, 4).await;
    assert_eq!(compacted.turns[4].status, TurnStatus::Completed);
    assert_eq!(
        compactions_measured(&compacted),
        [
            (
                ActivityStatus::Completed,
                CompactionTrigger::Manual,
                1,
                None,
                None
            ),
            (
                ActivityStatus::Completed,
                CompactionTrigger::Manual,
                4,
                Some(182_000),
                Some(31_000)
            ),
        ],
        "the next compaction is measured by its own reports alone"
    );
    assert!(
        matches!(
            compactions(&compacted)[1],
            Activity::Compaction { summary: Some(summary), .. }
                if summary == "<overview>The parser work is half done.</overview>"
        ),
        "with its own summary: {:?}",
        compacted.activities
    );
    assert_eq!(compacted.turns[4].usage, manual_usage());
    shutdown(opened).await;
}

#[tokio::test]
async fn a_failure_copilot_answers_ahead_of_its_reports_waits_for_them() {
    for answer in [COMPACTION_ERROR, NOT_COMPACTED] {
        let copilot = compacting_on_request(
            &format!(
                "{}{MANUAL_STARTED}{MANUAL_FAILED}",
                answer_compaction(answer)
            ),
            "",
        );
        // A Turn that settled on the answer alone would have to wait out this bound first.
        let opened = opened_session_on(
            CopilotRuntime::new(copilot.executable())
                .with_interrupt_request_timeout(Duration::from_secs(120)),
            "copilot-compaction-failure-answer-first",
            "Keep going",
        )
        .await;
        settled_session(&opened.client, opened.session_id, 0).await;
        request_compaction(&opened).await;
        let settled = settled_session(&opened.client, opened.session_id, 1).await;

        assert_eq!(settled.turns.len(), 2, "{answer}: {:?}", settled.turns);
        assert_eq!(settled.turns[1].status, TurnStatus::Failed, "{answer}");
        let [Activity::Compaction { status, error, .. }] = compactions(&settled)[..] else {
            panic!(
                "{answer}: the reports after the answer record the Compaction: {:?}",
                settled.activities
            );
        };
        assert_eq!(
            (*status, error.as_deref()),
            (
                ActivityStatus::Failed,
                Some("Compaction failed: the model returned an empty summary")
            ),
            "{answer}"
        );
        assert!(
            !turn_has_error(&settled, 1),
            "{answer}: the Compaction already says why: {:?}",
            settled.activities
        );
        shutdown(opened).await;
    }
}

#[tokio::test]
async fn a_request_copilot_rejects_outright_fails_its_turn_at_once() {
    let copilot = compacting_on_request(
        &answer_compaction(
            r#""error":{"code":-32601,"message":"Unhandled method session.history.compact"}"#,
        ),
        "",
    );
    // Nothing is waited for: Copilot rejected the request before it could compact anything.
    let opened = opened_session_on(
        CopilotRuntime::new(copilot.executable())
            .with_interrupt_request_timeout(Duration::from_secs(120)),
        "copilot-compaction-rejected",
        "Keep going",
    )
    .await;
    settled_session(&opened.client, opened.session_id, 0).await;
    request_compaction(&opened).await;
    let settled = settled_session(&opened.client, opened.session_id, 1).await;

    assert_eq!(settled.turns[1].status, TurnStatus::Failed);
    assert!(compactions(&settled).is_empty(), "{:?}", settled.activities);
    assert!(
        settled.activities.iter().any(|activity| matches!(
            activity,
            Activity::Error { turn_id, text, .. }
                if *turn_id == settled.turns[1].id && text.contains("Unhandled method")
        )),
        "the Turn fails in Copilot's words: {:?}",
        settled.activities
    );
    shutdown(opened).await;
}

#[tokio::test]
async fn a_prompt_held_behind_a_copilot_compaction_that_is_interrupted_is_withdrawn() {
    let copilot = compacting_on_request(
        MANUAL_STARTED,
        &abort_manual_compaction_arm(MANUAL_CANCELLED),
    );
    let opened = opened_session(&copilot, "copilot-compaction-held-prompt", "Keep going").await;
    let mut feed = feed(&opened).await;
    settled_session(&opened.client, opened.session_id, 0).await;
    request_compaction(&opened).await;
    session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "Copilot starts compacting on request",
        |snapshot| compaction_statuses(snapshot) == [ActivityStatus::Active],
    )
    .await;
    let held = |snapshot: &SessionSnapshot| {
        snapshot
            .prompts
            .iter()
            .find(|prompt| prompt.text == "Now the lexer")
            .map(|prompt| prompt.status)
    };
    // Sent to the Turn at work, which takes no steer: it is held to begin the next Turn.
    deliver(&opened, "Now the lexer", PromptDelivery::Steer).await;
    let holding = session_where(
        &opened.client,
        &mut feed,
        opened.session_id,
        "the Prompt is held behind the Compaction",
        |snapshot| held(snapshot) == Some(PromptStatus::Pending),
    )
    .await;
    assert_eq!(holding.turns.len(), 2, "{:?}", holding.turns);

    opened
        .client
        .interrupt_session(opened.session_id)
        .await
        .expect("Copilot aborts the compaction");
    let settled = settled_session(&opened.client, opened.session_id, 1).await;

    assert_eq!(settled.turns[1].status, TurnStatus::Interrupted);
    assert_eq!(
        held(&settled),
        Some(PromptStatus::Cancelled),
        "a Prompt its writer meant for a compacted context is withdrawn, not sent into this one"
    );
    assert_eq!(
        settled.turns.len(),
        2,
        "it begins no Turn: {:?}",
        settled.turns
    );
    assert_eq!(settled.session.status, SessionStatus::Idle);
    assert_eq!(
        requested(&copilot, "session.send"),
        1,
        "Copilot never receives it"
    );
    shutdown(opened).await;
}
