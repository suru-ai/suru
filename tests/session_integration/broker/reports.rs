//! Subagent Reports (ADR 0035): when a brokered Subagent's Turn settles, Suru
//! delivers the delegating Agent an account of it — the outcome and a bounded
//! excerpt of the Subagent's final Message — as that Agent's own Provider
//! input. An idle Agent wakes into a Continuation; a Turn still working is
//! steered; an Agent with no Provider process takes it at the head of its
//! Session's next Turn. The Report stands nowhere in any Transcript: the
//! settled row is the record.
//!
//! Each test reads what the parent's double was handed, which is what its
//! Agent would read.

use suru::provider::{SubagentReport, SubagentReportOutcome};

use super::*;

/// The answer the Subagents below settle with.
const ANSWER: &str = "Three seams: ProviderRuntime, ProviderSession and the Broker's Tools.";

/// The Report `caller` is owed for `child`'s latest settled row, as its row
/// reads: the Subagent by the name the spawn gave it, how its stretch settled
/// and after how long, and its final Message.
fn report_of(
    caller: &SessionSnapshot,
    child: SessionId,
    outcome: SubagentReportOutcome,
    final_message: Option<&str>,
) -> SubagentReport {
    let (_, duration_ms) = row_status(caller, child);
    SubagentReport::new(child, "Researcher", outcome, duration_ms, final_message)
}

/// Reads `holder` until its row for `child` has settled.
async fn row_settles(
    descriptor: &RuntimeDescriptor,
    holder: SessionId,
    child: SessionId,
) -> SessionSnapshot {
    read_until(
        descriptor,
        holder,
        "the Subagent's row settles",
        |snapshot| row_status(snapshot, child).0 != ActivityStatus::Active,
    )
    .await
}

/// The Turn `provider` is next asked to begin.
async fn next_turn(
    provider: &mut ControlledProviderSession,
    what: &str,
) -> crate::provider_support::TurnStart {
    timeout(PROGRESS_DEADLINE, provider.next_turn())
        .await
        .unwrap_or_else(|_| panic!("{what}"))
}

/// Takes up the interrupt `provider` is asked for, as its Provider does.
async fn take_interrupt(provider: &mut ControlledProviderSession, whose: &str) {
    timeout(PROGRESS_DEADLINE, provider.next_interrupt())
        .await
        .unwrap_or_else(|_| panic!("the interrupt reaches {whose} Provider"))
        .succeed();
}

/// Asks the Server to interrupt `session_id`, as a client does.
async fn interrupt(descriptor: &RuntimeDescriptor, session_id: SessionId) {
    reqwest::Client::new()
        .post(format!(
            "{}/v1/sessions/{session_id}/interrupt",
            descriptor.base_url
        ))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("send Session interruption")
        .error_for_status()
        .expect("the interrupt is taken");
}

/// Sets `session_id` aside as done for now, or brings it back, as a client
/// does.
async fn settle_session(descriptor: &RuntimeDescriptor, session_id: SessionId, settled: bool) {
    reqwest::Client::new()
        .post(format!(
            "{}/v1/sessions/{session_id}/settlement",
            descriptor.base_url
        ))
        .bearer_auth(&descriptor.token)
        .json(&suru::protocol::SettleSessionRequest { settled })
        .send()
        .await
        .expect("send Session settlement")
        .error_for_status()
        .expect("the Session's settlement changes");
}

/// When the user last set `session_id` aside, as the Session listing says.
async fn settled_at(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
) -> Option<suru::protocol::SessionTimestamp> {
    reqwest::Client::new()
        .get(format!("{}/v1/sessions", descriptor.base_url))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("list Sessions")
        .error_for_status()
        .expect("Session listing succeeds")
        .json::<Vec<suru::protocol::SessionListItem>>()
        .await
        .expect("decode the Session listing")
        .into_iter()
        .find(|item| item.id() == session_id)
        .expect("the Session is listed")
        .settled_at()
}

/// Everything `snapshot`'s Transcript holds that stands in Turn `turn_id`.
fn held_in(snapshot: &SessionSnapshot, turn_id: suru::protocol::TurnId) -> usize {
    snapshot
        .messages
        .iter()
        .filter(|message| message.turn_id == turn_id)
        .count()
        + snapshot
            .activities
            .iter()
            .filter(|activity| activity.turn_id() == turn_id)
            .count()
}

