//! Restoration is measured at SessionStore::new, before any client can read it.

use std::{cell::Cell, collections::HashMap, path::Path};

use super::{
    SessionStore,
    cost_coverage::{CostContribution, CountedCost, sum_contributions},
};
use crate::{
    protocol::{
        Activity, ActivityId, ActivityStatus, Cost, CostBasis, CostCoverage, CostDetails,
        CostTotal, ModelAvailability, ModelCatalog, Session, SessionId, SessionRevision,
        SessionSnapshot, SessionStandingInputs, SessionStatus, SessionSummary, SessionTimestamp,
        TranscriptItem, Turn, TurnId, TurnStatus, Usage, UsageMetricsAggregate,
        UsageMetricsBreakdown, UsageMetricsInterval, UsageMetricsQuery, UsageTotal, Workspace,
    },
    storage::{PersistedSession, RestoredSessions, StorageRepository, StorageWriter},
};

thread_local! {
    static WORK: Cell<usize> = const { Cell::new(0) };
}

fn turn(start: u64, end: Option<u64>, output: Option<u64>) -> Turn {
    Turn {
        id: TurnId::new(),
        prompt_id: None,
        compaction_requested: false,
        agent: None,
        status: if end.is_some() {
            TurnStatus::Completed
        } else {
            TurnStatus::Active
        },
        started_at: Some(SessionTimestamp(start)),
        settled_at: end.map(SessionTimestamp),
        last_output_at: None,
        usage: output.map(|output| Usage {
            output_tokens: Some(output),
            ..Default::default()
        }),
        cost: None,
        cost_basis: None,
        cost_details: None,
    }
}

fn priced_turn(
    started_at: Option<u64>,
    cost: f64,
    coverage: CostCoverage,
    recorded_at: u64,
) -> Turn {
    Turn {
        id: TurnId::new(),
        prompt_id: None,
        compaction_requested: false,
        agent: None,
        status: TurnStatus::Completed,
        started_at: started_at.map(SessionTimestamp),
        settled_at: Some(SessionTimestamp(recorded_at)),
        last_output_at: None,
        usage: Some(Usage {
            output_tokens: Some(1),
            ..Default::default()
        }),
        cost: Cost::from_usd(cost),
        cost_basis: Some(CostBasis::Reported),
        cost_details: Some(CostDetails {
            coverage,
            recorded_at: SessionTimestamp(recorded_at),
            is_partial: false,
            prior: Vec::new(),
        }),
    }
}

