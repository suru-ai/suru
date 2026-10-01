//! Compactions at the provider-neutral seam: a Provider compacting a
//! conversation's context records a Compaction in the Turn it fell in — or in
//! a Continuation it begins when no Turn is active — in the Session whose
//! context it compacted, settling as the Provider reports or with its Turn,
//! and kept as history like any other Activity.

use crate::server_support::PROGRESS_DEADLINE;
use crate::support::{
    WorkingTurn, read_session, read_session_until, the_subagent_row, working_turn,
};
use suru::{
    protocol::{
        Activity, ActivityStatus, CompactionTrigger, SessionId, SessionSnapshot, TranscriptItem,
        TurnStatus,
    },
    provider::{ProviderEvent, ProviderEventAttribution, ProviderSubagentId},
    server::{self, ServerConfig},
};
use tokio::time::timeout;

fn completed(before_tokens: Option<u64>, after_tokens: Option<u64>) -> ProviderEvent {
    ProviderEvent::CompactionCompleted {
        before_tokens,
        after_tokens,
    }
}

fn failed(error: &str) -> ProviderEvent {
    ProviderEvent::CompactionFailed {
        error: Some(error.to_owned()),
    }
}

async fn session_where(
    fixture: &WorkingTurn,
    session_id: SessionId,
    described: &str,
    predicate: impl Fn(&SessionSnapshot) -> bool,
) -> SessionSnapshot {
    read_session_until(
        &fixture.client,
        fixture.server.descriptor(),
        session_id,
        described,
        predicate,
    )
    .await
}

fn compactions(snapshot: &SessionSnapshot) -> Vec<&Activity> {
    snapshot
        .activities
        .iter()
        .filter(|activity| matches!(activity, Activity::Compaction { .. }))
        .collect()
}

/// The status of each Compaction in `snapshot`, in Transcript order.
fn compaction_statuses(snapshot: &SessionSnapshot) -> Vec<ActivityStatus> {
    compactions(snapshot)
        .into_iter()
        .filter_map(Activity::status)
        .collect()
}

fn turn_settled(snapshot: &SessionSnapshot, turn: usize) -> bool {
    snapshot
        .turns
        .get(turn)
        .is_some_and(|turn| turn.status != TurnStatus::Active)
}

