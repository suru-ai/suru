//! Insights: how the Sessions living on this Server worked over one span of
//! time, beside how much they consumed. The read counts the Sessions the
//! metrics read counts, copied under the store lock as that read copies them
//! and added up outside it, and places each Turn by the same rule; a Turn
//! with no recorded moment to place it by is left out.
//!
//! Each panel of the Insights tab reads one part of the report, which a
//! [`InsightsFold`] adds up a part apiece from the Turns the span holds.

use crate::protocol::{
    InsightsQuery, InsightsReport, TURN_DURATION_BIN_ENDS_MS, TURN_DURATION_BINS, Turn,
    TurnDurations, TurnInsights, TurnStatus,
};

use super::{super::SessionStore, CountedSession, MeteredForest, SessionTimestamp, placed_at};

/// Why an Insights query is refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InsightsRefusal {
    /// Its interval ends before it begins.
    Backwards,
}

impl SessionStore {
    /// How the Sessions living here worked over `query`'s interval. A pure
    /// read, as the metrics read is: it moves no Session's revision, writes
    /// nothing, and reads no history in.
    pub(crate) fn usage_insights(
        &self,
        query: &InsightsQuery,
    ) -> Result<InsightsReport, InsightsRefusal> {
        let interval = query.interval;
        if interval.end < interval.start {
            return Err(InsightsRefusal::Backwards);
        }
        let copied = self
            .state
            .lock()
            .expect("Session store lock is not poisoned")
            .copied_sessions();
        let forest = MeteredForest::index(copied);
        let mut fold = InsightsFold::default();
        for CountedSession { session_id, .. } in forest.counted_sessions(&forest.cost_tree_heads())
        {
            let held = forest.sessions[&session_id]
                .turns
                .iter()
                .filter(|turn| placed_at(turn).is_some_and(|at| interval.contains(at)));
            for turn in held {
                fold.add_turn(turn);
            }
        }
        Ok(fold.into_report())
    }
}

/// The report as it is added up, a part for each of its own.
#[derive(Default)]
struct InsightsFold {
    turns: TurnTally,
}

impl InsightsFold {
    /// Adds a Turn the interval holds.
    fn add_turn(&mut self, turn: &Turn) {
        self.turns.add(turn);
    }

    fn into_report(self) -> InsightsReport {
        InsightsReport {
            turns: self.turns.into_insights(),
        }
    }
}

/// How the interval's Turns ended, and how long each Settled one took.
#[derive(Default)]
struct TurnTally {
    durations: Vec<u64>,
    completed: u64,
    failed: u64,
    interrupted: u64,
}

impl TurnTally {
    fn add(&mut self, turn: &Turn) {
        let count = match turn.status {
            TurnStatus::Active => return,
            TurnStatus::Completed => &mut self.completed,
            TurnStatus::Failed => &mut self.failed,
            TurnStatus::Interrupted => &mut self.interrupted,
        };
        *count += 1;
        // A Settle stamped before its start gives no duration, as the
        // Transcript's Turn Fold states none for it.
        if let Some(duration) = turn
            .started_at
            .zip(turn.settled_at)
            .and_then(|(started_at, settled_at)| duration_ms(started_at, settled_at))
        {
            self.durations.push(duration);
        }
    }

    fn into_insights(mut self) -> TurnInsights {
        self.durations.sort_unstable();
        TurnInsights {
            durations: durations_of(&self.durations),
            completed: self.completed,
            failed: self.failed,
            interrupted: self.interrupted,
        }
    }
}

fn duration_ms(started_at: SessionTimestamp, settled_at: SessionTimestamp) -> Option<u64> {
    settled_at.0.checked_sub(started_at.0)
}

/// The figures of `sorted`, shortest first, none where it is empty.
fn durations_of(sorted: &[u64]) -> Option<TurnDurations> {
    let longest_ms = *sorted.last()?;
    // The nearest rank: the `numerator`/`denominator` share of the
    // durations, rounded up, is at least as long as the one at that rank.
    let rank = |numerator: usize, denominator: usize| {
        sorted[(numerator * sorted.len()).div_ceil(denominator) - 1]
    };
    let mut bins = [0; TURN_DURATION_BINS];
    for duration in sorted {
        bins[TURN_DURATION_BIN_ENDS_MS.partition_point(|end| end <= duration)] += 1;
    }
    Some(TurnDurations {
        median_ms: rank(1, 2),
        p90_ms: rank(9, 10),
        longest_ms,
        bins,
    })
}
