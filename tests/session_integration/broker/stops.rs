//! Stopping brokered Subagents. Suru runs a brokered Subagent on a Provider
//! actor of its own (ADR 0035), so whatever stops one reaches that actor: the
//! delegating Agent's `stop_subagent`, the client's stop of the one Subagent
//! from its row — whatever its parent's Provider allows for Subagents of its
//! own — and an interrupt of any Session above it, which carries down to
//! everything beneath. None of them asks before acting.

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