#[tokio::test]
async fn a_subagent_settling_while_its_parent_is_idle_wakes_the_parent_into_a_continuation_with_the_report()
 {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-report-wake", None).await;
    let descriptor = delegating.descriptor.clone();
    let child_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (child_provider, _) =
        run_child(&mut delegating.hosted.codex, codex_selection("high")).await;
    settle_callers_turn(
        &delegating,
        "the caller's Turn settles while its Subagent works",
    )
    .await;

    say(&descriptor, child_id, &child_provider, ANSWER).await;
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;

    let woken = next_turn(
        &mut delegating.caller_provider,
        "the Report wakes the idle parent's Provider into a Turn",
    )
    .await;
    let caller = row_settles(&descriptor, delegating.caller, child_id).await;
    assert!(
        !woken.has_prompt(),
        "no Prompt begins it: the Report is the whole of its input"
    );
    assert_eq!(
        woken.reports(),
        [report_of(
            &caller,
            child_id,
            SubagentReportOutcome::Completed,
            Some(ANSWER)
        )],
        "the Report carries the outcome and the Subagent's final Message"
    );
    assert_eq!(
        woken.selection(),
        &default_selection(&claude_models()),
        "the parent's own Agent is the one woken"
    );
    woken.succeed();

    let continuing = read_until(
        &descriptor,
        delegating.caller,
        "the parent's Session shows the Continuation the Report woke",
        |snapshot| snapshot.turns.len() == 2,
    )
    .await;
    let continuation = &continuing.turns[1];
    assert_eq!(continuation.status, TurnStatus::Active);
    assert_eq!(
        continuation.prompt_id, None,
        "a Continuation, which no Prompt began"
    );
    assert_eq!(
        held_in(&continuing, continuation.id),
        0,
        "the Report stands nowhere in the parent's Transcript"
    );
    assert!(continuing.working_since().is_some());

    delegating
        .caller_provider
        .emit(ProviderEvent::AgentMessageStarted);
    delegating
        .caller_provider
        .emit(ProviderEvent::AgentMessageDelta {
            content: "The Researcher found three seams.".to_owned(),
        });
    delegating
        .caller_provider
        .emit(ProviderEvent::AgentMessageCompleted);
    delegating
        .caller_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    let settled = read_until(
        &descriptor,
        delegating.caller,
        "the Continuation settles at the parent's Provider's own boundary",
        |snapshot| snapshot.turns[1].status == TurnStatus::Completed,
    )
    .await;
    let in_continuation = settled
        .messages
        .iter()
        .filter(|message| message.turn_id == settled.turns[1].id)
        .map(|message| (message.role.clone(), message.content.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        in_continuation,
        [(MessageRole::Agent, "The Researcher found three seams.")],
        "the Continuation holds what the parent's Agent did, and nothing for the Report"
    );
    assert_eq!(settled.working_since(), None);
    let child = read_session(&descriptor, child_id).await;
    assert!(
        child
            .messages
            .iter()
            .all(|message| message.role != MessageRole::User),
        "nor does the Report stand in the Subagent's Transcript"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_report_reaching_a_parent_whose_turn_still_works_steers_that_turn() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-report-steer", None).await;
    let descriptor = delegating.descriptor.clone();
    let child_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (child_provider, _) =
        run_child(&mut delegating.hosted.codex, codex_selection("high")).await;
    let before = read_session(&descriptor, delegating.caller).await;

    // A final Message longer than a Report carries whole.
    let long_answer = format!("{ANSWER}{}", " More detail.".repeat(400));
    say(&descriptor, child_id, &child_provider, &long_answer).await;
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;

    let steer = timeout(PROGRESS_DEADLINE, delegating.caller_provider.next_steer())
        .await
        .expect("the Report reaches the parent's working Turn as a steer");
    let caller = row_settles(&descriptor, delegating.caller, child_id).await;
    let expected = report_of(
        &caller,
        child_id,
        SubagentReportOutcome::Completed,
        Some(&long_answer),
    );
    assert!(expected.truncated, "the excerpt stops short of the Message");
    assert_eq!(
        expected
            .excerpt
            .as_ref()
            .map(|excerpt| excerpt.chars().count()),
        Some(SubagentReport::EXCERPT_CHARS)
    );
    assert_eq!(steer.reports(), [expected]);
    steer.succeed();
    assert!(
        delegating.caller_provider.try_next_turn().is_none(),
        "no Turn begins for it"
    );

    let steered = read_session(&descriptor, delegating.caller).await;
    assert_eq!(
        steered.turns.len(),
        1,
        "the Report joined the Turn at work rather than beginning another"
    );
    assert_eq!(steered.turns[0].status, TurnStatus::Active);
    assert_eq!(
        steered.messages, before.messages,
        "the steer adds no Message to the parent's Transcript"
    );
    assert_eq!(
        steered.transcript.len(),
        before.transcript.len(),
        "and nothing else stands in it for the Report"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_report_whose_parent_has_no_provider_process_waits_for_the_head_of_the_parents_next_turn()
{
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-report-held", None).await;
    let descriptor = delegating.descriptor.clone();
    let child_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (child_provider, _) =
        run_child(&mut delegating.hosted.codex, codex_selection("high")).await;
    settle_callers_turn(
        &delegating,
        "the caller's Turn settles while its Subagent works",
    )
    .await;

    // The parent's Provider process ends while its Subagent works on.
    let Delegating {
        mut hosted,
        caller,
        caller_provider,
        handoff,
        ..
    } = delegating;
    drop(caller_provider);
    timeout(PROGRESS_DEADLINE, async {
        while McpClient::handed(&handoff).initialize_status().await != StatusCode::UNAUTHORIZED {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the parent's Provider connection is gone");

    say(&descriptor, child_id, &child_provider, ANSWER).await;
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    let settled = row_settles(&descriptor, caller, child_id).await;
    let expected = report_of(
        &settled,
        child_id,
        SubagentReportOutcome::Completed,
        Some(ANSWER),
    );
    assert_eq!(
        settled.turns.len(),
        1,
        "no Continuation begins without a Provider to take the Report"
    );

    admit_prompt(&descriptor, caller, "What did the Researcher find?").await;
    let relaunch = next_start(&mut hosted.claude).await;
    let mut relaunched = relaunch.succeed(AgentIdentity {
        agent: AgentId::new("claude-agent"),
        selection: default_selection(&claude_models()),
    });
    let turn = next_turn(
        &mut relaunched,
        "the Prompt's Turn reaches the Provider it relaunched",
    )
    .await;
    assert_eq!(
        turn.reports(),
        [expected],
        "the held Report stands at the head of the next Turn's input"
    );
    assert_eq!(
        turn.prompt(),
        "What did the Researcher find?",
        "beside the Prompt that began it"
    );
    turn.succeed();
    let prompted = read_session(&descriptor, caller).await;
    assert_eq!(prompted.turns.len(), 2);
    assert_eq!(
        prompted.turns[1].status,
        TurnStatus::Active,
        "the Prompt began the one Turn the Report was delivered in"
    );
    assert_eq!(
        held_in(&prompted, prompted.turns[1].id),
        1,
        "which holds the user's Message and nothing for the Report"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_report_of_a_subagent_a_restart_settled_waits_for_the_head_of_its_parents_next_turn() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "broker-report-restart";
    let (caller_id, child_id) =
        stop_with_a_brokered_subagent_working(state_dir.path(), channel, Some(ANSWER)).await;
    reopen_every_turn(state_dir.path(), channel, caller_id);

    let mut restarted = host_providers(state_dir.path(), channel, None).await;
    let descriptor = restarted.server.descriptor().clone();
    let caller = row_settles(&descriptor, caller_id, child_id).await;
    assert_eq!(
        row_status(&caller, child_id),
        (ActivityStatus::Failed, None)
    );

    admit_prompt(&descriptor, caller_id, "Where were we?").await;
    let start = next_start(&mut restarted.claude).await;
    let mut resumed = start.succeed(AgentIdentity {
        agent: AgentId::new("claude-agent"),
        selection: default_selection(&claude_models()),
    });
    let turn = next_turn(
        &mut resumed,
        "the Prompt's Turn reaches the parent's Provider",
    )
    .await;
    assert_eq!(
        turn.reports(),
        [SubagentReport::new(
            child_id,
            "Researcher",
            SubagentReportOutcome::Failed,
            None,
            None,
        )],
        "the restart settled the Subagent's Turn failed, which nothing timed, and its Agent is \
         told so at the head of the parent's next Turn"
    );
    assert_eq!(turn.prompt(), "Where were we?");
    turn.succeed();

    restarted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_report_delivered_to_a_session_the_user_had_settled_makes_it_active_again() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-report-unsettles", None).await;
    let descriptor = delegating.descriptor.clone();
    delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (first_provider, _) =
        run_child(&mut delegating.hosted.codex, codex_selection("high")).await;
    settle_callers_turn(
        &delegating,
        "the caller's Turn settles while its Subagent works",
    )
    .await;
    settle_session(&descriptor, delegating.caller, true).await;
    assert!(settled_at(&descriptor, delegating.caller).await.is_some());

    first_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    next_turn(
        &mut delegating.caller_provider,
        "the Report wakes the parent's Provider",
    )
    .await
    .succeed();
    assert_eq!(
        settled_at(&descriptor, delegating.caller).await,
        None,
        "a Report that wakes a settled Session makes it active again"
    );
    delegating
        .caller_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    read_until(
        &descriptor,
        delegating.caller,
        "the Continuation settles",
        |snapshot| snapshot.working_since().is_none(),
    )
    .await;

    // A Report held for want of a Provider process makes it active too.
    let second = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (second_provider, _) =
        run_child(&mut delegating.hosted.codex, codex_selection("high")).await;
    settle_session(&descriptor, delegating.caller, true).await;
    let Delegating {
        hosted,
        caller,
        caller_provider,
        handoff,
        ..
    } = delegating;
    drop(caller_provider);
    timeout(PROGRESS_DEADLINE, async {
        while McpClient::handed(&handoff).initialize_status().await != StatusCode::UNAUTHORIZED {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the parent's Provider connection is gone");
    second_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    row_settles(&descriptor, caller, second).await;
    assert_eq!(
        settled_at(&descriptor, caller).await,
        None,
        "a Report held for the parent's next Turn makes it active again as it arrives"
    );

    hosted.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_subagent_the_user_stops_on_its_own_is_reported_stopped() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-report-stopped", None).await;
    let descriptor = delegating.descriptor.clone();
    let child_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (mut child_provider, _) =
        run_child(&mut delegating.hosted.codex, codex_selection("high")).await;
    say(
        &descriptor,
        child_id,
        &child_provider,
        "Halfway through the seams.",
    )
    .await;

    // The user stops the Subagent alone, from its own Session.
    tokio::join!(
        interrupt(&descriptor, child_id),
        take_interrupt(&mut child_provider, "the Subagent's own"),
    );
    assert!(
        delegating.caller_provider.try_next_interrupt().is_none(),
        "the stop reaches the Subagent alone"
    );
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnInterrupted)
        .await;

    let steer = timeout(PROGRESS_DEADLINE, delegating.caller_provider.next_steer())
        .await
        .expect("the stop is reported to the Agent that delegated the work");
    let caller = row_settles(&descriptor, delegating.caller, child_id).await;
    assert_eq!(
        steer.reports(),
        [report_of(
            &caller,
            child_id,
            SubagentReportOutcome::Stopped,
            Some("Halfway through the seams.")
        )],
        "as stopped, since its parent planned on the result"
    );
    steer.succeed();

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn interrupting_the_parent_reports_nothing_of_the_subagents_it_stopped() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-report-interrupt", None).await;
    let descriptor = delegating.descriptor.clone();
    let stopped = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (mut stopped_provider, _) =
        run_child(&mut delegating.hosted.codex, codex_selection("high")).await;

    tokio::join!(
        interrupt(&descriptor, delegating.caller),
        take_interrupt(&mut delegating.caller_provider, "the parent's"),
        take_interrupt(&mut stopped_provider, "the Subagent's own"),
    );
    stopped_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnInterrupted)
        .await;
    delegating
        .caller_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnInterrupted)
        .await;
    let interrupted = row_settles(&descriptor, delegating.caller, stopped).await;
    assert_eq!(
        row_status(&interrupted, stopped).0,
        ActivityStatus::Interrupted
    );

    // A Subagent spawned after the interrupt settles on its own, and its
    // Report is the first thing the parent's Agent hears: none came before it
    // for the one the interrupt stopped.
    let after = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (after_provider, _) =
        run_child(&mut delegating.hosted.codex, codex_selection("high")).await;
    after_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    let woken = next_turn(
        &mut delegating.caller_provider,
        "the later Subagent's Report wakes the parent",
    )
    .await;
    assert_eq!(
        woken
            .reports()
            .iter()
            .map(|report| report.subagent)
            .collect::<Vec<_>>(),
        [after],
        "the Subagent the parent's interrupt stopped reports nothing"
    );
    assert!(delegating.caller_provider.try_next_steer().is_none());
    woken.succeed();

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}