#[tokio::test]
async fn an_untimed_descendant_aggregate_is_suppressed_but_an_untimed_turn_cost_is_retained() {
    let directory = tempfile::tempdir().unwrap();
    let repository = StorageRepository::open(directory.path()).await.unwrap();
    let (writer, sink) = StorageWriter::spawn(repository);

    let mut aggregate_root = persisted(directory.path(), None);
    aggregate_root.snapshot.turns = vec![priced_turn(
        Some(10),
        0.10,
        CostCoverage::SessionSubtree {
            reporting_lifetime: "ancestor-process".to_owned(),
        },
        20,
    )];
    let aggregate_root_id = aggregate_root.snapshot.session.id;
    let mut untimed_aggregate = persisted(directory.path(), Some(aggregate_root_id));
    untimed_aggregate.snapshot.turns = vec![priced_turn(
        None,
        0.03,
        CostCoverage::SessionSubtree {
            reporting_lifetime: "child-process".to_owned(),
        },
        15,
    )];

    let mut turn_root = persisted(directory.path(), None);
    turn_root.snapshot.turns = vec![priced_turn(
        Some(10),
        0.10,
        CostCoverage::SessionSubtree {
            reporting_lifetime: "other-ancestor-process".to_owned(),
        },
        20,
    )];
    let turn_root_id = turn_root.snapshot.session.id;
    let mut untimed_turn = persisted(directory.path(), Some(turn_root_id));
    untimed_turn.snapshot.turns = vec![priced_turn(None, 0.03, CostCoverage::Turn, 15)];

    let mut untimed_root = persisted(directory.path(), None);
    untimed_root.snapshot.turns = vec![priced_turn(
        None,
        0.10,
        CostCoverage::SessionSubtree {
            reporting_lifetime: "untimed-ancestor-process".to_owned(),
        },
        20,
    )];
    let untimed_root_id = untimed_root.snapshot.session.id;
    let mut timed_aggregate = persisted(directory.path(), Some(untimed_root_id));
    timed_aggregate.snapshot.turns = vec![priced_turn(
        Some(10),
        0.03,
        CostCoverage::SessionSubtree {
            reporting_lifetime: "timed-child-process".to_owned(),
        },
        15,
    )];

    let store = SessionStore::new(
        RestoredSessions {
            readable: vec![
                aggregate_root,
                untimed_aggregate,
                turn_root,
                untimed_turn,
                untimed_root,
                timed_aggregate,
            ],
            ..Default::default()
        },
        sink,
        Vec::new(),
        Default::default(),
    );

    let aggregate_total = store
        .subscribe(aggregate_root_id)
        .unwrap()
        .snapshot
        .total_usage()
        .expect("ancestor aggregate total");
    assert_eq!(aggregate_total.cost, Cost::from_usd(0.10));
    assert!(aggregate_total.cost_is_partial);

    let turn_total = store
        .subscribe(turn_root_id)
        .unwrap()
        .snapshot
        .total_usage()
        .expect("ancestor plus Turn cost total");
    assert_eq!(turn_total.cost, Cost::from_usd(0.13));
    assert!(turn_total.cost_is_partial);

    let untimed_total = store
        .subscribe(untimed_root_id)
        .unwrap()
        .snapshot
        .total_usage()
        .expect("untimed ancestor aggregate total");
    assert_eq!(untimed_total.cost, Cost::from_usd(0.10));
    assert!(untimed_total.cost_is_partial);

    assert_contributions_add_up(&store);
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn restored_usage_folds_descendants_without_spending_revisions_or_writes() {
    let directory = tempfile::tempdir().unwrap();
    let repository = StorageRepository::open(directory.path()).await.unwrap();
    let (writer, sink) = StorageWriter::spawn(repository.clone());
    let mut root = persisted(directory.path(), None);
    root.snapshot.turns = vec![turn(10, Some(20), None), turn(40, Some(50), Some(0))];
    let mut child = persisted(directory.path(), Some(root.snapshot.session.id));
    child.snapshot.turns = vec![turn(18, Some(42), Some(3))];
    let mut leaf = persisted(directory.path(), Some(child.snapshot.session.id));
    leaf.snapshot.turns = vec![turn(49, Some(60), Some(5))];
    let mut sibling = persisted(directory.path(), Some(root.snapshot.session.id));
    sibling.snapshot.turns = vec![turn(1, Some(5), None)];
    let mut unknown = persisted(directory.path(), None);
    let mut untimed = turn(0, Some(1), None);
    untimed.started_at = None;
    unknown.snapshot.turns = vec![untimed];
    let mut zero = persisted(directory.path(), None);
    zero.snapshot.turns = vec![turn(10, Some(20), Some(0))];
    let records = vec![root, child, leaf, sibling, unknown, zero];
    let ids = records
        .iter()
        .map(|record| record.snapshot.session.id)
        .collect::<Vec<_>>();
    let store = SessionStore::new(
        RestoredSessions {
            readable: records,
            ..Default::default()
        },
        sink,
        Vec::new(),
        Default::default(),
    );
    for (index, (since, output)) in [
        (None, Some(8)),
        (None, Some(8)),
        (None, Some(5)),
        (None, None),
        (None, None),
        (None, Some(0)),
    ]
    .into_iter()
    .enumerate()
    {
        let snapshot = store.subscribe(ids[index]).unwrap().snapshot;
        assert_eq!(snapshot.working_since(), since.map(SessionTimestamp));
        assert_eq!(
            snapshot.total_usage(),
            output.map(|output| UsageTotal {
                output_tokens: Some(output),
                cost_is_partial: true,
                ..Default::default()
            })
        );
        assert_eq!(snapshot.revision, SessionRevision(7));
    }
    let listed = store.list(None);
    assert_eq!(listed.len(), 3);
    for item in listed {
        let crate::protocol::SessionListItem::Readable(summary) = item else {
            panic!("readable")
        };
        let snapshot = store.subscribe(summary.session.id).unwrap().snapshot;
        assert_eq!(summary.total_usage, snapshot.total_usage());
        assert_eq!(summary.session.working_since, snapshot.working_since());
        assert_eq!(summary.updated_at, SessionTimestamp(2));
    }
    assert_contributions_add_up(&store);
    writer.shutdown().await.unwrap();
    assert!(
        repository
            .load_sessions()
            .await
            .unwrap()
            .readable
            .is_empty(),
        "restoration wrote synthetic state"
    );
}

/// A parent's row for `child`, still open, as `fail_all` would have found it.
fn subagent_row(parent: &mut PersistedSession, child: SessionId) -> ActivityId {
    let id = ActivityId::new();
    let turn_id = parent.snapshot.turns[0].id;
    parent.snapshot.activities.push(Activity::Subagent {
        id,
        turn_id,
        status: ActivityStatus::Active,
        name: "explorer".into(),
        description: "map the seams".into(),
        model: None,
        session_id: child,
        brokered: false,
        duration_ms: None,
        delegated_at: None,
    });
    parent
        .snapshot
        .transcript
        .push(TranscriptItem::Activity { activity_id: id });
    id
}

fn subagent_status(snapshot: &SessionSnapshot, id: ActivityId) -> (ActivityStatus, Option<u64>) {
    snapshot
        .activities
        .iter()
        .find_map(|activity| match activity {
            Activity::Subagent {
                id: found,
                status,
                duration_ms,
                ..
            } if *found == id => Some((*status, *duration_ms)),
            _ => None,
        })
        .expect("the Subagent row survives restoration")
}

fn error_texts(snapshot: &SessionSnapshot, turn_id: TurnId) -> Vec<String> {
    snapshot
        .activities
        .iter()
        .filter_map(|activity| match activity {
            Activity::Error {
                turn_id: owner,
                text,
                ..
            } if *owner == turn_id => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// The Turn a child Session was running when the server stopped is settled at
/// the next start, and its parent's row settles with it, because nothing
/// survives a stop to finish either (ADR 0029).
#[tokio::test]
async fn a_subagent_turn_left_open_by_a_stop_settles_as_failed_at_the_next_start() {
    let directory = tempfile::tempdir().unwrap();
    let repository = StorageRepository::open(directory.path()).await.unwrap();
    let mut root = persisted(directory.path(), None);
    root.snapshot.turns = vec![turn(10, Some(32), None)];
    let mut child = persisted(directory.path(), Some(root.snapshot.session.id));
    let mut open = turn(30, None, Some(2));
    open.last_output_at = Some(SessionTimestamp(50));
    let open_id = open.id;
    child.snapshot.turns = vec![open];
    child.snapshot.session.status = SessionStatus::Active;
    let child_id = child.snapshot.session.id;
    let row = subagent_row(&mut root, child_id);
    let root_id = root.snapshot.session.id;
    let records = vec![root, child];
    let (writer, sink) = StorageWriter::spawn(repository.clone());
    let store = SessionStore::new(
        RestoredSessions {
            readable: records,
            ..Default::default()
        },
        sink,
        Vec::new(),
        Default::default(),
    );

    let restored_child = store.subscribe(child_id).unwrap().snapshot;
    let settled = restored_child
        .turns
        .iter()
        .find(|turn| turn.id == open_id)
        .unwrap();
    assert_eq!(settled.status, TurnStatus::Failed);
    assert_eq!(
        settled.settled_at,
        Some(SessionTimestamp(50)),
        "the Turn settles at its last output, not at the restart"
    );
    assert_eq!(
        error_texts(&restored_child, open_id),
        vec![super::hydration::STOPPED_SUBAGENT_MESSAGE.to_owned()]
    );
    assert_eq!(restored_child.session.status, SessionStatus::Idle);
    assert_eq!(restored_child.working_since(), None);
    let restored_root = store.subscribe(root_id).unwrap().snapshot;
    assert_eq!(
        subagent_status(&restored_root, row),
        (ActivityStatus::Failed, None)
    );
    assert_eq!(restored_root.working_since(), None);
    assert_eq!(restored_root.session.status, SessionStatus::Idle);

    // The repair is durable: the next process reads it back rather than
    // deciding it again.
    assert_contributions_add_up(&store);
    writer.shutdown().await.unwrap();
    let reloaded_child = repository
        .session(child_id)
        .await
        .unwrap()
        .expect("the child was written");
    assert_eq!(reloaded_child.snapshot, restored_child);
    let reloaded_root = repository
        .session(root_id)
        .await
        .unwrap()
        .expect("the parent was written");
    // Subagent Usage is derived at every start rather than stored, so the
    // parent is compared on what the repair wrote.
    assert_eq!(
        subagent_status(&reloaded_root.snapshot, row),
        (ActivityStatus::Failed, None)
    );
    assert_eq!(reloaded_root.snapshot.revision, restored_root.revision);
    assert_eq!(reloaded_root.snapshot.session, restored_root.session);
}

/// A root's own open Turn is orphaned by a stop the same way: no Provider
/// survives to finish it either, and nothing at restoration resumes it.
#[tokio::test]
async fn a_root_turn_left_open_by_a_stop_settles_as_failed_at_the_next_start() {
    let directory = tempfile::tempdir().unwrap();
    let repository = StorageRepository::open(directory.path()).await.unwrap();
    let mut root = persisted(directory.path(), None);
    let open = turn(40, None, Some(1));
    let open_id = open.id;
    root.snapshot.turns = vec![turn(10, Some(20), None), open];
    root.snapshot.session.status = SessionStatus::Active;
    let root_id = root.snapshot.session.id;
    let records = vec![root];
    let (writer, sink) = StorageWriter::spawn(repository.clone());
    let store = SessionStore::new(
        RestoredSessions {
            readable: records,
            ..Default::default()
        },
        sink,
        Vec::new(),
        Default::default(),
    );

    let restored = store.subscribe(root_id).unwrap().snapshot;
    let settled = restored
        .turns
        .iter()
        .find(|turn| turn.id == open_id)
        .unwrap();
    assert_eq!(settled.status, TurnStatus::Failed);
    assert_eq!(
        settled.settled_at,
        Some(SessionTimestamp(40)),
        "a Turn that never showed output settles where it began"
    );
    assert_eq!(
        error_texts(&restored, open_id),
        vec![super::hydration::STOPPED_TURN_MESSAGE.to_owned()]
    );
    assert_eq!(restored.session.status, SessionStatus::Idle);
    assert_eq!(restored.working_since(), None);
    let listed = store.list(None);
    let crate::protocol::SessionListItem::Readable(summary) = &listed[0] else {
        panic!("readable")
    };
    assert_eq!(summary.session.status, SessionStatus::Idle);
    assert_eq!(summary.session.working_since, None);

    assert_contributions_add_up(&store);
    writer.shutdown().await.unwrap();
    let reloaded = repository
        .session(root_id)
        .await
        .unwrap()
        .expect("the root was written");
    assert_eq!(reloaded.snapshot, restored);
}

/// A Subagent whose Turn already settled — completed, failed, or stopped —
/// is exactly as the last process left it: the repair spends no revision and
/// writes nothing.
#[tokio::test]
async fn a_subagent_already_settled_is_left_exactly_as_it_was() {
    let directory = tempfile::tempdir().unwrap();
    let repository = StorageRepository::open(directory.path()).await.unwrap();
    let mut root = persisted(directory.path(), None);
    root.snapshot.turns = vec![turn(10, Some(32), None)];
    let mut child = persisted(directory.path(), Some(root.snapshot.session.id));
    let mut interrupted = turn(30, Some(31), Some(2));
    interrupted.status = TurnStatus::Interrupted;
    child.snapshot.turns = vec![interrupted];
    let child_id = child.snapshot.session.id;
    let row = subagent_row(&mut root, child_id);
    let root_id = root.snapshot.session.id;
    let records = vec![root, child];
    let expected = records
        .iter()
        .map(|persisted| persisted.snapshot.clone())
        .collect::<Vec<_>>();
    let (writer, sink) = StorageWriter::spawn(repository.clone());
    let store = SessionStore::new(
        RestoredSessions {
            readable: records,
            ..Default::default()
        },
        sink,
        Vec::new(),
        Default::default(),
    );

    let restored_child = store.subscribe(child_id).unwrap().snapshot;
    assert_eq!(restored_child, expected[1]);
    let restored_root = store.subscribe(root_id).unwrap().snapshot;
    assert_eq!(restored_root.revision, SessionRevision(7));
    assert_eq!(
        subagent_status(&restored_root, row),
        (ActivityStatus::Active, None),
        "a row the last process left open over a settled Turn is not this repair's to close"
    );
    assert_contributions_add_up(&store);
    writer.shutdown().await.unwrap();
    assert!(
        repository
            .load_sessions()
            .await
            .unwrap()
            .readable
            .is_empty(),
        "restoration wrote synthetic state"
    );
}

/// A resumed Subagent's open row may stand in a sibling's Transcript — the
/// sibling sent the resume — rather than in its parent's. It settles with the
/// Turn the resume began all the same, while the rows its earlier stretches
/// settled with stay as they were (ADR 0031).
#[tokio::test]
async fn a_resume_row_a_sibling_holds_settles_with_the_turn_left_open_by_a_stop() {
    let directory = tempfile::tempdir().unwrap();
    let repository = StorageRepository::open(directory.path()).await.unwrap();
    let mut root = persisted(directory.path(), None);
    root.snapshot.turns = vec![turn(10, Some(32), None)];
    let root_id = root.snapshot.session.id;
    let mut resumed = persisted(directory.path(), Some(root_id));
    resumed.snapshot.turns = vec![turn(12, Some(20), None), turn(30, None, None)];
    resumed.snapshot.session.status = SessionStatus::Active;
    let resumed_id = resumed.snapshot.session.id;
    let mut sibling = persisted(directory.path(), Some(root_id));
    sibling.snapshot.turns = vec![turn(14, Some(31), None)];
    let sibling_id = sibling.snapshot.session.id;
    let spawn_row = subagent_row(&mut root, resumed_id);
    let Some(Activity::Subagent {
        status,
        duration_ms,
        ..
    }) = root.snapshot.activities.last_mut()
    else {
        unreachable!()
    };
    *status = ActivityStatus::Completed;
    *duration_ms = Some(8);
    let resume_row = subagent_row(&mut sibling, resumed_id);
    let records = vec![root, resumed, sibling];
    let (writer, sink) = StorageWriter::spawn(repository.clone());
    let store = SessionStore::new(
        RestoredSessions {
            readable: records,
            ..Default::default()
        },
        sink,
        Vec::new(),
        Default::default(),
    );

    let restored = store.subscribe(resumed_id).unwrap().snapshot;
    assert_eq!(restored.turns[0].status, TurnStatus::Completed);
    assert_eq!(restored.turns[1].status, TurnStatus::Failed);
    assert_eq!(
        subagent_status(&store.subscribe(sibling_id).unwrap().snapshot, resume_row),
        (ActivityStatus::Failed, None),
        "the resume row settles wherever it stands"
    );
    assert_eq!(
        subagent_status(&store.subscribe(root_id).unwrap().snapshot, spawn_row),
        (ActivityStatus::Completed, Some(8)),
        "the spawn's row keeps what its own stretch settled with"
    );
    assert_contributions_add_up(&store);
    writer.shutdown().await.unwrap();
}

async fn restore_tree(wide: bool) {
    let directory = tempfile::tempdir().unwrap();
    let repository = StorageRepository::open(directory.path()).await.unwrap();
    let (writer, sink) = StorageWriter::spawn(repository);
    let mut records = vec![persisted(directory.path(), None)];
    for index in 1..512 {
        let parent = if wide { 0 } else { index - 1 };
        records.push(persisted(
            directory.path(),
            Some(records[parent].snapshot.session.id),
        ));
    }
    for (index, record) in records.iter_mut().enumerate() {
        let start = if wide { 10 } else { 10 + index as u64 * 3 };
        record.snapshot.turns = vec![turn(start, Some(start + 1), Some(1))];
    }
    let root = records[0].snapshot.session.id;
    let leaf = records[511].snapshot.session.id;
    WORK.set(0);
    let store = SessionStore::new(
        RestoredSessions {
            readable: records,
            ..Default::default()
        },
        sink,
        Vec::new(),
        Default::default(),
    );
    let work = WORK.get();
    assert!(
        work <= 512 * 16,
        "512 Sessions/Turns required {work} operations (wide={wide})"
    );
    let snapshot = store.subscribe(root).unwrap().snapshot;
    assert_eq!(snapshot.total_usage().unwrap().output_tokens, Some(512));
    assert_eq!(snapshot.subagent_usage.unwrap().output_tokens, Some(511));
    assert_eq!(snapshot.working_since(), None);
    assert_eq!(
        store
            .subscribe(leaf)
            .unwrap()
            .snapshot
            .total_usage()
            .unwrap()
            .output_tokens,
        Some(1)
    );
    assert_eq!(store.list(None).len(), 1);
    assert_contributions_add_up(&store);
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn restoring_a_deep_tree_reuses_descendant_history() {
    restore_tree(false).await;
}

#[tokio::test]
async fn restoring_a_wide_tree_reuses_child_totals() {
    restore_tree(true).await;
}

#[tokio::test]
async fn restoring_many_disjoint_reporting_lifetimes_uses_indexed_coverage() {
    let directory = tempfile::tempdir().unwrap();
    let repository = StorageRepository::open(directory.path()).await.unwrap();
    let (writer, sink) = StorageWriter::spawn(repository);
    let mut record = persisted(directory.path(), None);
    record.snapshot.turns = (0..256)
        .map(|index| {
            let started_at = SessionTimestamp(index * 4 + 1);
            Turn {
                id: TurnId::new(),
                prompt_id: None,
                compaction_requested: false,
                agent: None,
                status: TurnStatus::Completed,
                started_at: Some(started_at),
                settled_at: Some(SessionTimestamp(started_at.0 + 2)),
                last_output_at: Some(SessionTimestamp(started_at.0 + 1)),
                usage: Some(Usage {
                    output_tokens: Some(1),
                    ..Default::default()
                }),
                cost: Cost::from_usd(0.01),
                cost_basis: Some(CostBasis::Reported),
                cost_details: Some(CostDetails {
                    coverage: CostCoverage::SessionSubtree {
                        reporting_lifetime: format!("process-{index}"),
                    },
                    recorded_at: SessionTimestamp(started_at.0 + 1),
                    is_partial: false,
                    prior: Vec::new(),
                }),
            }
        })
        .collect();
    let root = record.snapshot.session.id;
    WORK.set(0);
    let store = SessionStore::new(
        RestoredSessions {
            readable: vec![record],
            ..Default::default()
        },
        sink,
        Vec::new(),
        Default::default(),
    );
    let work = WORK.get();
    assert!(
        work <= 256 * 16,
        "256 reporting lifetimes required {work} restoration operations"
    );
    let total = store
        .subscribe(root)
        .unwrap()
        .snapshot
        .total_usage()
        .expect("reported totals");
    assert_eq!(total.cost, Cost::from_usd(2.56));
    assert!(!total.cost_is_partial);
    assert_contributions_add_up(&store);
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn restored_balanced_tree_matches_the_durable_interval_union() {
    let directory = tempfile::tempdir().unwrap();
    let repository = StorageRepository::open(directory.path()).await.unwrap();
    let (writer, sink) = StorageWriter::spawn(repository);
    let mut records = vec![persisted(directory.path(), None)];
    for index in 1..127 {
        records.push(persisted(
            directory.path(),
            Some(records[(index - 1) / 2].snapshot.session.id),
        ));
    }
    for (index, record) in records.iter_mut().enumerate() {
        // Repeated starts, touching ends, gaps, nested overlaps and old Turns
        // without timing.
        let start = (index as u64 * 17) % 83;
        record.snapshot.turns = vec![
            turn(start, Some(start + 5), Some(0)),
            turn(start + 20, Some(start + 23), Some(2)),
        ];
        let mut untimed = turn(0, Some(1), None);
        untimed.started_at = None;
        record.snapshot.turns.push(untimed);
        let mut no_end = turn(0, Some(1000), None);
        no_end.settled_at = None;
        record.snapshot.turns.push(no_end);
    }
    // Reference reading: collect raw descendant Turns and sort/sweep their
    // intervals, as restoration did before #279. This deliberately does no
    // bottom-up composition and uses no interval-set implementation.
    let expected = (0..records.len())
        .map(|index| {
            let mut descendants = vec![index];
            let mut next = 0;
            while next < descendants.len() {
                let child = descendants[next] * 2 + 1;
                descendants.extend((child..child + 2).filter(|child| *child < records.len()));
                next += 1;
            }
            let turns = descendants
                .iter()
                .flat_map(|index| &records[*index].snapshot.turns)
                .collect::<Vec<_>>();
            let usage = UsageTotal::of_turns(turns.iter().copied());
            let mut intervals = turns
                .iter()
                .filter_map(|turn| {
                    Some((
                        turn.started_at?,
                        if turn.status.is_terminal() {
                            Some(turn.settled_at?)
                        } else {
                            None
                        },
                    ))
                })
                .collect::<Vec<_>>();
            intervals.sort_unstable_by_key(|(start, _)| *start);
            let mut since = None;
            let mut end: Option<SessionTimestamp> = Some(SessionTimestamp(0));
            for (start, settled) in intervals {
                if since.is_none() || end.is_some_and(|end| end < start) {
                    since = Some(start);
                    end = settled;
                } else {
                    end = end.zip(settled).map(|(left, right)| left.max(right));
                }
            }
            (
                records[index].snapshot.session.id,
                since.filter(|_| end.is_none()),
                usage,
            )
        })
        .collect::<Vec<_>>();
    WORK.set(0);
    let store = SessionStore::new(
        RestoredSessions {
            readable: records,
            ..Default::default()
        },
        sink,
        Vec::new(),
        Default::default(),
    );
    assert!(
        WORK.get() <= 127 * 64,
        "balanced histories repeatedly traversed"
    );
    for (id, since, usage) in expected {
        let snapshot = store.subscribe(id).unwrap().snapshot;
        assert_eq!(snapshot.working_since(), since, "Working for {id}");
        assert_eq!(snapshot.total_usage(), usage, "Usage for {id}");
    }
    assert_contributions_add_up(&store);
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn malformed_relationships_do_not_promote_children_into_listed_roots() {
    let directory = tempfile::tempdir().unwrap();
    let repository = StorageRepository::open(directory.path()).await.unwrap();
    let (writer, sink) = StorageWriter::spawn(repository);
    let mut root = persisted(directory.path(), None);
    let mut orphan = persisted(directory.path(), Some(SessionId::new()));
    let mut cycle = persisted(directory.path(), None);
    let mut other = persisted(directory.path(), Some(cycle.snapshot.session.id));
    cycle.snapshot.session.parent = Some(other.snapshot.session.id);
    cycle.summary.session.parent = cycle.snapshot.session.parent;
    // Priced, so that Usage Metrics leaving the orphan and the cycle out is
    // pinned on purpose: they are in no tree Cost a listing shows, so the
    // read counts neither their Cost nor their Usage, and its two measures
    // cover the same Sessions.
    for (record, usd) in [
        (&mut root, 0.01),
        (&mut orphan, 0.10),
        (&mut cycle, 1.00),
        (&mut other, 10.00),
    ] {
        record.snapshot.turns = vec![priced_turn(Some(10), usd, CostCoverage::Turn, 12)];
    }
    let root_id = root.snapshot.session.id;
    let store = SessionStore::new(
        RestoredSessions {
            readable: vec![root, orphan, cycle, other],
            ..Default::default()
        },
        sink,
        Vec::new(),
        Default::default(),
    );
    store.reconcile_approval_postures(&crate::protocol::EffectiveSettings::default());
    assert_eq!(store.list(None).len(), 1);
    assert!(store.subscribe(root_id).is_some());
    let all_time = all_time_metrics(&store, UsageMetricsBreakdown::Model).buckets[0].total;
    assert_eq!(
        (all_time.cost, all_time.output_tokens),
        (Cost::from_usd(0.01), Some(1)),
        "only the listed root's Turn counts"
    );
    assert_contributions_add_up(&store);
    writer.shutdown().await.unwrap();
}

pub(super) fn record_work(count: usize) {
    WORK.set(WORK.get() + count);
}

pub(super) fn persisted(workspace: &Path, parent: Option<SessionId>) -> PersistedSession {
    let session = Session {
        checkout: None,
        context_fill: None,
        id: SessionId::new(),
        execution_directory: crate::protocol::ExecutionDirectory {
            path: workspace.to_owned(),
        },
        workspace: Workspace::directory(workspace.to_owned()),
        agent_selection: None,
        agent_selection_availability: ModelAvailability::Unavailable,
        approval_posture: None,
        status: SessionStatus::Idle,
        working_since: None,
        monitoring_since: None,
        parent,
        begun_by: None,
    };
    PersistedSession {
        summary: SessionSummary {
            checkout_state: None,
            session: session.clone(),
            title: "restoration fixture".into(),
            icon: None,
            settled_at: None,
            standing_inputs: SessionStandingInputs::default(),
            total_usage: None,
            own_cost: None,
            remote_subsessions: Vec::new(),
            created_at: SessionTimestamp(1),
            updated_at: SessionTimestamp(2),
        },
        snapshot: SessionSnapshot {
            title: "restoration fixture".into(),
            icon: None,
            session,
            revision: SessionRevision(7),
            prompts: vec![],
            turns: vec![],
            messages: vec![],
            activities: vec![],
            transcript: vec![],
            subagent_usage: None,
            total_cost: None,
            own_cost: None,
            subagent_interventions: vec![],
            pending_approvals: Vec::new(),
            submitting_approvals: Vec::new(),
            pending_approvals_revision: crate::protocol::SessionRevision(0),
            watches: Vec::new(),
            attachments: Vec::new(),
            waiting_on_subagents: None,
        },
        resume_states: HashMap::new(),
        subagent_identity: None,
        brokered: false,
    }
}

#[tokio::test]
async fn restoring_independent_roots_does_linear_relationship_work() {
    let directory = tempfile::tempdir().unwrap();
    let repository = StorageRepository::open(directory.path()).await.unwrap();
    let (writer, sink) = StorageWriter::spawn(repository);
    let restored = RestoredSessions {
        readable: (0..128)
            .map(|_| persisted(directory.path(), None))
            .collect(),
        ..Default::default()
    };
    WORK.set(0);
    let store = SessionStore::new(restored, sink, Vec::new(), Default::default());
    let work = WORK.get();
    assert_eq!(store.list(None).len(), 128);
    assert!(
        work <= 128 * 8,
        "128 roots required {work} restoration operations"
    );
    assert_contributions_add_up(&store);
    writer.shutdown().await.unwrap();
}

/// A Session's two Cost readings: its tree Cost and its own.
#[derive(Debug, PartialEq)]
struct Costs {
    tree: Option<CostTotal>,
    own: Option<CostTotal>,
}

fn known(usd: f64, is_partial: bool) -> Option<CostTotal> {
    Some(CostTotal {
        cost: Cost::from_usd(usd).unwrap(),
        is_partial,
    })
}

fn subtree(reporting_lifetime: &str) -> CostCoverage {
    CostCoverage::SessionSubtree {
        reporting_lifetime: reporting_lifetime.to_owned(),
    }
}

async fn restore(
    directory: &Path,
    records: Vec<PersistedSession>,
) -> (SessionStore, StorageWriter) {
    let repository = StorageRepository::open(directory).await.unwrap();
    let (writer, sink) = StorageWriter::spawn(repository);
    let store = SessionStore::new(
        RestoredSessions {
            readable: records,
            ..Default::default()
        },
        sink,
        Vec::new(),
        Default::default(),
    );
    (store, writer)
}

/// What restoration left `id` reading, after checking the live roll-up
/// derives the same from the same history: the two implementations are the
/// seam a Cost reading could otherwise change across a restart.
fn costs(store: &SessionStore, id: SessionId) -> Costs {
    let snapshot = store.subscribe(id).unwrap().snapshot;
    let restored = Costs {
        tree: snapshot.total_cost,
        own: snapshot.own_cost,
    };
    let state = store.state.lock().unwrap();
    let live = Costs {
        tree: state.cost_total(id),
        own: state.own_cost(id),
    };
    assert_eq!(restored, live, "restoration and the live roll-up agree");
    restored
}

#[tokio::test]
async fn a_whole_tree_report_covers_a_native_child_but_never_a_brokered_one() {
    let workspace = tempfile::tempdir().unwrap();
    // The child's work lies inside the parent's reporting lifetime, and then
    // outside it; only its link to the parent varies within each.
    for (brokered, (started_at, recorded_at), expected) in [
        (false, (12, 15), known(0.10, false)),
        (true, (12, 15), known(0.15, false)),
        (false, (25, 30), known(0.15, false)),
        (true, (25, 30), known(0.15, false)),
    ] {
        let mut parent = persisted(workspace.path(), None);
        parent.snapshot.turns = vec![priced_turn(Some(10), 0.10, subtree("parent-process"), 20)];
        let parent_id = parent.snapshot.session.id;
        let mut child = persisted(workspace.path(), Some(parent_id));
        child.brokered = brokered;
        child.snapshot.turns = vec![priced_turn(
            Some(started_at),
            0.05,
            CostCoverage::Turn,
            recorded_at,
        )];
        let child_id = child.snapshot.session.id;
        let (store, writer) = restore(workspace.path(), vec![parent, child]).await;

        assert_eq!(
            costs(&store, parent_id),
            Costs {
                tree: expected,
                own: known(0.10, false),
            },
            "brokered: {brokered}, child worked {started_at}..{recorded_at}"
        );
        assert_eq!(
            costs(&store, child_id),
            Costs {
                tree: known(0.05, false),
                own: known(0.05, false),
            }
        );
        assert_contributions_add_up(&store);
        writer.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn a_sidekicks_tree_cost_carries_its_subsessions_whole_trees() {
    let workspace = tempfile::tempdir().unwrap();
    let mut sidekick = persisted(workspace.path(), None);
    // The Sidekick's whole-tree report spans the Subsession's work, yet
    // covers none of it: the Subsession ran on a Provider actor of its own.
    sidekick.snapshot.turns = vec![priced_turn(Some(10), 0.10, subtree("sidekick-process"), 40)];
    let sidekick_id = sidekick.snapshot.session.id;
    let begun_by = Some(crate::protocol::Author::Sidekick {
        session_id: sidekick_id,
        title: "Sidekick".to_owned(),
    });
    let subsession_with = |turns| {
        let mut subsession = persisted(workspace.path(), None);
        subsession.snapshot.session.begun_by = begun_by.clone();
        subsession.summary.session.begun_by = begun_by.clone();
        subsession.snapshot.turns = turns;
        subsession
    };
    let subsession = subsession_with(vec![priced_turn(Some(12), 0.05, CostCoverage::Turn, 15)]);
    let subsession_id = subsession.snapshot.session.id;
    let mut subagent = persisted(workspace.path(), Some(subsession_id));
    subagent.snapshot.turns = vec![priced_turn(Some(20), 0.02, CostCoverage::Turn, 25)];
    let unpriced = subsession_with(vec![turn(30, Some(35), Some(1))]);
    let unpriced_id = unpriced.snapshot.session.id;
    let mut user_begun = persisted(workspace.path(), None);
    user_begun.snapshot.turns = vec![priced_turn(Some(12), 1.00, CostCoverage::Turn, 15)];
    let (store, writer) = restore(
        workspace.path(),
        vec![sidekick, subsession, subagent, unpriced, user_begun],
    )
    .await;

    assert_eq!(
        costs(&store, sidekick_id),
        Costs {
            tree: known(0.17, true),
            own: known(0.10, false),
        },
        "each Subsession's tree Cost adds beneath the Sidekick's, and one \
         whose work has no Cost leaves the sum partial"
    );
    assert_eq!(
        costs(&store, subsession_id),
        Costs {
            tree: known(0.07, false),
            own: known(0.05, false),
        },
        "a Subsession's own tree Cost carries nothing of its Sidekick's"
    );
    assert_eq!(costs(&store, unpriced_id).tree, None);
    {
        let state = store.state.lock().unwrap();
        let counted_under_sidekick = state
            .tree_cost_contributions(sidekick_id)
            .into_iter()
            .filter(|contribution| {
                contribution.session_id == subsession_id && contribution.amount.is_some()
            })
            .count();
        assert_eq!(
            counted_under_sidekick, 1,
            "a Subsession's Turn counts once beneath its Sidekick"
        );
    }

    assert_contributions_add_up(&store);
    // A Subsession deleted takes its Cost out of its Sidekick's tree Cost.
    store.delete(subsession_id).unwrap();
    store.delete(unpriced_id).unwrap();
    assert_eq!(
        costs(&store, sidekick_id),
        Costs {
            tree: known(0.10, false),
            own: known(0.10, false),
        },
        "a deleted Subsession's Cost leaves its Sidekick's tree Cost"
    );
    assert_contributions_add_up(&store);
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_brokered_subagent_covers_its_own_native_descendants_and_counts_its_brokered_ones() {
    let workspace = tempfile::tempdir().unwrap();
    let mut parent = persisted(workspace.path(), None);
    parent.snapshot.turns = vec![priced_turn(Some(10), 0.10, subtree("parent-process"), 30)];
    let parent_id = parent.snapshot.session.id;
    let mut child = persisted(workspace.path(), Some(parent_id));
    child.brokered = true;
    child.snapshot.turns = vec![priced_turn(Some(12), 0.04, subtree("child-process"), 20)];
    let child_id = child.snapshot.session.id;
    let mut native = persisted(workspace.path(), Some(child_id));
    native.snapshot.turns = vec![priced_turn(Some(14), 0.02, CostCoverage::Turn, 16)];
    let native_id = native.snapshot.session.id;
    let mut brokered = persisted(workspace.path(), Some(child_id));
    brokered.brokered = true;
    brokered.snapshot.turns = vec![priced_turn(Some(15), 0.03, CostCoverage::Turn, 18)];
    let brokered_id = brokered.snapshot.session.id;
    // A brokered child's untimed report counts, and cannot say what it spans.
    let mut untimed = persisted(workspace.path(), Some(parent_id));
    untimed.brokered = true;
    untimed.snapshot.turns = vec![priced_turn(None, 0.05, subtree("untimed-process"), 25)];
    let untimed_id = untimed.snapshot.session.id;
    let (store, writer) = restore(
        workspace.path(),
        vec![parent, child, native, brokered, untimed],
    )
    .await;

    assert_eq!(
        costs(&store, parent_id),
        Costs {
            tree: known(0.22, true),
            own: known(0.10, false),
        },
        "every brokered link counts, to any depth, and the native grandchild once"
    );
    assert_eq!(
        costs(&store, untimed_id),
        Costs {
            tree: known(0.05, true),
            own: known(0.05, true),
        }
    );
    assert_eq!(
        costs(&store, child_id),
        Costs {
            tree: known(0.07, false),
            own: known(0.04, false),
        },
        "the brokered child's report covers its native Subagent and nothing brokered"
    );
    assert_eq!(
        costs(&store, native_id),
        Costs {
            tree: known(0.02, false),
            own: known(0.02, false),
        }
    );
    assert_eq!(
        costs(&store, brokered_id),
        Costs {
            tree: known(0.03, false),
            own: known(0.03, false),
        }
    );
    let listed = store
        .list(None)
        .into_iter()
        .find_map(|item| match item {
            crate::protocol::SessionListItem::Readable(summary)
                if summary.session.id == parent_id =>
            {
                Some(summary)
            }
            _ => None,
        })
        .expect("the parent is listed");
    assert_eq!(
        (
            listed.total_usage.and_then(|total| total.cost),
            listed.own_cost
        ),
        (Cost::from_usd(0.22), known(0.10, false)),
        "a listing reads the same two figures an open Session does"
    );
    assert_contributions_add_up(&store);
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn own_cost_counts_each_reporting_lifetime_once_and_every_turn_scoped_cost() {
    let workspace = tempfile::tempdir().unwrap();
    let unpriced = turn(16, Some(18), Some(5));
    for (turns, expected, case) in [
        (
            vec![
                priced_turn(Some(10), 0.05, subtree("process"), 12),
                priced_turn(Some(13), 0.08, subtree("process"), 20),
            ],
            known(0.08, false),
            "two cumulative reports in one lifetime read as the latest",
        ),
        (
            vec![
                priced_turn(Some(10), 0.08, subtree("first-process"), 20),
                priced_turn(Some(30), 0.03, subtree("second-process"), 40),
            ],
            known(0.11, false),
            "two lifetimes add",
        ),
        (
            vec![
                priced_turn(Some(10), 0.01, CostCoverage::Turn, 12),
                priced_turn(Some(13), 0.02, CostCoverage::Turn, 15),
            ],
            known(0.03, false),
            "Turn-scoped Costs add",
        ),
        (
            vec![
                priced_turn(Some(10), 0.01, CostCoverage::Turn, 12),
                unpriced.clone(),
            ],
            known(0.01, true),
            "an unpriced Turn leaves the own Cost partial",
        ),
    ] {
        let mut session = persisted(workspace.path(), None);
        session.snapshot.turns = turns;
        let id = session.snapshot.session.id;
        let (store, writer) = restore(workspace.path(), vec![session]).await;
        assert_eq!(
            costs(&store, id),
            Costs {
                tree: expected,
                own: expected,
            },
            "{case}"
        );
        assert_contributions_add_up(&store);
        writer.shutdown().await.unwrap();
    }
}

/// One Turn's counted Cost contribution, Reported, as a test states it.
fn counted(session_id: SessionId, turn: &Turn, usd: f64, is_partial: bool) -> CostContribution {
    CostContribution {
        session_id,
        turn_id: turn.id,
        amount: Some(CountedCost {
            cost: Cost::from_usd(usd).unwrap(),
            basis: CostBasis::Reported,
        }),
        is_partial,
    }
}

/// `actual` and `expected` hold the same contributions, in any order.
fn assert_same_contributions(
    mut actual: Vec<CostContribution>,
    expected: &[CostContribution],
    case: &str,
) {
    for contribution in expected {
        let position = actual
            .iter()
            .position(|found| found == contribution)
            .unwrap_or_else(|| panic!("{case}: {contribution:?} missing from {actual:?}"));
        actual.remove(position);
    }
    assert!(actual.is_empty(), "{case}: unexpected {actual:?}");
}

/// Every breakdown a Usage Metrics read can be asked for.
const BREAKDOWNS: [UsageMetricsBreakdown; 3] = [
    UsageMetricsBreakdown::Model,
    UsageMetricsBreakdown::Provider,
    UsageMetricsBreakdown::Workspace,
];

/// Usage Metrics over one bucket holding all of time, broken down by
/// `breakdown`.
fn all_time_metrics(
    store: &SessionStore,
    breakdown: UsageMetricsBreakdown,
) -> crate::protocol::UsageMetricsReport {
    store
        .usage_metrics(
            &UsageMetricsQuery {
                buckets: vec![UsageMetricsInterval {
                    start: SessionTimestamp(0),
                    end: SessionTimestamp(u64::MAX),
                }],
                breakdown,
            },
            &ModelCatalog {
                providers: Vec::new(),
            },
            &crate::protocol::WorkspacePaths::default(),
        )
        .expect("an all-time query is answered")
}

#[tokio::test]
async fn reading_usage_metrics_over_a_wide_forest_walks_each_tree_once() {
    let workspace = tempfile::tempdir().unwrap();
    let mut records = Vec::new();
    // 128 roots, each reporting its tree's running total over a native
    // Subagent beside a brokered one, and a Sidekick that began 64
    // Subsessions: 449 Sessions.
    for index in 0..128 {
        let mut root = persisted(workspace.path(), None);
        root.snapshot.turns = vec![priced_turn(
            Some(10),
            0.10,
            subtree(&format!("root-{index}")),
            30,
        )];
        let root_id = root.snapshot.session.id;
        let mut native = persisted(workspace.path(), Some(root_id));
        native.snapshot.turns = vec![priced_turn(Some(12), 0.05, CostCoverage::Turn, 14)];
        let mut brokered = persisted(workspace.path(), Some(root_id));
        brokered.brokered = true;
        brokered.snapshot.turns = vec![priced_turn(Some(12), 0.02, CostCoverage::Turn, 14)];
        records.extend([root, native, brokered]);
    }
    let mut sidekick = persisted(workspace.path(), None);
    sidekick.snapshot.turns = vec![priced_turn(Some(10), 0.01, CostCoverage::Turn, 11)];
    let begun_by = Some(crate::protocol::Author::Sidekick {
        session_id: sidekick.snapshot.session.id,
        title: "Sidekick".to_owned(),
    });
    records.push(sidekick);
    for _ in 0..64 {
        let mut subsession = persisted(workspace.path(), None);
        subsession.snapshot.session.begun_by = begun_by.clone();
        subsession.summary.session.begun_by = begun_by.clone();
        subsession.snapshot.turns = vec![priced_turn(Some(12), 0.01, CostCoverage::Turn, 13)];
        records.push(subsession);
    }
    let sessions = records.len();
    let (store, writer) = restore(workspace.path(), records).await;

    for breakdown in BREAKDOWNS {
        WORK.set(0);
        all_time_metrics(&store, breakdown);
        let work = WORK.get();
        assert!(
            work <= sessions * 8,
            "{sessions} Sessions required {work} operations to read by {breakdown:?}"
        );
    }
    let report = all_time_metrics(&store, UsageMetricsBreakdown::Model);
    assert_eq!(
        report.buckets[0].total.cost,
        Cost::from_usd(128.0 * 0.12 + 0.65),
        "every tree once: each root's report and its brokered Subagent, and \
         the Sidekick with its Subsessions"
    );
    let total = &report.buckets[0].total;
    assert_eq!(
        (total.sessions, total.subagent_sessions),
        (128 + 1 + 64, 128 * 2),
        "the roots, the Sidekick and its Subsessions top-level, and the \
         Subagents apart, each once"
    );
    assert_contributions_add_up(&store);
    writer.shutdown().await.unwrap();
}

/// The Usage Metrics invariant (ADR 0054): over a bucket holding all of time
/// and the untimed figures beside it, the Server's Cost is its root
/// Sessions' tree Costs added, each Session counted once — a Sidekick's
/// Subsessions beneath it, not again as roots of their own — and partial
/// exactly where one of those trees holds work no counted amount accounts
/// for, a tree whose Cost is wholly unknown included. The keys of every
/// breakdown divide that Cost without adding to it.
fn assert_all_time_metrics_add_up_to_root_tree_costs(store: &SessionStore) {
    for breakdown in BREAKDOWNS {
        assert_all_time_metrics_by_add_up_to_root_tree_costs(store, breakdown);
    }
}

fn assert_all_time_metrics_by_add_up_to_root_tree_costs(
    store: &SessionStore,
    breakdown: UsageMetricsBreakdown,
) {
    let report = all_time_metrics(store, breakdown);
    let added = |aggregates: &[UsageMetricsAggregate]| {
        (
            aggregates
                .iter()
                .filter_map(|aggregate| aggregate.cost)
                .reduce(Cost::saturating_add),
            aggregates.iter().any(|aggregate| aggregate.is_cost_partial),
        )
    };
    let all_time = added(&[report.buckets[0].total, report.untimed.total]);
    let by_key = added(
        &report
            .buckets
            .iter()
            .chain([&report.untimed])
            .flat_map(|bucket| bucket.by_key.iter().map(|keyed| keyed.aggregate))
            .collect::<Vec<_>>(),
    );

    let state = store.state.lock().unwrap();
    let roots = state
        .sessions
        .iter()
        .filter(|(_, record)| {
            record.snapshot.session.parent.is_none()
                && record
                    .summary
                    .session
                    .sidekick()
                    .is_none_or(|sidekick| !state.sessions.contains_key(&sidekick))
        })
        .map(|(id, _)| *id)
        .collect::<Vec<_>>();
    let tree_costs = (
        roots
            .iter()
            .filter_map(|id| state.sessions[id].snapshot.total_cost)
            .map(|total| total.cost)
            .reduce(Cost::saturating_add),
        roots.iter().any(|id| {
            state
                .tree_cost_contributions(*id)
                .iter()
                .any(|contribution| contribution.is_partial)
        }),
    );
    assert_eq!(
        all_time, tree_costs,
        "all of time costs what the root Sessions' trees do"
    );
    assert_eq!(
        by_key, all_time,
        "the keys by {breakdown:?} divide the Cost exactly"
    );
}

/// Restoration states, for every root it folded, the tree Cost the
/// contributions under that root add up to, and for every Session beneath,
/// the own Cost its own contributions add up to. The live roll-up reads both
/// as those sums, so this holds the startup fold to the same reading of Cost
/// Coverage; `costs` holds it there for any Session a test names.
fn assert_contributions_add_up(store: &SessionStore) {
    assert_all_time_metrics_add_up_to_root_tree_costs(store);
    let state = store.state.lock().unwrap();
    let mut walk = state
        .sessions
        .iter()
        .filter(|(_, record)| record.snapshot.session.parent.is_none())
        .map(|(id, _)| *id)
        .collect::<Vec<_>>();
    while let Some(id) = walk.pop() {
        walk.extend(
            state
                .sessions
                .iter()
                .filter(|(_, record)| record.snapshot.session.parent == Some(id))
                .map(|(child, _)| *child),
        );
        let snapshot = &state.sessions[&id].snapshot;
        if snapshot.session.parent.is_none() {
            assert_eq!(
                sum_contributions(&state.tree_cost_contributions(id)),
                snapshot.total_cost,
                "tree Cost of the root {id}"
            );
        }
        assert_eq!(
            sum_contributions(&state.own_cost_contributions(id)),
            snapshot.own_cost,
            "own Cost of {id}"
        );
    }
}

#[tokio::test]
async fn running_total_reports_contribute_their_increments_and_turn_scoped_costs_their_own() {
    let workspace = tempfile::tempdir().unwrap();
    let reports = |first: (f64, &str, u64), second: (f64, &str, u64)| {
        vec![
            priced_turn(Some(10), first.0, subtree(first.1), first.2),
            priced_turn(Some(13), second.0, subtree(second.1), second.2),
        ]
    };
    for (turns, increments, expected, case) in [
        (
            reports((0.05, "process", 12), (0.08, "process", 20)),
            [(0.05, false), (0.03, false)],
            known(0.08, false),
            "two running totals in one lifetime: the first amount, then the increase",
        ),
        (
            reports((0.08, "first-process", 12), (0.03, "second-process", 20)),
            [(0.08, false), (0.03, false)],
            known(0.11, false),
            "two lifetimes add",
        ),
        (
            reports((0.08, "process", 12), (0.05, "process", 20)),
            [(0.08, false), (0.0, true)],
            known(0.08, true),
            "a report lower than the one before it adds nothing, and says so",
        ),
        (
            vec![
                priced_turn(Some(10), 0.01, CostCoverage::Turn, 12),
                priced_turn(Some(13), 0.02, CostCoverage::Turn, 15),
            ],
            [(0.01, false), (0.02, false)],
            known(0.03, false),
            "Turn-scoped Costs contribute whole",
        ),
    ] {
        let mut session = persisted(workspace.path(), None);
        session.snapshot.turns = turns.clone();
        let id = session.snapshot.session.id;
        let (store, writer) = restore(workspace.path(), vec![session]).await;
        let expected_contributions = turns
            .iter()
            .zip(increments)
            .map(|(turn, (usd, is_partial))| counted(id, turn, usd, is_partial))
            .collect::<Vec<_>>();
        {
            let state = store.state.lock().unwrap();
            assert_same_contributions(
                state.own_cost_contributions(id),
                &expected_contributions,
                case,
            );
            assert_same_contributions(
                state.tree_cost_contributions(id),
                &expected_contributions,
                case,
            );
        }
        assert_eq!(
            costs(&store, id),
            Costs {
                tree: expected,
                own: expected,
            },
            "{case}"
        );
        assert_contributions_add_up(&store);
        writer.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn a_covered_native_subagent_contributes_nothing_and_a_brokered_one_its_own_cost() {
    let workspace = tempfile::tempdir().unwrap();
    for brokered in [false, true] {
        let mut parent = persisted(workspace.path(), None);
        parent.snapshot.turns = vec![priced_turn(Some(10), 0.10, subtree("parent-process"), 20)];
        let parent_id = parent.snapshot.session.id;
        let mut child = persisted(workspace.path(), Some(parent_id));
        child.brokered = brokered;
        child.snapshot.turns = vec![priced_turn(Some(12), 0.05, CostCoverage::Turn, 15)];
        let child_id = child.snapshot.session.id;
        let parent_counted = counted(parent_id, &parent.snapshot.turns[0], 0.10, false);
        let child_counted = counted(child_id, &child.snapshot.turns[0], 0.05, false);
        let (store, writer) = restore(workspace.path(), vec![parent, child]).await;
        {
            let state = store.state.lock().unwrap();
            let expected = if brokered {
                vec![parent_counted, child_counted]
            } else {
                vec![parent_counted]
            };
            assert_same_contributions(
                state.tree_cost_contributions(parent_id),
                &expected,
                &format!("beneath the parent, brokered: {brokered}"),
            );
            assert_same_contributions(
                state.own_cost_contributions(child_id),
                &[child_counted],
                "the child's own Cost counts its own Turn either way",
            );
        }
        assert_contributions_add_up(&store);
        writer.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn a_contribution_carries_its_cost_basis() {
    let workspace = tempfile::tempdir().unwrap();
    let mut session = persisted(workspace.path(), None);
    let reported = priced_turn(Some(10), 0.02, CostCoverage::Turn, 12);
    let mut estimated = priced_turn(Some(13), 0.03, CostCoverage::Turn, 15);
    estimated.cost_basis = Some(CostBasis::Estimated);
    session.snapshot.turns = vec![reported.clone(), estimated.clone()];
    let id = session.snapshot.session.id;
    let (store, writer) = restore(workspace.path(), vec![session]).await;
    let mut estimated_counted = counted(id, &estimated, 0.03, false);
    estimated_counted.amount = estimated_counted.amount.map(|amount| CountedCost {
        basis: CostBasis::Estimated,
        ..amount
    });
    assert_same_contributions(
        store.state.lock().unwrap().tree_cost_contributions(id),
        &[counted(id, &reported, 0.02, false), estimated_counted],
        "each Turn's Basis rides with its amount",
    );
    assert_contributions_add_up(&store);
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn reading_contributions_moves_no_revision_and_changes_no_turn() {
    let workspace = tempfile::tempdir().unwrap();
    let mut parent = persisted(workspace.path(), None);
    parent.snapshot.turns = vec![
        priced_turn(Some(10), 0.05, subtree("parent-process"), 12),
        priced_turn(Some(13), 0.08, subtree("parent-process"), 20),
    ];
    let parent_id = parent.snapshot.session.id;
    let mut child = persisted(workspace.path(), Some(parent_id));
    child.snapshot.turns = vec![priced_turn(Some(14), 0.01, CostCoverage::Turn, 16)];
    let child_id = child.snapshot.session.id;
    let (store, writer) = restore(workspace.path(), vec![parent, child]).await;
    let before = [parent_id, child_id].map(|id| store.subscribe(id).unwrap().snapshot);
    {
        let state = store.state.lock().unwrap();
        for id in [parent_id, child_id] {
            state.tree_cost_contributions(id);
            state.own_cost_contributions(id);
        }
    }
    let after = [parent_id, child_id].map(|id| store.subscribe(id).unwrap().snapshot);
    for (before, after) in before.iter().zip(&after) {
        assert_eq!(before.revision, after.revision);
        assert_eq!(before.turns, after.turns);
    }
    assert_contributions_add_up(&store);
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_prior_record_from_an_earlier_lifetime_contributes_beside_the_current_one() {
    let workspace = tempfile::tempdir().unwrap();
    let mut session = persisted(workspace.path(), None);
    let mut reconnected = priced_turn(Some(10), 0.06, subtree("second-process"), 20);
    reconnected
        .cost_details
        .as_mut()
        .unwrap()
        .prior
        .push(crate::protocol::CostRecord {
            cost: Cost::from_usd(0.04).unwrap(),
            basis: CostBasis::Reported,
            coverage: subtree("first-process"),
            recorded_at: SessionTimestamp(12),
            is_partial: false,
        });
    let later = priced_turn(Some(22), 0.09, subtree("second-process"), 25);
    session.snapshot.turns = vec![reconnected.clone(), later.clone()];
    let id = session.snapshot.session.id;
    let (store, writer) = restore(workspace.path(), vec![session]).await;
    assert_same_contributions(
        store.state.lock().unwrap().own_cost_contributions(id),
        &[
            counted(id, &reconnected, 0.04, false),
            counted(id, &reconnected, 0.06, false),
            counted(id, &later, 0.03, false),
        ],
        "the earlier lifetime's record counts whole, and the later lifetime by its increments",
    );
    assert_eq!(
        costs(&store, id),
        Costs {
            tree: known(0.13, false),
            own: known(0.13, false),
        }
    );
    assert_contributions_add_up(&store);
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_complete_running_total_settles_an_earlier_partial_report_in_its_lifetime() {
    let workspace = tempfile::tempdir().unwrap();
    let mut earlier = priced_turn(Some(10), 0.05, subtree("process"), 12);
    // A later Usage update with no Cost left this report partial.
    earlier.cost_details.as_mut().unwrap().is_partial = true;
    let later = priced_turn(Some(13), 0.08, subtree("process"), 20);
    for (turns, expected, case) in [
        (
            vec![earlier.clone(), later.clone()],
            known(0.08, false),
            "a later complete total restates the whole lifetime",
        ),
        (
            vec![later.clone(), {
                let mut latest = earlier.clone();
                latest.started_at = Some(SessionTimestamp(21));
                latest.cost_details.as_mut().unwrap().recorded_at = SessionTimestamp(22);
                latest.cost = Cost::from_usd(0.09);
                latest
            }],
            known(0.09, true),
            "a partial latest total leaves the lifetime partial",
        ),
    ] {
        let mut session = persisted(workspace.path(), None);
        session.snapshot.turns = turns;
        let id = session.snapshot.session.id;
        let (store, writer) = restore(workspace.path(), vec![session]).await;
        assert_eq!(
            costs(&store, id),
            Costs {
                tree: expected,
                own: expected,
            },
            "{case}"
        );
        assert_contributions_add_up(&store);
        writer.shutdown().await.unwrap();
    }
}
