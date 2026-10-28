//! Insights read from a restored Session forest, as the next process finds
//! it: every Session's Turns in memory, and nothing yet read since.

use super::{
    InsightsRefusal, Pairing, RemoteAct, SessionStore,
    restoration_tests::persisted,
    usage_metrics_tests::{
        CODEX, FABLE, OPUS, PAIRING, STUDIO, by_model, metered, read as read_metrics, restore,
        restore_deferred, running_total, turn_scoped, usd,
    },
};
use crate::{
    protocol::{
        Author, Cost, InsightsQuery, InsightsReport, QUARTER_HOUR_MS, QuarterHourTurns,
        SessionListItem, SessionTimestamp, Turn, TurnDurations, TurnInsights, TurnStatus,
        UsageMetricsInterval, UsageTotal,
    },
    sidekick::SidekickWorkspace,
};

/// Insights over the half-open span from `start` up to `end`.
fn over(start: u64, end: u64) -> InsightsQuery {
    InsightsQuery {
        interval: UsageMetricsInterval {
            start: SessionTimestamp(start),
            end: SessionTimestamp(end),
        },
    }
}

fn read(store: &SessionStore, query: &InsightsQuery) -> InsightsReport {
    store
        .usage_insights(query)
        .expect("a well-formed query is answered")
}

/// A Turn that ended as `status`, begun at `started_at` and Settled at
/// `settled_at`, either absent where it is `None`.
fn ended(status: TurnStatus, started_at: Option<u64>, settled_at: Option<u64>) -> Turn {
    Turn {
        status,
        ..metered(FABLE, started_at, settled_at, 1)
    }
}

/// A Completed Turn begun at `start` that took `seconds` to Settle.
fn took(start: u64, seconds: u64) -> Turn {
    ended(
        TurnStatus::Completed,
        Some(start),
        Some(start + seconds * 1_000),
    )
}

const START: u64 = 1_000_000;
const END: u64 = 100_000_000;

#[tokio::test]
async fn turn_durations_are_read_from_settled_turns_with_both_moments_in_the_interval() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = persisted(directory.path(), None);
    let at = |offset: u64| START + offset;
    session.snapshot.turns = vec![
        took(at(0), 5),
        ended(TurnStatus::Failed, Some(at(1)), Some(at(1) + 9_000)),
        took(at(2), 12),
        took(at(3), 20),
        took(at(4), 45),
        took(at(5), 90),
        took(at(6), 200),
        took(at(7), 600),
        took(at(8), 1_200),
        ended(
            TurnStatus::Interrupted,
            Some(at(9)),
            Some(at(9) + 3_600_000),
        ),
        // Still working: neither a duration nor an ending yet, however long
        // a Settle a stale record says it had.
        ended(TurnStatus::Active, Some(at(10)), None),
        ended(TurnStatus::Active, Some(at(11)), Some(at(11) + 1_000)),
        // Ended, but without both moments, so with no duration to give.
        ended(TurnStatus::Failed, Some(at(12)), None),
        ended(TurnStatus::Interrupted, None, Some(at(13))),
        // Untimed, so in no interval at all.
        ended(TurnStatus::Completed, None, None),
        // Begun before the interval, though it Settled in it.
        ended(TurnStatus::Completed, Some(START - 1), Some(at(14))),
        // Begun as the interval ends, which it does not hold.
        took(END, 1),
    ];
    let (store, writer) = restore_deferred(directory.path(), vec![session]).await;

    let report = read(&store, &over(START, END));

    assert_eq!(
        report.turns,
        TurnInsights {
            durations: Some(TurnDurations {
                // Ten durations: the fifth and ninth shortest.
                median_ms: 45_000,
                p90_ms: 1_200_000,
                longest_ms: 3_600_000,
                bins: [2, 2, 1, 1, 1, 1, 1, 1],
            }),
            completed: 8,
            failed: 2,
            interrupted: 2,
        }
    );
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_duration_falls_in_the_bin_its_bound_begins() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = persisted(directory.path(), None);
    session.snapshot.turns = [10, 30, 60, 120, 300, 900, 1_800]
        .into_iter()
        .enumerate()
        .map(|(offset, seconds)| took(START + offset as u64, seconds))
        .collect();
    let (store, writer) = restore(directory.path(), vec![session]).await;

    let durations = read(&store, &over(START, END)).turns.durations;

    assert_eq!(
        durations.map(|durations| durations.bins),
        Some([0, 1, 1, 1, 1, 1, 1, 1])
    );
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_interval_with_no_timed_settled_turn_has_no_durations() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = persisted(directory.path(), None);
    session.snapshot.turns = vec![
        ended(TurnStatus::Active, Some(START), None),
        ended(TurnStatus::Completed, None, None),
    ];
    let (store, writer) = restore_deferred(directory.path(), vec![session]).await;

    assert_eq!(
        read(&store, &over(START, END)).turns,
        TurnInsights::default()
    );
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_interval_that_runs_backwards_is_refused_and_an_empty_one_is_not() {
    let directory = tempfile::tempdir().unwrap();
    let (store, writer) = restore(directory.path(), Vec::new()).await;

    assert_eq!(
        store.usage_insights(&over(END, START)),
        Err(InsightsRefusal::Backwards)
    );
    assert_eq!(
        store.usage_insights(&over(START, START)),
        Ok(InsightsReport::default())
    );
    writer.shutdown().await.unwrap();
}

