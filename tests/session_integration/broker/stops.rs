//! Stopping brokered Subagents. Suru runs a brokered Subagent on a Provider
//! actor of its own (ADR 0035), so whatever stops one reaches that actor: the
//! delegating Agent's `stop_subagent`, the client's stop of the one Subagent
//! from its row — whatever its parent's Provider allows for Subagents of its
//! own — and an interrupt of any Session above it, which carries down to
//! everything beneath. None of them asks before acting.

use suru::provider::{ProviderWatchId, ProviderWatchOutcome};

use super::*;

/// Asks the Server to interrupt `session_id`, as a client does — the Picker
/// row's stop of one Subagent included — answering with the raw response.
async fn interrupt(descriptor: &RuntimeDescriptor, session_id: SessionId) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!(
            "{}/v1/sessions/{session_id}/interrupt",
            descriptor.base_url
        ))
        .bearer_auth(&descriptor.token)
        .send()
        .await
        .expect("send Session interruption")
}

/// Takes up the interrupt `provider` is asked for, as its Provider does.
async fn acknowledge_interrupt(provider: &mut ControlledProviderSession, whose: &str) {
    timeout(PROGRESS_DEADLINE, provider.next_interrupt())
        .await
        .unwrap_or_else(|_| panic!("the interrupt reaches {whose} Provider"))
        .succeed();
}

/// Reads `session_id` until its first Turn settles, answering how it did.
async fn first_turn_settled(descriptor: &RuntimeDescriptor, session_id: SessionId) -> TurnStatus {
    read_until(
        descriptor,
        session_id,
        "its first Turn settles",
        |snapshot| snapshot.turns[0].status != TurnStatus::Active,
    )
    .await
    .turns[0]
        .status
}

/// Reads `holder` until its row for `child` settles, answering how it did.
async fn row_settled(
    descriptor: &RuntimeDescriptor,
    holder: SessionId,
    child: SessionId,
) -> (ActivityStatus, Option<u64>) {
    let settled = read_until(
        descriptor,
        holder,
        "the Subagent's row settles",
        |snapshot| row_status(snapshot, child).0 != ActivityStatus::Active,
    )
    .await;
    row_status(&settled, child)
}

