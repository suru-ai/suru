//! Stopping brokered Subagents. Suru runs a brokered Subagent on a Provider
//! actor of its own (ADR 0035), so whatever stops one reaches that actor: the
//! delegating Agent's `stop_subagent`, the client's stop of the one Subagent
//! from its row — whatever its parent's Provider allows for Subagents of its
//! own — and an interrupt of any Session above it, which carries down to
//! everything beneath. None of them asks before acting.

use suru::provider::{ProviderSubagentId, ProviderWatchId, ProviderWatchOutcome};

use super::*;

impl McpClient {
    /// `stop_subagent`'s answer for `subagent`, read from the structured
    /// content the call carries.
    pub(super) async fn stop_subagent(&mut self, subagent: SessionId) -> Value {
        let result = self
            .call_tool("stop_subagent", json!({ "id": subagent }))
            .await;
        assert_ne!(
            result["isError"],
            json!(true),
            "stop_subagent answers: {result}"
        );
        result["structuredContent"].clone()
    }
}

/// The brokered Subagent's own Provider, started on `provider`'s double, with
/// the Broker handoff its start carried, and its first Turn taken up — for a
/// Subagent that delegates in turn.
async fn run_handed_child(
    provider: &mut ControlledProvider,
    selection: AgentSelection,
) -> (ControlledProviderSession, BrokerHandoff) {
    let start = next_start(provider).await;
    let handoff = start
        .broker()
        .cloned()
        .expect("a brokered Subagent is handed the Broker too");
    let mut child = start.succeed(AgentIdentity {
        agent: AgentId::new(format!("{}-agent", selection.provider)),
        selection,
    });
    timeout(PROGRESS_DEADLINE, child.next_turn())
        .await
        .expect("the Delegation reaches the Subagent's Provider")
        .succeed();
    (child, handoff)
}

/// The Agent Selection a Claude Subagent on Haiku runs under.
fn haiku_selection() -> AgentSelection {
    AgentSelection {
        provider: ProviderId::new("claude"),
        model: ModelId::new("haiku"),
        options: Vec::new(),
    }
}

