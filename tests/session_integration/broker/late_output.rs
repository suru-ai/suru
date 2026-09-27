//! Output owed to brokered Subagents. A Turn settles at its Provider's own
//! boundary while the brokered Subagents it spawned work on (ADR 0015, 0035),
//! and what the delegating Agent's Provider sends after that — while one of
//! them still works, or once one has settled — is owed to them: it begins a
//! Continuation of the delegating Session, which settles at that Provider's
//! next boundary like any Turn, rather than being discarded as stray. An
//! interrupted Turn's trailing stream owes nothing to the Subagents the
//! interrupt stopped with it, so it is discarded as it always was.

use super::stops::{acknowledge_interrupt, interrupt};
use super::*;

/// What the parent's Agent says once its Turn has settled.
const LATE: &str = "The Researcher's survey is in: folding its findings into the plan.";

/// Has `provider` write `text` as one whole Agent Message, with no Turn
/// active to hold it.
async fn say_late(provider: &ControlledProviderSession, text: &str) {
    for event in [
        ProviderEvent::AgentMessageStarted,
        ProviderEvent::AgentMessageDelta {
            content: text.to_owned(),
        },
        ProviderEvent::AgentMessageCompleted,
    ] {
        provider.emit_and_wait_until_observed(event).await;
    }
}

/// A brokered Subagent on Codex working on its own Provider beneath the
/// Claude Session `delegating` holds, whose own Turn has settled at its
/// Provider's boundary.
async fn settle_parent_while_child_works(
    delegating: &mut Delegating,
) -> (SessionId, ControlledProviderSession) {
    let (child_id, child_provider) = spawn_working_child(delegating).await;
    delegating
        .caller_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    let settled = read_until(
        &delegating.descriptor,
        delegating.caller,
        "the parent's Turn settles while its brokered Subagent works",
        |snapshot| snapshot.turns[0].status == TurnStatus::Completed,
    )
    .await;
    assert!(settled.working_since().is_some());
    (child_id, child_provider)
}

/// The Continuation `snapshot` holds `text` in: its latest Turn, begun by
/// neither a Prompt nor a Delegation.
fn continuation_holding<'a>(snapshot: &'a SessionSnapshot, text: &str) -> &'a suru::protocol::Turn {
    let message = snapshot
        .messages
        .iter()
        .find(|message| message.content == text)
        .unwrap_or_else(|| panic!("the Transcript holds the late Message: {snapshot:?}"));
    assert_eq!(message.role, MessageRole::Agent);
    assert_eq!(
        snapshot.turns.len(),
        2,
        "the late output began one new Turn: {:?}",
        snapshot.turns
    );
    let continuation = &snapshot.turns[1];
    assert!(
        continuation.is_continuation(),
        "a Continuation, begun by neither a Prompt nor a Delegation: {continuation:?}"
    );
    assert_eq!(
        message.turn_id, continuation.id,
        "the late Message belongs to the Continuation"
    );
    continuation
}