#[tokio::test]
async fn interrupting_a_parent_whose_only_work_is_its_brokered_subagents_stops_them_and_clears_working()
 {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-interrupt-subagents", None).await;
    let descriptor = delegating.descriptor.clone();
    let child_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (mut child_provider, _) =
        run_child(&mut delegating.hosted.codex, codex_selection("high")).await;
    delegating
        .caller_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    read_until(
        &descriptor,
        delegating.caller,
        "the caller's Turn settles while its brokered Subagent works",
        |snapshot| {
            snapshot.turns[0].status == TurnStatus::Completed && snapshot.working_since().is_some()
        },
    )
    .await;

    let (response, ()) = tokio::join!(
        interrupt(&descriptor, delegating.caller),
        acknowledge_interrupt(&mut child_provider, "the brokered Subagent's own"),
    );
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    child_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnInterrupted)
        .await;
    assert_eq!(
        first_turn_settled(&descriptor, child_id).await,
        TurnStatus::Interrupted
    );
    let idle = read_until(
        &descriptor,
        delegating.caller,
        "Working clears once the Subagent has stopped",
        |snapshot| snapshot.working_since().is_none(),
    )
    .await;
    assert_eq!(
        row_status(&idle, child_id).0,
        ActivityStatus::Interrupted,
        "the row reads Stopped"
    );
    assert_eq!(idle.session.status, SessionStatus::Idle);
    assert_eq!(
        idle.turns[0].status,
        TurnStatus::Completed,
        "the Turn that settled before the interrupt keeps its outcome"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn interrupting_a_parents_turn_stops_its_brokered_subagents_along_with_it() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-interrupt-turn", None).await;
    let descriptor = delegating.descriptor.clone();
    let child_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (mut child_provider, _) =
        run_child(&mut delegating.hosted.codex, codex_selection("high")).await;

    let (response, (), ()) = tokio::join!(
        interrupt(&descriptor, delegating.caller),
        acknowledge_interrupt(&mut delegating.caller_provider, "the parent's own"),
        acknowledge_interrupt(&mut child_provider, "the brokered Subagent's own"),
    );
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    delegating
        .caller_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnInterrupted)
        .await;
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnInterrupted)
        .await;
    assert_eq!(
        first_turn_settled(&descriptor, delegating.caller).await,
        TurnStatus::Interrupted
    );
    assert_eq!(
        first_turn_settled(&descriptor, child_id).await,
        TurnStatus::Interrupted
    );
    assert_eq!(
        row_settled(&descriptor, delegating.caller, child_id)
            .await
            .0,
        ActivityStatus::Interrupted
    );
    let idle = read_until(
        &descriptor,
        delegating.caller,
        "nothing beneath the parent works on",
        |snapshot| snapshot.working_since().is_none(),
    )
    .await;
    assert_eq!(idle.session.status, SessionStatus::Idle);

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn interrupting_a_monitoring_parent_stops_the_watches_its_brokered_subagent_left_running_through_the_subagents_own_provider()
 {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-interrupt-watches", None).await;
    let descriptor = delegating.descriptor.clone();
    let child_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (mut child_provider, _) =
        run_child(&mut delegating.hosted.codex, codex_selection("high")).await;
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::WatchStarted {
            watch_id: ProviderWatchId::new("watch-tests"),
            description: "cargo test".to_owned(),
        })
        .await;
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    delegating
        .caller_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    read_until(
        &descriptor,
        delegating.caller,
        "the parent is Monitoring the Watch its brokered Subagent left running",
        |snapshot| snapshot.working_since().is_none() && snapshot.monitoring_since().is_some(),
    )
    .await;

    let (response, ()) = tokio::join!(interrupt(&descriptor, delegating.caller), async {
        let stop = timeout(PROGRESS_DEADLINE, child_provider.next_watches_stop())
            .await
            .expect("the Watch stop reaches the Provider that runs the Watch");
        assert_eq!(stop.watches(), ["watch-tests"]);
        stop.succeed();
    });
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(
        delegating.caller_provider.try_next_watches_stop().is_none(),
        "the parent's own Provider runs none of them, so it is asked to stop none"
    );

    child_provider
        .emit_and_wait_until_observed(ProviderEvent::WatchSettled {
            watch_id: ProviderWatchId::new("watch-tests"),
            outcome: ProviderWatchOutcome::Stopped,
            summary: None,
            woke_agent: false,
        })
        .await;
    let idle = read_until(
        &descriptor,
        delegating.caller,
        "Monitoring ends once the Watch settles",
        |snapshot| snapshot.monitoring_since().is_none(),
    )
    .await;
    assert_eq!(
        row_status(&idle, child_id).0,
        ActivityStatus::Completed,
        "the Subagent's settled row keeps its outcome: only its Watch was stopped"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn the_clients_stop_of_a_brokered_subagent_reaches_its_own_provider_whatever_its_parents_provider_allows()
 {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-stop-from-row", None).await;
    let descriptor = delegating.descriptor.clone();
    // The parent's Provider offers no stop of one of its own Subagents, as
    // Copilot does not.
    delegating.hosted.runtimes[0].withdraw_subagent_stop();
    let child_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (mut child_provider, _) =
        run_child(&mut delegating.hosted.codex, codex_selection("high")).await;

    let caller = read_session(&descriptor, delegating.caller).await;
    let Activity::Subagent { brokered, .. } = row_for(&caller, child_id) else {
        unreachable!()
    };
    assert!(
        *brokered,
        "the row says Suru spawned the Subagent through the Broker, which is what offers its \
         stop whatever the parent's Provider allows"
    );

    // The client's stop of one Subagent is an interrupt of its own Session.
    let (response, ()) = tokio::join!(
        interrupt(&descriptor, child_id),
        acknowledge_interrupt(&mut child_provider, "the Subagent's own"),
    );
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(
        delegating
            .caller_provider
            .try_next_subagent_stop()
            .is_none()
            && delegating.caller_provider.try_next_interrupt().is_none(),
        "the parent's Provider is asked nothing of a Subagent it does not run"
    );

    child_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnInterrupted)
        .await;
    assert_eq!(
        row_settled(&descriptor, delegating.caller, child_id)
            .await
            .0,
        ActivityStatus::Interrupted
    );
    let caller = read_session(&descriptor, delegating.caller).await;
    assert_eq!(caller.turns[0].status, TurnStatus::Active);

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}