/// Asks the Server to interrupt `session_id`, as a client does — the Picker
/// row's stop of one Subagent included — answering with the raw response.
pub(super) async fn interrupt(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
) -> reqwest::Response {
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
pub(super) async fn acknowledge_interrupt(provider: &mut ControlledProviderSession, whose: &str) {
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
async fn stop_subagent_settles_a_brokered_subagents_turn_stopped_and_its_row_with_it() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-stop-subagent", None).await;
    let descriptor = delegating.descriptor.clone();
    let child_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (mut child_provider, _) =
        run_child(&mut delegating.hosted.codex, codex_selection("high")).await;

    let (answer, ()) = tokio::join!(
        delegating.client.stop_subagent(child_id),
        acknowledge_interrupt(&mut child_provider, "the Subagent's own"),
    );
    assert_eq!(
        answer,
        json!({ "session_id": child_id, "stopped": true }),
        "the Agent is told the Subagent was stopped"
    );
    assert!(
        delegating.caller_provider.try_next_interrupt().is_none(),
        "the stop reaches the Subagent alone, never the Turn that spawned it"
    );

    // The Subagent's Turn settles at its Provider's own boundary, as any
    // interrupted Turn does, and the row follows it.
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnInterrupted)
        .await;
    assert_eq!(
        first_turn_settled(&descriptor, child_id).await,
        TurnStatus::Interrupted
    );
    let (status, duration_ms) = row_settled(&descriptor, delegating.caller, child_id).await;
    assert_eq!(status, ActivityStatus::Interrupted, "the row reads Stopped");
    assert!(
        duration_ms.is_some(),
        "a stop is a real settle, so the row says how long the Subagent worked"
    );
    let caller = read_session(&descriptor, delegating.caller).await;
    assert_eq!(
        caller.turns[0].status,
        TurnStatus::Active,
        "the caller's own Turn works on"
    );

    let again = delegating.client.stop_subagent(child_id).await;
    assert_eq!(again["session_id"], json!(child_id));
    assert_eq!(
        again["stopped"],
        json!(false),
        "a Subagent that has settled has nothing left to stop"
    );
    assert!(
        again["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("not working")),
        "and the Agent is told why rather than refused: {again}"
    );
    assert!(child_provider.try_next_interrupt().is_none());

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn stop_subagent_on_a_settled_subagent_stops_only_the_watches_it_left_running() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-stop-watches", None).await;
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
    read_until(
        &descriptor,
        child_id,
        "the Subagent settles, Monitoring the Watch it left running",
        |snapshot| snapshot.working_since().is_none() && snapshot.monitoring_since().is_some(),
    )
    .await;

    let (answer, ()) = tokio::join!(delegating.client.stop_subagent(child_id), async {
        let stop = timeout(PROGRESS_DEADLINE, child_provider.next_watches_stop())
            .await
            .expect("the Watch stop reaches the Subagent's own Provider");
        assert_eq!(stop.watches(), ["watch-tests"]);
        stop.succeed();
    });
    assert_eq!(answer["session_id"], json!(child_id));
    assert_eq!(
        answer["stopped"],
        json!(false),
        "the Subagent's work had settled, so it is not said to be stopped: {answer}"
    );
    assert!(
        answer["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("Watches it left running")),
        "the Agent is told only its Watches were stopped: {answer}"
    );
    assert!(
        child_provider.try_next_interrupt().is_none(),
        "no Turn was working to interrupt"
    );
    let caller = read_session(&descriptor, delegating.caller).await;
    assert_eq!(
        row_status(&caller, child_id).0,
        ActivityStatus::Completed,
        "the row stays as it settled"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn stop_subagent_on_a_settled_subagent_stops_only_what_it_delegated_that_still_works() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-stop-delegated", None).await;
    let descriptor = delegating.descriptor.clone();
    let child_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (mut child_provider, child_handoff) =
        run_handed_child(&mut delegating.hosted.codex, codex_selection("high")).await;
    let mut child_client = McpClient::handed(&child_handoff);
    child_client.initialize().await;
    let grandchild_id = child_client
        .spawn_subagent(researcher("claude", "haiku", json!({})))
        .await;
    let (mut grandchild_provider, _) =
        run_child(&mut delegating.hosted.claude, haiku_selection()).await;
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    assert_eq!(
        row_settled(&descriptor, delegating.caller, child_id)
            .await
            .0,
        ActivityStatus::Completed
    );

    let (answer, ()) = tokio::join!(
        delegating.client.stop_subagent(child_id),
        acknowledge_interrupt(
            &mut grandchild_provider,
            "the Subagent it delegated to's own"
        ),
    );
    assert_eq!(answer["session_id"], json!(child_id));
    assert_eq!(
        answer["stopped"],
        json!(false),
        "the Subagent's own work had settled, so it is not said to be stopped: {answer}"
    );
    assert!(
        answer["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("The Subagents it delegated to")),
        "the Agent is told what was stopped instead: {answer}"
    );
    assert!(
        child_provider.try_next_interrupt().is_none(),
        "the Subagent had no Turn working to interrupt"
    );

    grandchild_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnInterrupted)
        .await;
    assert_eq!(
        row_settled(&descriptor, child_id, grandchild_id).await.0,
        ActivityStatus::Interrupted
    );
    let caller = read_session(&descriptor, delegating.caller).await;
    assert_eq!(
        row_status(&caller, child_id).0,
        ActivityStatus::Completed,
        "the Subagent's own row stays as it settled"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn stop_subagent_refuses_anything_but_a_brokered_subagent_beneath_the_caller() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-stop-refusals", None).await;
    let descriptor = delegating.descriptor.clone();
    let workspace = delegating.hosted.workspace.path().to_owned();

    // A native Subagent the caller's own Provider spawned.
    delegating
        .caller_provider
        .emit_and_wait_until_observed(ProviderEvent::SubagentStarted {
            subagent_id: ProviderSubagentId::new("task-1"),
            name: "Explore".to_owned(),
            description: "Map the provider seams".to_owned(),
            delegation: None,
        })
        .await;
    let caller = read_until(
        &descriptor,
        delegating.caller,
        "the native Subagent's row opens",
        |snapshot| {
            snapshot
                .activities
                .iter()
                .any(|activity| matches!(activity, Activity::Subagent { .. }))
        },
    )
    .await;
    let Some(Activity::Subagent {
        session_id: native_id,
        brokered,
        ..
    }) = caller
        .activities
        .iter()
        .find(|activity| matches!(activity, Activity::Subagent { .. }))
    else {
        unreachable!()
    };
    assert!(
        !*brokered,
        "a row the caller's own Provider spawned says it is native"
    );
    let native_id = *native_id;

    // A brokered Subagent another Session's Agent spawned.
    let (_other, other_handoff, _other_provider) = start_session(
        &descriptor,
        &mut delegating.hosted.codex,
        &workspace,
        default_selection(&codex_models()),
    )
    .await;
    let mut other_client = McpClient::handed(&other_handoff);
    other_client.initialize().await;
    let foreign_id = other_client
        .spawn_subagent(researcher("claude", "haiku", json!({})))
        .await;
    let (mut foreign_provider, _) =
        run_child(&mut delegating.hosted.claude, haiku_selection()).await;

    for (id, refused) in [
        (json!(delegating.caller), "the caller's own Session"),
        (json!(native_id), "a native Subagent"),
        (
            json!(foreign_id),
            "a brokered Subagent beneath another Session",
        ),
        (json!(SessionId::new()), "a Session Suru does not hold"),
    ] {
        let refusal = delegating
            .client
            .refusal("stop_subagent", json!({ "id": id }))
            .await;
        assert!(
            refusal.contains("names no Subagent spawned through the Broker beneath you"),
            "{refused} is refused, saying which Subagents may be stopped: {refusal}"
        );
    }
    for (arguments, says) in [
        (json!({}), "needs `id`"),
        (json!({ "id": 7 }), "`id` must be the session_id"),
        (
            json!({ "id": "the researcher" }),
            "`id` must be the session_id",
        ),
        (
            json!({ "id": foreign_id, "force": true }),
            "takes no argument `force`",
        ),
    ] {
        let refusal = delegating.client.refusal("stop_subagent", arguments).await;
        assert!(refusal.contains(says), "{says:?} is said in {refusal:?}");
    }

    // The foreign Subagent's own spawner may stop it, which shows the refusals
    // above were about the caller rather than the Subagent.
    let (answer, ()) = tokio::join!(
        other_client.stop_subagent(foreign_id),
        acknowledge_interrupt(&mut foreign_provider, "the foreign Subagent's"),
    );
    assert_eq!(answer["stopped"], json!(true));
    assert!(
        delegating.caller_provider.try_next_interrupt().is_none()
            && delegating
                .caller_provider
                .try_next_subagent_stop()
                .is_none(),
        "nothing refused reached a Provider"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
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
    steered_by_the_report(&mut delegating.caller_provider, child_id).await;
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
async fn stopping_a_brokered_subagent_stops_whatever_it_delegated_in_turn() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-stop-subtree", None).await;
    let descriptor = delegating.descriptor.clone();
    let child_id = delegating
        .client
        .spawn_subagent(researcher("codex", "gpt-5.5", json!({})))
        .await;
    let (mut child_provider, child_handoff) =
        run_handed_child(&mut delegating.hosted.codex, codex_selection("high")).await;
    // The Subagent's own Agent delegates one level down, through the Broker.
    let mut child_client = McpClient::handed(&child_handoff);
    child_client.initialize().await;
    let grandchild_id = child_client
        .spawn_subagent(researcher("claude", "haiku", json!({})))
        .await;
    let (mut grandchild_provider, _) =
        run_child(&mut delegating.hosted.claude, haiku_selection()).await;

    let (answer, (), ()) = tokio::join!(
        delegating.client.stop_subagent(child_id),
        acknowledge_interrupt(&mut child_provider, "the Subagent's own"),
        acknowledge_interrupt(
            &mut grandchild_provider,
            "the Subagent it delegated to's own"
        ),
    );
    assert_eq!(answer, json!({ "session_id": child_id, "stopped": true }));

    child_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnInterrupted)
        .await;
    grandchild_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnInterrupted)
        .await;
    assert_eq!(
        first_turn_settled(&descriptor, grandchild_id).await,
        TurnStatus::Interrupted
    );
    assert_eq!(
        row_settled(&descriptor, child_id, grandchild_id).await.0,
        ActivityStatus::Interrupted,
        "the row in the Subagent's own Transcript reads Stopped"
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
    assert!(
        delegating.caller_provider.try_next_interrupt().is_none(),
        "the Session above the stopped Subagent works on"
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
