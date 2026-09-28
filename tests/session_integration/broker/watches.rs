//! Watches in a tree with brokered Subagents. A Watch belongs to the Session
//! whose Agent started it and runs in that Agent's Provider process, and a
//! brokered Subagent runs on a Provider actor of its own (ADR 0035), so each
//! Watch is stopped through the actor that owns it: stopping one Session's
//! Watches never reaches another Provider's through the wrong connection.
//! Interrupting a Monitoring parent stops the Watches its brokered Subagent
//! left running through that Subagent's own Provider — see
//! `stops::interrupting_a_monitoring_parent_stops_the_watches_its_brokered_subagent_left_running_through_the_subagents_own_provider`;
//! what follows is the rest of the tree's Watches.

use suru::provider::{ProviderWatchId, ProviderWatchOutcome};

use super::stops::interrupt;
use super::*;

fn watch_started(watch: &str, description: &str) -> ProviderEvent {
    ProviderEvent::WatchStarted {
        watch_id: ProviderWatchId::new(watch),
        description: description.to_owned(),
    }
}

/// A Watch settling the way one Suru stopped does: with nothing woken.
fn watch_stopped(watch: &str) -> ProviderEvent {
    ProviderEvent::WatchSettled {
        watch_id: ProviderWatchId::new(watch),
        outcome: ProviderWatchOutcome::Stopped,
        summary: None,
        woke_agent: false,
    }
}

/// The descriptions of the Watches live in `snapshot`'s subtree, as a reader
/// of the Session is told what it is Monitoring.
fn watched(snapshot: &SessionSnapshot) -> Vec<&str> {
    snapshot
        .watches
        .iter()
        .map(|watch| watch.description.as_str())
        .collect()
}

/// A brokered Subagent on Codex beneath the Claude Session `delegating`
/// holds, each of them settled with a Watch of its own left running on its
/// own Provider, so the parent is Monitoring both.
async fn monitor_one_watch_each(
    delegating: &mut Delegating,
) -> (SessionId, ControlledProviderSession) {
    let (child_id, child_provider) = spawn_working_child(delegating).await;
    delegating
        .caller_provider
        .emit_and_wait_until_observed(watch_started("watch-log", "tail the build log"))
        .await;
    child_provider
        .emit_and_wait_until_observed(watch_started("watch-tests", "cargo test"))
        .await;
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    // The Subagent's Report steers the parent's Turn, still working.
    steered_by_the_report(&mut delegating.caller_provider, child_id).await;
    delegating
        .caller_provider
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    let parent = read_until(
        &delegating.descriptor,
        delegating.caller,
        "the parent is Monitoring its own Watch and its brokered Subagent's",
        |snapshot| {
            snapshot.working_since().is_none()
                && snapshot.monitoring_since().is_some()
                && snapshot.watches.len() == 2
        },
    )
    .await;
    let mut descriptions = watched(&parent);
    descriptions.sort_unstable();
    assert_eq!(descriptions, ["cargo test", "tail the build log"]);
    let child = read_session(&delegating.descriptor, child_id).await;
    assert_eq!(
        watched(&child),
        ["cargo test"],
        "the Subagent's Session Monitors only the Watch its own Agent started"
    );
    (child_id, child_provider)
}

