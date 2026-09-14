//! Restoration is measured at SessionStore::new, before any client can read it.

use std::{cell::Cell, collections::HashMap, path::Path};

use super::SessionStore;
use crate::{
    protocol::{
        Cost, CostBasis, CostCoverage, CostDetails, ModelAvailability, Session, SessionId,
        SessionRevision, SessionSnapshot, SessionStandingInputs, SessionStatus, SessionSummary,
        SessionTimestamp, Turn, TurnId, TurnStatus, Usage, UsageTotal, Workspace,
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
    let (writer, sink) = StorageWriter::spawn(repository, &[]);

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

    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn restored_working_bridges_settled_ancestors_without_spending_revisions_or_writes() {
    let directory = tempfile::tempdir().unwrap();
    let repository = StorageRepository::open(directory.path()).await.unwrap();
    let (writer, sink) = StorageWriter::spawn(repository.clone(), &[]);
    let mut root = persisted(directory.path(), None);
    root.snapshot.turns = vec![turn(10, Some(20), None), turn(40, Some(50), Some(0))];
    let mut child = persisted(directory.path(), Some(root.snapshot.session.id));
    child.snapshot.turns = vec![turn(18, Some(42), Some(3))];
    let mut leaf = persisted(directory.path(), Some(child.snapshot.session.id));
    leaf.snapshot.turns = vec![turn(49, None, Some(5))];
    let mut sibling = persisted(directory.path(), Some(root.snapshot.session.id));
    sibling.snapshot.turns = vec![turn(1, Some(5), None)];
    let mut unknown = persisted(directory.path(), None);
    let mut untimed = turn(0, None, None);
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
    );
    for (index, (since, output)) in [
        (Some(10), Some(8)),
        (Some(49), Some(8)),
        (Some(49), Some(5)),
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

async fn restore_tree(wide: bool) {
    let directory = tempfile::tempdir().unwrap();
    let repository = StorageRepository::open(directory.path()).await.unwrap();
    let (writer, sink) = StorageWriter::spawn(repository, &[]);
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
        record.snapshot.turns = vec![turn(start, (index != 511).then_some(start + 1), Some(1))];
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
    );
    let work = WORK.get();
    assert!(
        work <= 512 * 16,
        "512 Sessions/Turns required {work} operations (wide={wide})"
    );
    let snapshot = store.subscribe(root).unwrap().snapshot;
    assert_eq!(snapshot.total_usage().unwrap().output_tokens, Some(512));
    assert_eq!(snapshot.subagent_usage.unwrap().output_tokens, Some(511));
    assert_eq!(
        snapshot.working_since(),
        Some(SessionTimestamp(if wide { 10 } else { 1543 }))
    );
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
    let (writer, sink) = StorageWriter::spawn(repository, &[]);
    let mut record = persisted(directory.path(), None);
    record.snapshot.turns = (0..256)
        .map(|index| {
            let started_at = SessionTimestamp(index * 4 + 1);
            Turn {
                id: TurnId::new(),
                prompt_id: None,
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
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn restored_balanced_tree_matches_the_durable_interval_union() {
    let directory = tempfile::tempdir().unwrap();
    let repository = StorageRepository::open(directory.path()).await.unwrap();
    let (writer, sink) = StorageWriter::spawn(repository, &[]);
    let mut records = vec![persisted(directory.path(), None)];
    for index in 1..127 {
        records.push(persisted(
            directory.path(),
            Some(records[(index - 1) / 2].snapshot.session.id),
        ));
    }
    for (index, record) in records.iter_mut().enumerate() {
        // Repeated starts, touching ends, gaps, nested overlaps and old Turns
        // without timing. Some branches have no live work at all.
        let start = (index as u64 * 17) % 83;
        record.snapshot.turns = vec![
            turn(start, Some(start + 5), Some(0)),
            turn(start + 20, (index % 7 != 0).then_some(start + 23), Some(2)),
        ];
        let mut untimed = turn(0, None, None);
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
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn malformed_relationships_do_not_promote_children_into_listed_roots() {
    let directory = tempfile::tempdir().unwrap();
    let repository = StorageRepository::open(directory.path()).await.unwrap();
    let (writer, sink) = StorageWriter::spawn(repository, &[]);
    let root = persisted(directory.path(), None);
    let orphan = persisted(directory.path(), Some(SessionId::new()));
    let mut cycle = persisted(directory.path(), None);
    let other = persisted(directory.path(), Some(cycle.snapshot.session.id));
    cycle.snapshot.session.parent = Some(other.snapshot.session.id);
    cycle.summary.session.parent = cycle.snapshot.session.parent;
    let root_id = root.snapshot.session.id;
    let store = SessionStore::new(
        RestoredSessions {
            readable: vec![root, orphan, cycle, other],
            ..Default::default()
        },
        sink,
        Vec::new(),
    );
    store.reconcile_approval_postures(&crate::protocol::EffectiveSettings::default());
    assert_eq!(store.list(None).len(), 1);
    assert!(store.subscribe(root_id).is_some());
    writer.shutdown().await.unwrap();
}

pub(super) fn record_work(count: usize) {
    WORK.set(WORK.get() + count);
}

fn persisted(workspace: &Path, parent: Option<SessionId>) -> PersistedSession {
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
        parent,
    };
    PersistedSession {
        summary: SessionSummary {
            checkout_state: None,
            session: session.clone(),
            title: "restoration fixture".into(),
            settled_at: None,
            standing_inputs: SessionStandingInputs::default(),
            total_usage: None,
            created_at: SessionTimestamp(1),
            updated_at: SessionTimestamp(2),
        },
        snapshot: SessionSnapshot {
            title: "restoration fixture".into(),
            session,
            revision: SessionRevision(7),
            prompts: vec![],
            turns: vec![],
            messages: vec![],
            activities: vec![],
            transcript: vec![],
            subagent_usage: None,
            total_cost: None,
            subagent_interventions: vec![],
            pending_approvals: Vec::new(),
            submitting_approvals: Vec::new(),
            pending_approvals_revision: crate::protocol::SessionRevision(0),
        },
        resume_states: HashMap::new(),
    }
}

#[tokio::test]
async fn restoring_independent_roots_does_linear_relationship_work() {
    let directory = tempfile::tempdir().unwrap();
    let repository = StorageRepository::open(directory.path()).await.unwrap();
    let (writer, sink) = StorageWriter::spawn(repository, &[]);
    let restored = RestoredSessions {
        readable: (0..128)
            .map(|_| persisted(directory.path(), None))
            .collect(),
        ..Default::default()
    };
    WORK.set(0);
    let store = SessionStore::new(restored, sink, Vec::new());
    let work = WORK.get();
    assert_eq!(store.list(None).len(), 128);
    assert!(
        work <= 128 * 8,
        "128 roots required {work} restoration operations"
    );
    writer.shutdown().await.unwrap();
}
