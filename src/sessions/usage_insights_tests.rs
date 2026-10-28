//! Insights read from a restored Session forest, as the next process finds
//! it: every Session's Turns in memory, and nothing yet read since.

use super::{
    InsightsRefusal, Pairing, RemoteAct, SessionStore,
    restoration_tests::persisted,
    usage_metrics_tests::{
        CODEX, FABLE, OPUS, PAIRING, SONNET, STUDIO, by_model, metered, read as read_metrics,
        restore, restore_deferred, running_total, turn_scoped, usd,
    },
};
use crate::{
    protocol::{
        Activity, ActivityId, ActivityStatus, Approval, ApprovalId, ApprovalInsights,
        ApprovalOutcome, ApprovalSubject, Author, CompactionCounts, CompactionTrigger, Cost,
        Decision, DelegationInsights, FreedShare, InsightsQuery, InsightsReport, InsightsTool,
        QUARTER_HOUR_MS, QuarterHourTurns, RoleCost, SessionId, SessionListItem, SessionTimestamp,
        ToolTally, TranscriptItem, Turn, TurnDurations, TurnId, TurnInsights, TurnStatus,
        UsageMetricsInterval, UsageTotal,
    },
    sidekick::SidekickWorkspace,
    storage::PersistedSession,
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

async fn read(store: &SessionStore, query: &InsightsQuery) -> InsightsReport {
    store
        .usage_insights(query)
        .await
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

    let report = read(&store, &over(START, END)).await;

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

    let durations = read(&store, &over(START, END)).await.turns.durations;

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
        read(&store, &over(START, END)).await.turns,
        TurnInsights::default()
    );
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_interval_that_runs_backwards_is_refused_and_an_empty_one_is_not() {
    let directory = tempfile::tempdir().unwrap();
    let (store, writer) = restore(directory.path(), Vec::new()).await;

    assert_eq!(
        store.usage_insights(&over(END, START)).await,
        Err(InsightsRefusal::Backwards)
    );
    assert_eq!(
        store.usage_insights(&over(START, START)).await,
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

    let turns = read(&store, &over(START, END)).await.turns;

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

    let turns = read(&store, &over(START, END)).await.turns;

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

    let report = read(&store, &over(QUARTER, QUARTER + 4 * QUARTER_HOUR_MS)).await;

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

    let report = read(&store, &over(start, end)).await;

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

/// What the Sessions of one role cost.
fn role(cost: Option<Cost>, partial: bool) -> RoleCost {
    RoleCost {
        cost,
        is_cost_partial: partial,
    }
}

/// `session`, begun by the Sidekick's Session `sidekick`: a Subsession.
fn begun_by(session: &mut PersistedSession, sidekick: SessionId) {
    let author = Some(Author::Sidekick {
        session_id: sidekick,
        title: "Sidekick".to_owned(),
    });
    session.snapshot.session.begun_by = author.clone();
    session.summary.session.begun_by = author;
}

#[tokio::test]
async fn delegation_divides_the_range_s_cost_by_the_role_of_the_session_carrying_it() {
    let directory = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let at = |offset: u64| START + offset * 1_000;
    // A Session of the user's own, whose Cost is its Turns' own.
    let mut own = persisted(directory.path(), None);
    own.snapshot.turns = vec![
        turn_scoped(CODEX, at(0), 0.50),
        // Before the range: its Cost is no part of the range's.
        turn_scoped(CODEX, START - 10, 9.00),
    ];
    let own_id = own.snapshot.session.id;
    // A native Subagent whose Cost no report above it covers is its own.
    let mut native = persisted(directory.path(), Some(own_id));
    native.snapshot.turns = vec![turn_scoped(CODEX, at(1), 0.05)];
    // A brokered Subagent's Cost is its own, apart from its parent's, and
    // work of it nobody priced leaves its role's Cost partial.
    let mut brokered = persisted(directory.path(), Some(own_id));
    brokered.brokered = true;
    brokered.snapshot.turns = vec![
        turn_scoped(CODEX, at(2), 0.08),
        metered(CODEX, Some(at(3)), None, 1),
    ];
    // A Sidekick, the Subagent it delegated to, and the Subsession it
    // began, whose own native Subagent is a Subagent like any other.
    let sidekick_workspace = SidekickWorkspace::beside(data.path()).unwrap();
    let mut sidekick = persisted(&sidekick_workspace.ensure().unwrap(), None);
    sidekick.snapshot.turns = vec![turn_scoped(CODEX, at(4), 0.02)];
    let sidekick_id = sidekick.snapshot.session.id;
    let mut sidekicks_subagent = persisted(directory.path(), Some(sidekick_id));
    sidekicks_subagent.snapshot.turns = vec![turn_scoped(CODEX, at(5), 0.03)];
    let mut subsession = persisted(directory.path(), None);
    begun_by(&mut subsession, sidekick_id);
    subsession.snapshot.turns = vec![turn_scoped(CODEX, at(6), 0.01)];
    let subsession_id = subsession.snapshot.session.id;
    let mut subsessions_subagent = persisted(directory.path(), Some(subsession_id));
    subsessions_subagent.snapshot.turns = vec![turn_scoped(CODEX, at(7), 0.04)];
    let (store, writer) = restore(
        directory.path(),
        vec![
            own,
            native,
            brokered,
            sidekick,
            sidekicks_subagent,
            subsession,
            subsessions_subagent,
        ],
    )
    .await;
    let store = store.with_sidekicks(sidekick_workspace, Vec::new());

    let delegation = read(&store, &over(START, END)).await.delegation;

    assert_eq!(
        delegation,
        DelegationInsights {
            own_sessions: role(usd(0.50), false),
            native_subagents: role(usd(0.12), false),
            brokered_subagents: role(usd(0.08), true),
            sidekicks: role(usd(0.02), false),
            subsessions: role(usd(0.01), false),
        }
    );
    let range = read_metrics(&store, &by_model(&[(START, END)]))
        .across_buckets
        .total;
    assert_eq!(
        delegation.total(),
        role(range.cost, range.is_cost_partial),
        "the roles add up to the Cost the metrics read gives the range"
    );
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_claude_native_subagent_s_cost_is_inside_its_parent_s_own_sessions_share() {
    let directory = tempfile::tempdir().unwrap();
    // Claude's whole-tree report covers the native Subagent it ran.
    let mut parent = persisted(directory.path(), None);
    parent.snapshot.turns = vec![running_total(OPUS, START, 0.30, "parent-process")];
    parent.snapshot.turns[0]
        .cost_details
        .as_mut()
        .unwrap()
        .recorded_at = SessionTimestamp(START + 30);
    let parent_id = parent.snapshot.session.id;
    let mut native = persisted(directory.path(), Some(parent_id));
    native.snapshot.turns = vec![turn_scoped(SONNET, START + 12, 0.05)];
    let (store, writer) = restore(directory.path(), vec![parent, native]).await;

    let delegation = read(&store, &over(START, END)).await.delegation;

    assert_eq!(
        delegation,
        DelegationInsights {
            own_sessions: role(usd(0.30), false),
            ..DelegationInsights::default()
        },
        "the native Subagent's own Cost is inside its parent's report"
    );
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_range_whose_turns_carry_no_cost_divides_none() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = persisted(directory.path(), None);
    session.snapshot.turns = vec![metered(FABLE, Some(START), None, 1)];
    let (store, writer) = restore(directory.path(), vec![session]).await;

    let delegation = read(&store, &over(START, END)).await.delegation;

    assert_eq!(
        delegation,
        DelegationInsights {
            own_sessions: role(None, true),
            ..DelegationInsights::default()
        },
        "work nobody priced is partial, with no amount anyone measured"
    );
    writer.shutdown().await.unwrap();
}

/// Records `activity` in `session`'s Transcript, as working the Turn it
/// names recorded it.
fn record(session: &mut PersistedSession, activity: Activity) {
    session.snapshot.transcript.push(TranscriptItem::Activity {
        activity_id: activity.id(),
    });
    session.snapshot.activities.push(activity);
}

fn command(turn_id: TurnId, status: ActivityStatus) -> Activity {
    Activity::Command {
        id: ActivityId::new(),
        turn_id,
        status,
        command: "cargo nextest run".to_owned(),
        cwd: None,
        output: "a long output no tally reads".repeat(64),
        output_truncated: false,
        exit_status: None,
    }
}

fn file_change(turn_id: TurnId, status: ActivityStatus) -> Activity {
    Activity::FileChange {
        id: ActivityId::new(),
        turn_id,
        status,
        changes: Vec::new(),
    }
}

fn tool_call(
    turn_id: TurnId,
    server: Option<&str>,
    name: &str,
    status: ActivityStatus,
) -> Activity {
    Activity::ToolCall {
        id: ActivityId::new(),
        turn_id,
        status,
        name: name.to_owned(),
        server: server.map(str::to_owned),
        input: String::new(),
        input_truncated: false,
        output: String::new(),
        output_truncated: false,
        omitted_parts: 0,
    }
}

fn spawned(turn_id: TurnId, status: ActivityStatus, brokered: bool) -> Activity {
    delegated(turn_id, status, brokered, crate::protocol::SessionId::new())
}

/// A later stretch of the delegation `spawn` began, resuming the same
/// Subagent in the Turn `turn_id`: a Subagent row of its own.
fn resumed(spawn: &Activity, turn_id: TurnId, status: ActivityStatus) -> Activity {
    let Activity::Subagent { session_id, .. } = spawn else {
        panic!("a Subagent row resumes a Subagent row's Subagent");
    };
    delegated(turn_id, status, false, *session_id)
}

/// A Subagent row delegating to the Subagent of `session_id`.
fn delegated(
    turn_id: TurnId,
    status: ActivityStatus,
    brokered: bool,
    session_id: crate::protocol::SessionId,
) -> Activity {
    Activity::Subagent {
        id: ActivityId::new(),
        turn_id,
        status,
        name: "explorer".to_owned(),
        description: "map the seams".to_owned(),
        model: None,
        session_id,
        brokered,
        duration_ms: None,
        delegated_at: None,
    }
}

fn reasoned(turn_id: TurnId) -> Activity {
    Activity::Reasoning {
        id: ActivityId::new(),
        turn_id,
        status: ActivityStatus::Completed,
        title: None,
        content: "thinking".to_owned(),
        content_truncated: false,
        duration_ms: None,
    }
}

fn named(server: Option<&str>, name: &str) -> InsightsTool {
    InsightsTool::Named {
        name: name.to_owned(),
        server: server.map(str::to_owned),
    }
}

fn tally(tool: InsightsTool, uses: u64, failed: u64) -> ToolTally {
    ToolTally { tool, uses, failed }
}

#[tokio::test]
async fn tools_are_tallied_from_the_stored_activities_of_the_interval_s_turns() {
    use ActivityStatus::{Active, Completed, Failed, Interrupted};
    let directory = tempfile::tempdir().unwrap();
    let mut parent = persisted(directory.path(), None);
    parent.snapshot.turns = vec![
        took(START, 1),
        took(END - 1, 1),
        // Begun before the interval, and never placed in time at all.
        took(START - 1, 1),
        ended(TurnStatus::Completed, None, None),
    ];
    let [first, last, before, untimed] = [0, 1, 2, 3].map(|turn| parent.snapshot.turns[turn].id);
    let explorer = spawned(first, Completed, false);
    for activity in [
        command(first, Completed),
        command(first, Failed),
        // Interrupted, or still running: a use, though not a failed one.
        command(last, Interrupted),
        command(last, Active),
        file_change(first, Completed),
        file_change(last, Failed),
        tool_call(first, None, "Read", Completed),
        // A Tool an MCP server hosts is told apart by its server, from one
        // of the same name that none does as from one another hosts.
        tool_call(first, Some("github"), "create_pr", Failed),
        tool_call(last, Some("fs"), "Read", Completed),
        // Each delegation counts, a resume of a Subagent as its spawn does.
        resumed(&explorer, last, Completed),
        explorer,
        spawned(last, Failed, true),
        // No use of a Tool.
        reasoned(first),
        // Uses by Turns the interval does not hold.
        command(before, Failed),
        command(untimed, Failed),
        tool_call(before, Some("github"), "create_pr", Completed),
    ] {
        record(&mut parent, activity);
    }
    // A Subagent's Tools are counted as its own Turns are.
    let mut subagent = persisted(directory.path(), Some(parent.snapshot.session.id));
    subagent.snapshot.turns = vec![took(START + 5, 1)];
    let turn = subagent.snapshot.turns[0].id;
    record(&mut subagent, tool_call(turn, None, "Read", Completed));
    let (store, writer) = restore_deferred(directory.path(), vec![parent, subagent]).await;

    let report = read(&store, &over(START, END)).await;

    assert_eq!(
        report.tools,
        vec![
            tally(InsightsTool::Command, 4, 1),
            tally(InsightsTool::Subagent, 3, 1),
            // As often used: the most failed first.
            tally(InsightsTool::FileChange, 2, 1),
            tally(named(None, "Read"), 2, 0),
            tally(named(Some("github"), "create_pr"), 1, 1),
            tally(named(Some("fs"), "Read"), 1, 0),
        ]
    );
    writer.shutdown().await.unwrap();
}

/// A Session the read does not count used no Tool it reports: a Subagent
/// whose parent is gone, which is in no tree Cost, and the Session a
/// Sidekick began on a Remote, whose Activities storage here never holds.
#[tokio::test]
async fn tools_are_tallied_only_for_the_sessions_the_read_counts() {
    let directory = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let sidekick_workspace = SidekickWorkspace::beside(data.path()).unwrap();
    let mut sidekick = persisted(&sidekick_workspace.ensure().unwrap(), None);
    sidekick.snapshot.turns = vec![took(START, 1)];
    let turn = sidekick.snapshot.turns[0].id;
    record(&mut sidekick, command(turn, ActivityStatus::Completed));
    let sidekick_id = sidekick.snapshot.session.id;
    let mut orphan = persisted(directory.path(), Some(crate::protocol::SessionId::new()));
    orphan.snapshot.turns = vec![took(START, 1)];
    let turn = orphan.snapshot.turns[0].id;
    record(&mut orphan, command(turn, ActivityStatus::Failed));
    record(&mut orphan, file_change(turn, ActivityStatus::Completed));
    let (store, writer) = restore_deferred(directory.path(), vec![sidekick, orphan]).await;
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
    store.remote_read(
        STUDIO,
        &Pairing {
            fingerprint: PAIRING.to_owned(),
            generation: 1,
        },
        vec![SessionListItem::Readable(Box::new(remote_session.summary))],
        store.moment(),
    );
    assert_eq!(
        store.remotes_listed_with(sidekick_id),
        vec![STUDIO.to_owned()],
        "the Sidekick's tree lists the Remote's Session"
    );

    let tools = read(&store, &over(START, END)).await.tools;

    assert_eq!(tools, vec![tally(InsightsTool::Command, 1, 0)]);
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn tools_are_read_from_storage_without_hydrating_a_deferred_session() {
    let directory = tempfile::tempdir().unwrap();
    let mut root = persisted(directory.path(), None);
    root.snapshot.turns = vec![took(START, 1)];
    let turn = root.snapshot.turns[0].id;
    record(&mut root, command(turn, ActivityStatus::Failed));
    let root_id = root.snapshot.session.id;
    let (store, writer) = restore_deferred(directory.path(), vec![root]).await;
    let read_state = |store: &SessionStore| {
        let state = store.state.lock().unwrap();
        (
            state.is_deferred(root_id),
            state.sessions[&root_id].snapshot.revision,
            state.sessions[&root_id].snapshot.activities.len(),
        )
    };
    let before = read_state(&store);
    assert_eq!(
        (before.0, before.2),
        (true, 0),
        "the history opens deferred"
    );

    let tools = read(&store, &over(START, END)).await.tools;

    assert_eq!(tools, vec![tally(InsightsTool::Command, 1, 1)]);
    assert_eq!(read_state(&store), before, "no history was read in");
    writer.shutdown().await.unwrap();
}

/// A Session already open is read from storage too, so its Tools are
/// counted as a deferred one's are, as of its latest save.
#[tokio::test]
async fn an_open_session_s_tools_are_read_from_storage_as_well() {
    let directory = tempfile::tempdir().unwrap();
    let mut root = persisted(directory.path(), None);
    root.snapshot.turns = vec![took(START, 1)];
    let turn = root.snapshot.turns[0].id;
    record(&mut root, file_change(turn, ActivityStatus::Completed));
    let root_id = root.snapshot.session.id;
    let (store, writer) = restore_deferred(directory.path(), vec![root]).await;
    store.hydrate(root_id).await.unwrap();

    let tools = read(&store, &over(START, END)).await.tools;

    assert_eq!(tools, vec![tally(InsightsTool::FileChange, 1, 0)]);
    writer.shutdown().await.unwrap();
}

fn compaction(
    turn_id: TurnId,
    trigger: CompactionTrigger,
    status: ActivityStatus,
    before_tokens: Option<u64>,
    after_tokens: Option<u64>,
) -> Activity {
    Activity::Compaction {
        id: ActivityId::new(),
        turn_id,
        status,
        trigger,
        instructions: None,
        before_tokens,
        after_tokens,
        error: None,
        summary: Some("a summary no tally reads".repeat(64)),
        summary_truncated: false,
    }
}

fn counts(active: u64, completed: u64, failed: u64, interrupted: u64) -> CompactionCounts {
    CompactionCounts {
        active,
        completed,
        failed,
        interrupted,
    }
}

#[tokio::test]
async fn compactions_are_tallied_by_trigger_and_status_from_stored_activities() {
    use ActivityStatus::{Active, Completed, Failed, Interrupted};
    use CompactionTrigger::{Automatic, Manual};
    let directory = tempfile::tempdir().unwrap();
    let mut session = persisted(directory.path(), None);
    session.snapshot.turns = vec![took(START, 1), took(START - 1, 1)];
    let [held, before] = [0, 1].map(|turn| session.snapshot.turns[turn].id);
    for activity in [
        compaction(held, Automatic, Completed, Some(100_000), Some(20_000)),
        compaction(held, Automatic, Completed, Some(50_000), Some(40_000)),
        compaction(held, Automatic, Failed, Some(90_000), None),
        compaction(held, Automatic, Active, Some(80_000), None),
        compaction(held, Manual, Completed, None, Some(10_000)),
        compaction(held, Manual, Interrupted, None, None),
        compaction(held, Manual, Failed, None, None),
        // A Turn the interval does not hold.
        compaction(before, Manual, Completed, Some(10), Some(1)),
        command(held, Completed),
    ] {
        record(&mut session, activity);
    }
    let (store, writer) = restore_deferred(directory.path(), vec![session]).await;

    let compactions = read(&store, &over(START, END)).await.compactions;

    assert_eq!(
        (
            compactions.automatic,
            compactions.manual,
            compactions.total()
        ),
        (counts(1, 2, 1, 0), counts(0, 1, 1, 1), 7)
    );
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn freed_share_averages_only_completed_compactions_that_knew_both_figures() {
    use ActivityStatus::{Completed, Failed, Interrupted};
    use CompactionTrigger::{Automatic, Manual};
    let directory = tempfile::tempdir().unwrap();
    let mut session = persisted(directory.path(), None);
    session.snapshot.turns = vec![took(START, 1)];
    let turn = session.snapshot.turns[0].id;
    for activity in [
        // Frees 80%, and 20%.
        compaction(turn, Automatic, Completed, Some(100_000), Some(20_000)),
        compaction(turn, Manual, Completed, Some(50_000), Some(40_000)),
        // A short history's summary can leave more than it found: -10%.
        compaction(turn, Automatic, Completed, Some(10_000), Some(11_000)),
        // Neither figure, one alone, or nothing before to free.
        compaction(turn, Automatic, Completed, None, None),
        compaction(turn, Automatic, Completed, Some(100_000), None),
        compaction(turn, Automatic, Completed, None, Some(1_000)),
        compaction(turn, Automatic, Completed, Some(0), Some(0)),
        // Not completed, whatever it knew.
        compaction(turn, Automatic, Failed, Some(100_000), Some(100_000)),
        compaction(turn, Manual, Interrupted, Some(100_000), Some(0)),
    ] {
        record(&mut session, activity);
    }
    let (store, writer) = restore_deferred(directory.path(), vec![session]).await;

    let compactions = read(&store, &over(START, END)).await.compactions;

    assert_eq!(
        compactions.freed,
        Some(FreedShare {
            compactions: 3,
            average_millionths: 300_000,
        })
    );
    writer.shutdown().await.unwrap();
}

#[tokio::test]
async fn no_compaction_that_could_say_leaves_the_freed_share_unknown() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = persisted(directory.path(), None);
    session.snapshot.turns = vec![took(START, 1)];
    let turn = session.snapshot.turns[0].id;
    record(
        &mut session,
        compaction(
            turn,
            CompactionTrigger::Manual,
            ActivityStatus::Interrupted,
            Some(100_000),
            Some(1_000),
        ),
    );
    let (store, writer) = restore_deferred(directory.path(), vec![session]).await;

    let compactions = read(&store, &over(START, END)).await.compactions;

    assert_eq!(
        (compactions.manual, compactions.freed),
        (counts(0, 0, 0, 1), None)
    );
    writer.shutdown().await.unwrap();
}

fn approval(turn_id: TurnId, outcome: ApprovalOutcome, decision: Option<Decision>) -> Activity {
    Activity::Approval {
        id: ActivityId::new(),
        turn_id,
        approval: Approval {
            id: ApprovalId::new(),
            subject: ApprovalSubject::Command {
                command: "rm -rf target".to_owned(),
                cwd: None,
                actions: Vec::new(),
            },
            reason: Some("a reason no tally reads".repeat(64)),
        },
        tool_activity_id: None,
        detail_truncated: false,
        outcome,
        decision,
        follow_up_error: None,
        asked_at: None,
    }
}

#[tokio::test]
async fn approvals_are_tallied_by_the_decision_recorded_for_each() {
    use ApprovalOutcome::{
        Decided, DeliveryUncertain, Pending, SubmissionRejected, Submitting, TurnEnded,
        Unavailable, Withdrawn,
    };
    use Decision::{Accept, AcceptForSession, Decline, DeclineAndInterrupt};
    let directory = tempfile::tempdir().unwrap();
    let mut session = persisted(directory.path(), None);
    session.snapshot.turns = vec![took(START, 1), took(START - 1, 1)];
    let [held, before] = [0, 1].map(|turn| session.snapshot.turns[turn].id);
    for activity in [
        approval(held, Decided, Some(Accept)),
        approval(held, Decided, Some(Accept)),
        approval(held, Decided, Some(Accept)),
        approval(held, Decided, Some(AcceptForSession)),
        approval(held, Decided, Some(AcceptForSession)),
        approval(held, Decided, Some(Decline)),
        // A Decision that also ended its Turn is a Decline all the same,
        // and told apart.
        approval(held, Decided, Some(DeclineAndInterrupt)),
        // Still waiting on a Decision, or on its delivery.
        approval(held, Pending, None),
        approval(held, Submitting, None),
        approval(held, SubmissionRejected, None),
        // Settled with no Decision recorded.
        approval(held, Withdrawn, None),
        approval(held, TurnEnded, None),
        approval(held, Unavailable, None),
        approval(held, DeliveryUncertain, None),
        // A Turn the interval does not hold.
        approval(before, Decided, Some(Decline)),
    ] {
        record(&mut session, activity);
    }
    let (store, writer) = restore_deferred(directory.path(), vec![session]).await;

    let approvals = read(&store, &over(START, END)).await.approvals;

    assert_eq!(
        approvals,
        ApprovalInsights {
            asked: 14,
            accepted: 3,
            accepted_for_session: 2,
            declined: 1,
            declined_and_interrupted: 1,
            waiting: 3,
            expired: 4,
        }
    );
    writer.shutdown().await.unwrap();
}

/// Compactions and Approvals are read in the same pass over storage as the
/// Tools, loading no history in either.
#[tokio::test]
async fn compactions_and_approvals_are_read_without_hydrating_a_deferred_session() {
    let directory = tempfile::tempdir().unwrap();
    let mut root = persisted(directory.path(), None);
    root.snapshot.turns = vec![took(START, 1)];
    let turn = root.snapshot.turns[0].id;
    record(
        &mut root,
        compaction(
            turn,
            CompactionTrigger::Manual,
            ActivityStatus::Failed,
            None,
            None,
        ),
    );
    record(&mut root, approval(turn, ApprovalOutcome::Pending, None));
    let root_id = root.snapshot.session.id;
    let (store, writer) = restore_deferred(directory.path(), vec![root]).await;
    let read_state = |store: &SessionStore| {
        let state = store.state.lock().unwrap();
        (
            state.is_deferred(root_id),
            state.sessions[&root_id].snapshot.revision,
            state.sessions[&root_id].snapshot.activities.len(),
        )
    };
    let before = read_state(&store);

    let report = read(&store, &over(START, END)).await;

    assert_eq!(
        (
            report.compactions.manual,
            report.approvals.asked,
            report.approvals.waiting
        ),
        (counts(0, 0, 1, 0), 1, 1)
    );
    assert_eq!(read_state(&store), before, "no history was read in");
    writer.shutdown().await.unwrap();
}