#[tokio::test]
async fn an_automatic_compaction_stands_active_in_its_turn_then_settles_with_the_providers_counts()
{
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "compaction-automatic-test").await;
    let session_id = fixture.session_id;

    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::CompactionStarted)
        .await;
    // A Provider restates that it is still compacting while it summarises;
    // that is the same occasion, not another.
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::CompactionStarted)
        .await;
    let compacting = session_where(&fixture, session_id, "the Compaction opens", |snapshot| {
        !compactions(snapshot).is_empty()
    })
    .await;
    assert_eq!(
        compactions(&compacting),
        vec![&Activity::Compaction {
            id: compactions(&compacting)[0].id(),
            turn_id: compacting.turns[0].id,
            status: ActivityStatus::Active,
            trigger: CompactionTrigger::Automatic,
            before_tokens: None,
            after_tokens: None,
            error: None,
        }],
        "one Compaction stands Active in the Turn it fell in"
    );

    fixture
        .provider_session
        .emit_and_wait_until_observed(completed(Some(182_000), Some(31_000)))
        .await;
    fixture.provider_session.emit(ProviderEvent::TurnCompleted);
    let settled = session_where(&fixture, session_id, "the Turn settles", |snapshot| {
        turn_settled(snapshot, 0)
    })
    .await;
    assert_eq!(
        compactions(&settled),
        vec![&Activity::Compaction {
            id: compactions(&compacting)[0].id(),
            turn_id: settled.turns[0].id,
            status: ActivityStatus::Completed,
            trigger: CompactionTrigger::Automatic,
            before_tokens: Some(182_000),
            after_tokens: Some(31_000),
            error: None,
        }],
        "the Compaction settles where it stood, with the Context Fill the Provider reported"
    );
    assert_eq!(
        settled.turns.len(),
        1,
        "an automatic Compaction begins no Turn"
    );
    assert_eq!(settled.turns[0].status, TurnStatus::Completed);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_failed_compaction_and_its_retry_stand_as_two_and_leave_the_turn_to_the_provider() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "compaction-failed-test").await;
    let session_id = fixture.session_id;

    fixture
        .provider_session
        .emit(ProviderEvent::CompactionStarted);
    fixture
        .provider_session
        .emit(failed("Conversation too long to summarise"));
    // The retry's completion arrives with no start before it, which records
    // a Compaction settled from the moment it stands.
    fixture
        .provider_session
        .emit(completed(Some(190_000), None));
    fixture.provider_session.emit(ProviderEvent::TurnCompleted);
    let settled = session_where(&fixture, session_id, "the Turn settles", |snapshot| {
        turn_settled(snapshot, 0)
    })
    .await;

    let [first, retry] = compactions(&settled)[..] else {
        panic!(
            "each attempt is its own Compaction: {:?}",
            settled.activities
        );
    };
    let Activity::Compaction { status, error, .. } = first else {
        unreachable!()
    };
    assert_eq!(*status, ActivityStatus::Failed);
    assert_eq!(
        error.as_deref(),
        Some("Conversation too long to summarise"),
        "a failed Compaction keeps the Provider's account of why"
    );
    let Activity::Compaction {
        status,
        before_tokens,
        after_tokens,
        error,
        ..
    } = retry
    else {
        unreachable!()
    };
    assert_eq!(*status, ActivityStatus::Completed);
    assert_eq!(
        (*before_tokens, *after_tokens),
        (Some(190_000), None),
        "a side the Provider did not report stays absent"
    );
    assert_eq!(*error, None);
    assert_eq!(
        settled.turns[0].status,
        TurnStatus::Completed,
        "a failed automatic Compaction leaves its Turn to Settle as the Provider says"
    );
    assert!(
        !settled
            .activities
            .iter()
            .any(|activity| matches!(activity, Activity::Error { .. })),
        "the failure is the Compaction's alone: {:?}",
        settled.activities
    );
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_compaction_still_active_when_its_turn_settles_settles_with_it() {
    for (settle, turn_status, compaction_status) in [
        (
            ProviderEvent::TurnInterrupted,
            TurnStatus::Interrupted,
            ActivityStatus::Interrupted,
        ),
        (
            ProviderEvent::TurnFailed {
                message: "the CLI exited".to_owned(),
            },
            TurnStatus::Failed,
            ActivityStatus::Failed,
        ),
        (
            ProviderEvent::TurnCompleted,
            TurnStatus::Completed,
            ActivityStatus::Failed,
        ),
    ] {
        let state_dir = tempfile::tempdir().expect("create isolated state directory");
        let fixture = working_turn(state_dir.path(), "compaction-turn-settle-test").await;
        fixture
            .provider_session
            .emit(ProviderEvent::CompactionStarted);
        fixture.provider_session.emit(settle);
        let settled = session_where(
            &fixture,
            fixture.session_id,
            "the Turn settles",
            |snapshot| turn_settled(snapshot, 0),
        )
        .await;
        assert_eq!(settled.turns[0].status, turn_status);
        assert_eq!(
            compaction_statuses(&settled),
            vec![compaction_status],
            "a Compaction still Active when its Turn Settles {turn_status:?} Settles with it"
        );
        fixture.server.shutdown().await.expect("shut down server");
    }
}