#[tokio::test]
async fn late_output_owed_by_the_parent_after_its_brokered_subagent_settles_begins_a_continuation()
{
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-late-output-settled", None).await;
    let descriptor = delegating.descriptor.clone();
    let (child_id, child_provider) = settle_parent_while_child_works(&mut delegating).await;

    child_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    read_until(
        &descriptor,
        delegating.caller,
        "the Subagent's row settles with its Turn",
        |snapshot| row_status(snapshot, child_id).0 == ActivityStatus::Completed,
    )
    .await;

    // The parent's Provider works on for the Subagent it heard settle.
    say_late(&delegating.caller_provider, LATE).await;
    let parent = read_until(
        &descriptor,
        delegating.caller,
        "the late output lands in a Continuation",
        |snapshot| {
            snapshot
                .messages
                .iter()
                .any(|message| message.content == LATE)
        },
    )
    .await;
    let continuation = continuation_holding(&parent, LATE);
    assert_eq!(continuation.status, TurnStatus::Active);
    assert!(
        continuation.started_at.is_some(),
        "a Continuation records when it began, like any Turn"
    );
    assert!(
        parent.working_since().is_some(),
        "the parent works again in it"
    );

    // Its Provider's next boundary settles it like any Turn.
    delegating
        .caller_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    let settled = read_until(
        &descriptor,
        delegating.caller,
        "the Continuation settles at its Provider's boundary",
        |snapshot| snapshot.turns[1].status != TurnStatus::Active,
    )
    .await;
    assert_eq!(settled.turns[1].status, TurnStatus::Completed);
    assert!(settled.turns[1].settled_at.is_some());
    assert_eq!(
        settled.working_since(),
        None,
        "nothing in the tree works any more"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn late_output_owed_by_the_parent_while_its_brokered_subagent_works_begins_a_continuation() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-late-output-working", None).await;
    let descriptor = delegating.descriptor.clone();
    let (child_id, _child_provider) = settle_parent_while_child_works(&mut delegating).await;
    let working_since = read_session(&descriptor, delegating.caller)
        .await
        .working_since()
        .expect("the parent reads Working while its brokered Subagent works");

    say_late(&delegating.caller_provider, LATE).await;
    let parent = read_until(
        &descriptor,
        delegating.caller,
        "the late output lands in a Continuation",
        |snapshot| {
            snapshot
                .messages
                .iter()
                .any(|message| message.content == LATE)
        },
    )
    .await;
    assert_eq!(
        continuation_holding(&parent, LATE).status,
        TurnStatus::Active
    );
    assert_eq!(
        parent.working_since(),
        Some(working_since),
        "a Continuation overlapping the working Subagent does not reset Working"
    );

    delegating
        .caller_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    let settled = read_until(
        &descriptor,
        delegating.caller,
        "the Continuation settles at its Provider's boundary",
        |snapshot| snapshot.turns[1].status != TurnStatus::Active,
    )
    .await;
    assert_eq!(settled.turns[1].status, TurnStatus::Completed);
    assert_eq!(
        row_status(&settled, child_id).0,
        ActivityStatus::Active,
        "the Subagent works on past it"
    );
    assert_eq!(
        settled.working_since(),
        Some(working_since),
        "and the parent's Working runs on uninterrupted"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

/// Has `provider`, the Provider of `session_id`, start a Watch — never output
/// owed a Continuation — and reads the Session once it shows, by which time
/// whatever the stream carried before it has landed wherever it was going to.
async fn after_a_watch_starts(
    descriptor: &RuntimeDescriptor,
    session_id: SessionId,
    provider: &ControlledProviderSession,
    description: &str,
) -> SessionSnapshot {
    provider
        .emit_and_wait_until_observed(ProviderEvent::WatchStarted {
            watch_id: suru::provider::ProviderWatchId::new(description),
            description: description.to_owned(),
        })
        .await;
    read_until(descriptor, session_id, "the Watch shows", |snapshot| {
        snapshot
            .watches
            .iter()
            .any(|watch| watch.description == description)
    })
    .await
}

/// Asserts the trailing output `snapshot`'s Provider sent landed nowhere: its
/// Turns are still the ones it had, `settled` — no Continuation began for the
/// output — and no Message holds it.
fn discarded(snapshot: &SessionSnapshot, settled: &[TurnStatus]) {
    assert_eq!(
        snapshot
            .turns
            .iter()
            .map(|turn| turn.status)
            .collect::<Vec<_>>(),
        settled,
        "no Continuation begins for the trailing stream"
    );
    assert!(
        !snapshot
            .messages
            .iter()
            .any(|message| message.content.starts_with("Trailing words")),
        "the trailing output is discarded: {:?}",
        snapshot.messages
    );
}

#[tokio::test]
async fn an_interrupted_parents_trailing_output_begins_no_continuation_for_the_brokered_subagent_stopped_with_it()
 {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-late-output-interrupted", None).await;
    let descriptor = delegating.descriptor.clone();
    let (stopped_id, mut stopped_provider) = spawn_working_child(&mut delegating).await;

    let (response, (), ()) = tokio::join!(
        interrupt(&descriptor, delegating.caller),
        acknowledge_interrupt(&mut stopped_provider, "the Subagent's own"),
        acknowledge_interrupt(&mut delegating.caller_provider, "the parent's own"),
    );
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    delegating
        .caller_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnInterrupted)
        .await;
    read_until(
        &descriptor,
        delegating.caller,
        "the parent's Turn settles interrupted",
        |snapshot| snapshot.turns[0].status == TurnStatus::Interrupted,
    )
    .await;

    // The interrupted stream trails on, first while the Subagent stopped
    // with it has yet to reach its own boundary, and again once it has.
    say_late(&delegating.caller_provider, "Trailing words, before.").await;
    discarded(
        &after_a_watch_starts(
            &descriptor,
            delegating.caller,
            &delegating.caller_provider,
            "tail the build log",
        )
        .await,
        &[TurnStatus::Interrupted],
    );
    stopped_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnInterrupted)
        .await;
    read_until(
        &descriptor,
        delegating.caller,
        "the stopped Subagent's row settles with its Turn",
        |snapshot| row_status(snapshot, stopped_id).0 == ActivityStatus::Interrupted,
    )
    .await;
    say_late(&delegating.caller_provider, "Trailing words, after.").await;
    discarded(
        &after_a_watch_starts(
            &descriptor,
            delegating.caller,
            &delegating.caller_provider,
            "tail the test log",
        )
        .await,
        &[TurnStatus::Interrupted],
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}