/// The Sessions the metrics read counts and no others: a Sidekick, its
/// Subagents native and brokered, and its Subsession; not a Subagent whose
/// parent is gone, which is in no tree Cost; and not the Session the
/// Sidekick began on a Remote, which that Remote counts. The store holds
/// such a Session only as its listing, apart from the Sessions living here
/// (ADR 0049) and with no Turns to count, so leaving it out is structural:
/// the test pins that its listing reaches the Sidekick's tree and still
/// adds nothing.
#[tokio::test]
async fn the_sessions_the_metrics_read_counts_are_read_and_a_remotes_are_not() {
    let directory = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let sidekick_workspace = SidekickWorkspace::beside(data.path()).unwrap();
    let mut sidekick = persisted(&sidekick_workspace.ensure().unwrap(), None);
    sidekick.snapshot.turns = vec![took(START, 1)];
    let sidekick_id = sidekick.snapshot.session.id;
    let mut subagent = persisted(directory.path(), Some(sidekick_id));
    subagent.snapshot.turns = vec![ended(TurnStatus::Failed, Some(START), None)];
    let mut brokered = persisted(directory.path(), Some(sidekick_id));
    brokered.brokered = true;
    brokered.snapshot.turns = vec![ended(TurnStatus::Interrupted, Some(START), None)];
    let begun_by = Some(Author::Sidekick {
        session_id: sidekick_id,
        title: "Sidekick".to_owned(),
    });
    let mut subsession = persisted(directory.path(), None);
    subsession.snapshot.session.begun_by = begun_by.clone();
    subsession.summary.session.begun_by = begun_by;
    subsession.snapshot.turns = vec![took(START, 1)];
    let mut orphan = persisted(directory.path(), Some(crate::protocol::SessionId::new()));
    orphan.snapshot.turns = vec![took(START, 1)];
    let (store, writer) = restore(
        directory.path(),
        vec![sidekick, subagent, brokered, subsession, orphan],
    )
    .await;
    let store = store.with_sidekicks(sidekick_workspace, Vec::new());
    let remote_session = persisted(directory.path(), None);
    let remote_id = remote_session.summary.session.id;
    store.record_remote_sidekick_act(
        sidekick_id,
        STUDIO,
        remote_id,
        RemoteAct {
            began: true,
            resolved: true,
            confirmed: true,
            pairing: PAIRING.to_owned(),
            beginning: None,
            evidence: None,
        },
    );
    let mut remote_summary = remote_session.summary;
    remote_summary.total_usage = Some(UsageTotal {
        output_tokens: Some(1_000_000),
        ..Default::default()
    });
    store.remote_read(
        STUDIO,
        &Pairing {
            fingerprint: PAIRING.to_owned(),
            generation: 1,
        },
        vec![SessionListItem::Readable(Box::new(remote_summary))],
        store.moment(),
    );
    assert_eq!(
        store.remotes_listed_with(sidekick_id),
        vec![STUDIO.to_owned()],
        "the Sidekick's tree lists the Remote's Session"
    );

    let turns = read(&store, &over(START, END)).turns;

    assert_eq!(
        (turns.completed, turns.failed, turns.interrupted),
        (2, 1, 1),
        "the Sidekick and its Subsession, and the two Subagents"
    );
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn reading_insights_never_hydrates_a_deferred_session_and_moves_no_revision() {
    let directory = tempfile::tempdir().unwrap();
    let mut root = persisted(directory.path(), None);
    root.snapshot.turns = vec![took(START, 1)];
    let root_id = root.snapshot.session.id;
    let (store, writer) = restore_deferred(directory.path(), vec![root]).await;
    let read_state = |store: &SessionStore| {
        let state = store.state.lock().unwrap();
        (
            state.is_deferred(root_id),
            state.sessions[&root_id].snapshot.revision,
        )
    };
    let before = read_state(&store);
    assert!(before.0, "the history opens deferred");

    let turns = read(&store, &over(START, END)).turns;

    assert_eq!(turns.completed, 1);
    assert_eq!(read_state(&store), before, "no history was read in");
    writer.shutdown().await.unwrap();
}

/// A quarter-hour beginning on a whole quarter-hour since the epoch, from
/// which the quarter-hour tests count.
const QUARTER: u64 = 1_800_000_000_000;

/// The quarter-hour `place` quarter-hours after [`QUARTER`] holding `turns`
/// Turns of `tokens` tokens, costing `cost`.
fn quarter(
    place: u64,
    turns: u64,
    tokens: u64,
    cost: Option<Cost>,
    partial: bool,
) -> QuarterHourTurns {
    QuarterHourTurns {
        start: SessionTimestamp(QUARTER + place * QUARTER_HOUR_MS),
        turns,
        tokens: Some(tokens),
        cost,
        is_cost_partial: partial,
    }
}

#[tokio::test]
async fn turns_land_in_the_utc_quarter_hour_they_are_placed_in_earliest_first() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = persisted(directory.path(), None);
    session.snapshot.turns = vec![
        // The last moment of the third quarter-hour, listed first.
        metered(FABLE, Some(QUARTER + 3 * QUARTER_HOUR_MS - 1), None, 100),
        // The first moment of the first, and the last of it.
        metered(FABLE, Some(QUARTER), Some(QUARTER + 5 * QUARTER_HOUR_MS), 1),
        metered(FABLE, Some(QUARTER + QUARTER_HOUR_MS - 1), None, 2),
        // Never said when it began: its Settle places it, as it places it in
        // the range.
        metered(FABLE, None, Some(QUARTER + 2 * QUARTER_HOUR_MS), 10),
        // Untimed, and begun before the interval: in no quarter-hour.
        metered(FABLE, None, None, 1_000),
        metered(FABLE, Some(QUARTER - 1), None, 1_000),
    ];
    let (store, writer) = restore_deferred(directory.path(), vec![session]).await;

    let report = read(&store, &over(QUARTER, QUARTER + 4 * QUARTER_HOUR_MS));

    assert_eq!(
        report.quarter_hours,
        vec![quarter(0, 2, 3, None, true), quarter(2, 2, 110, None, true),],
        "only quarter-hours a Turn is placed in, each Turn's Cost unknown"
    );
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn quarter_hour_costs_add_up_to_the_range_s_cost() {
    let directory = tempfile::tempdir().unwrap();
    let at = |place: u64| QUARTER + place * QUARTER_HOUR_MS + 1;
    let mut parent = persisted(directory.path(), None);
    // A running total's increments land on the Turns carrying them.
    parent.snapshot.turns = vec![
        running_total(OPUS, at(0), 0.10, "first-process"),
        running_total(OPUS, at(1), 0.25, "first-process"),
        running_total(OPUS, at(1) + 5, 0.40, "first-process"),
    ];
    let parent_id = parent.snapshot.session.id;
    // A brokered Subagent's own Cost is its own, apart from its parent's.
    let mut brokered = persisted(directory.path(), Some(parent_id));
    brokered.brokered = true;
    brokered.snapshot.turns = vec![
        turn_scoped(CODEX, at(3), 0.08),
        // Unpriced: its quarter-hour's Cost is partial.
        metered(CODEX, Some(at(3) + 7), None, 1),
    ];
    let (store, writer) = restore(directory.path(), vec![parent, brokered]).await;
    let (start, end) = (QUARTER, QUARTER + 4 * QUARTER_HOUR_MS);

    let report = read(&store, &over(start, end));

    assert_eq!(
        report.quarter_hours,
        vec![
            quarter(0, 1, 1, usd(0.10), false),
            quarter(1, 2, 2, usd(0.30), false),
            quarter(3, 2, 2, usd(0.08), true),
        ]
    );
    let range = read_metrics(&store, &by_model(&[(start, end)]))
        .across_buckets
        .total;
    let added = report
        .quarter_hours
        .iter()
        .filter_map(|quarter| quarter.cost)
        .fold(None, |total: Option<Cost>, cost| {
            Some(Cost::from_nano_usd(
                total.map_or(0, Cost::nano_usd) + cost.nano_usd(),
            ))
        });
    assert_eq!(
        (
            added,
            report.quarter_hours.iter().any(|q| q.is_cost_partial)
        ),
        (range.cost, range.is_cost_partial),
        "the quarter-hours add up to the Cost the metrics read gives the range"
    );
    writer.shutdown().await.unwrap();
}