#[tokio::test]
async fn a_subagents_compaction_stands_in_its_own_session_and_never_its_parents() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "compaction-subagent-test").await;
    let subagent = ProviderSubagentId::new("task-1");
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::SubagentStarted {
            subagent_id: subagent.clone(),
            name: "Explore".to_owned(),
            description: "Map the provider seams".to_owned(),
            delegation: None,
        })
        .await;
    let parent = session_where(
        &fixture,
        fixture.session_id,
        "the Subagent row opens",
        |snapshot| !snapshot.activities.is_empty(),
    )
    .await;
    let Activity::Subagent {
        session_id: child_id,
        ..
    } = *the_subagent_row(&parent)
    else {
        unreachable!()
    };

    for event in [
        ProviderEvent::CompactionStarted,
        completed(Some(90_000), Some(12_000)),
    ] {
        fixture
            .provider_session
            .emit_attributed_and_wait_until_observed(
                ProviderEventAttribution::Subagent(subagent.clone()),
                event,
            )
            .await;
    }

    let child = session_where(&fixture, child_id, "the child compacts", |snapshot| {
        compaction_statuses(snapshot) == [ActivityStatus::Completed]
    })
    .await;
    let [
        Activity::Compaction {
            turn_id,
            before_tokens,
            after_tokens,
            ..
        },
    ] = compactions(&child)[..]
    else {
        unreachable!()
    };
    assert_eq!(
        *turn_id, child.turns[0].id,
        "the Subagent's Compaction stands in the Turn its stretch works in"
    );
    assert_eq!(
        (*before_tokens, *after_tokens),
        (Some(90_000), Some(12_000))
    );
    let parent = read_session(fixture.server.descriptor(), fixture.session_id).await;
    assert!(
        compactions(&parent).is_empty(),
        "the parent's Transcript carries nothing of its Subagent's Compaction: {:?}",
        parent.activities
    );
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn a_compaction_reported_while_no_turn_is_active_begins_a_continuation() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let fixture = working_turn(state_dir.path(), "compaction-continuation-test").await;
    let session_id = fixture.session_id;
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::TurnCompleted)
        .await;
    session_where(&fixture, session_id, "the first Turn settles", |snapshot| {
        turn_settled(snapshot, 0)
    })
    .await;

    // Nothing is owed the Session — no Subagent works on, no Watch woke it —
    // and still the Provider compacting is work a Turn must hold.
    fixture
        .provider_session
        .emit_and_wait_until_observed(ProviderEvent::CompactionStarted)
        .await;
    let compacting = session_where(&fixture, session_id, "a Continuation opens", |snapshot| {
        snapshot.turns.len() == 2
    })
    .await;
    let continuation = &compacting.turns[1];
    assert!(
        continuation.is_continuation(),
        "the Compaction begins a Continuation: {continuation:?}"
    );
    assert_eq!(continuation.status, TurnStatus::Active);
    let [
        Activity::Compaction {
            turn_id, status, ..
        },
    ] = compactions(&compacting)[..]
    else {
        panic!("the Compaction stands in the Continuation: {compacting:?}");
    };
    assert_eq!(*turn_id, continuation.id);
    assert_eq!(*status, ActivityStatus::Active);

    fixture
        .provider_session
        .emit(completed(Some(182_000), Some(31_000)));
    fixture.provider_session.emit(ProviderEvent::TurnCompleted);
    let settled = session_where(
        &fixture,
        session_id,
        "the Continuation settles",
        |snapshot| turn_settled(snapshot, 1),
    )
    .await;
    assert_eq!(settled.turns[1].status, TurnStatus::Completed);
    assert_eq!(compaction_statuses(&settled), [ActivityStatus::Completed]);
    fixture.server.shutdown().await.expect("shut down server");
}

#[tokio::test]
async fn compactions_are_stored_with_the_sessions_history_and_survive_a_restart() {
    let state_dir = tempfile::tempdir().expect("create isolated state directory");
    let channel = "compaction-restart-test";
    let fixture = working_turn(state_dir.path(), channel).await;
    let session_id = fixture.session_id;
    for event in [
        ProviderEvent::CompactionStarted,
        failed("Conversation too long to summarise"),
        ProviderEvent::CompactionStarted,
        completed(Some(182_000), Some(31_000)),
        ProviderEvent::TurnCompleted,
    ] {
        fixture.provider_session.emit(event);
    }
    let before = session_where(&fixture, session_id, "the Turn settles", |snapshot| {
        turn_settled(snapshot, 0)
    })
    .await;
    let recorded = compactions(&before)
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        compaction_statuses(&before),
        [ActivityStatus::Failed, ActivityStatus::Completed],
        "both attempts are recorded, each as it settled"
    );
    assert_eq!(before.turns[0].status, TurnStatus::Completed);
    fixture.server.shutdown().await.expect("shut down server");

    let (runtime, _provider) = crate::provider_support::ControlledProvider::new();
    let restarted = timeout(
        PROGRESS_DEADLINE,
        server::spawn_with_provider(
            ServerConfig::new(state_dir.path(), channel).expect("configure server"),
            runtime,
        ),
    )
    .await
    .expect("the server restarts in time")
    .expect("respawn server");
    let restored = read_session(restarted.descriptor(), session_id).await;
    assert_eq!(
        compactions(&restored),
        recorded.iter().collect::<Vec<_>>(),
        "Compactions are history like any other Activity"
    );
    let positions = |snapshot: &SessionSnapshot| {
        recorded
            .iter()
            .map(|compaction| {
                snapshot.transcript.iter().position(|item| {
                    *item
                        == TranscriptItem::Activity {
                            activity_id: compaction.id(),
                        }
                })
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        positions(&restored),
        positions(&before),
        "each keeps its place in the Transcript"
    );
    restarted.shutdown().await.expect("shut down server");
}