#[tokio::test]
async fn a_watch_a_brokered_subagent_started_is_stopped_through_its_own_provider_and_the_parents_watches_run_on()
 {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-watches-child", None).await;
    let descriptor = delegating.descriptor.clone();
    let (child_id, mut child_provider) = monitor_one_watch_each(&mut delegating).await;

    let (response, ()) = tokio::join!(interrupt(&descriptor, child_id), async {
        let stop = timeout(PROGRESS_DEADLINE, child_provider.next_watches_stop())
            .await
            .expect("the Watch stop reaches the Subagent's own Provider");
        assert_eq!(
            stop.watches(),
            ["watch-tests"],
            "naming the Subagent's Watch alone"
        );
        stop.succeed();
    });
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(
        delegating.caller_provider.try_next_watches_stop().is_none(),
        "the parent's Provider is asked to stop nothing"
    );

    child_provider
        .emit_and_wait_until_observed(watch_stopped("watch-tests"))
        .await;
    read_until(
        &descriptor,
        child_id,
        "the Subagent stops Monitoring once its Watch settles",
        |snapshot| snapshot.monitoring_since().is_none(),
    )
    .await;
    let parent = read_session(&descriptor, delegating.caller).await;
    assert!(
        parent.monitoring_since().is_some(),
        "the parent still Monitors its own Watch"
    );
    assert_eq!(watched(&parent), ["tail the build log"]);
    assert!(
        !parent
            .activities
            .iter()
            .chain(read_session(&descriptor, child_id).await.activities.iter())
            .any(|activity| matches!(activity, Activity::WatchOutcome { .. })),
        "a stopped Watch wakes nothing, so it leaves no Watch Outcome anywhere"
    );

    // Interrupting the parent now stops the one Watch still live, through
    // the Provider that runs it, and asks the Subagent's Provider nothing.
    let (response, ()) = tokio::join!(interrupt(&descriptor, delegating.caller), async {
        let stop = timeout(
            PROGRESS_DEADLINE,
            delegating.caller_provider.next_watches_stop(),
        )
        .await
        .expect("the Watch stop reaches the parent's own Provider");
        assert_eq!(stop.watches(), ["watch-log"]);
        stop.succeed();
    });
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(
        child_provider.try_next_watches_stop().is_none(),
        "the Subagent's Provider runs no Watch still live, so it is asked to stop none"
    );
    delegating
        .caller_provider
        .emit_and_wait_until_observed(watch_stopped("watch-log"))
        .await;
    read_until(
        &descriptor,
        delegating.caller,
        "the parent stops Monitoring once its Watch settles",
        |snapshot| snapshot.monitoring_since().is_none(),
    )
    .await;

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}

#[tokio::test]
async fn a_watch_that_wakes_a_settled_brokered_subagent_records_its_outcome_in_the_subagents_own_continuation()
 {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let mut delegating = delegating(state_dir.path(), "broker-watches-wake", None).await;
    let descriptor = delegating.descriptor.clone();
    let (child_id, child_provider) = monitor_one_watch_each(&mut delegating).await;

    // The Subagent's Watch settles and wakes the Subagent's own Agent, which
    // works on for it.
    child_provider
        .emit_and_wait_until_observed(ProviderEvent::WatchSettled {
            watch_id: ProviderWatchId::new("watch-tests"),
            outcome: ProviderWatchOutcome::Completed,
            summary: Some("Tests passed".to_owned()),
            woke_agent: true,
        })
        .await;
    write_agent_message(&child_provider, "The tests pass.").await;
    let child = read_until(
        &descriptor,
        child_id,
        "the woken Subagent works in a Continuation of its own Session",
        |snapshot| snapshot.turns.len() == 2,
    )
    .await;
    let continuation = &child.turns[1];
    assert!(continuation.is_continuation());
    let outcomes = child
        .activities
        .iter()
        .filter(|activity| matches!(activity, Activity::WatchOutcome { .. }))
        .collect::<Vec<_>>();
    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    assert_eq!(
        outcomes[0].turn_id(),
        continuation.id,
        "the Watch Outcome stands in the Continuation its settling woke the Subagent into"
    );
    assert_eq!(
        child.transcript.first(),
        Some(&TranscriptItem::Message {
            message_id: child.messages[0].id
        }),
        "after the Delegation that opened the Subagent's Session"
    );
    let parent = read_session(&descriptor, delegating.caller).await;
    assert_eq!(
        parent.turns.len(),
        1,
        "the parent's Session gains no Turn of its own"
    );
    assert!(
        !parent
            .activities
            .iter()
            .any(|activity| matches!(activity, Activity::WatchOutcome { .. })),
        "nor the Watch Outcome"
    );
    assert_eq!(
        watched(&parent),
        ["tail the build log"],
        "and still Monitors its own Watch"
    );

    delegating
        .hosted
        .server
        .shutdown()
        .await
        .expect("shut down server");
}
