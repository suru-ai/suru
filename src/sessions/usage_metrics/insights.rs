//! Insights: how the Sessions living on this Server worked over one span of
//! time, beside how much they consumed. The read counts the Sessions the
//! metrics read counts, copied under the store lock as that read copies them
//! and added up outside it, and places each Turn by the same rule; a Turn
//! with no recorded moment to place it by is left out.
//!
//! Each panel of the Insights tab reads one part of the report, which a
//! [`InsightsFold`] adds up a part apiece from the Turns the span holds.
//!
//! The Tools those Turns used are Activities, which no Session holds in
//! memory until its history is read (ADR 0022). Rather than read any in,
//! the read asks storage for the few fields of each Activity it counts,
//! and keeps those whose Turn the span holds (ADR 0056).
//!
//! The quarter-hours carry Cost as the metrics read's buckets do: every
//! Cost Coverage contribution of the counted trees lands where the Turn
//! carrying it does, so the quarter-hours of a span add up to the Cost the
//! metrics read gives it.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::protocol::{
    InsightsQuery, InsightsReport, QUARTER_HOUR_MS, QuarterHourTurns, SessionId,
    TURN_DURATION_BIN_ENDS_MS, TURN_DURATION_BINS, ToolTally, Turn, TurnDurations, TurnId,
    TurnInsights, TurnStatus, Usage,
};

use super::{
    super::{
        SessionStore,
        cost_coverage::{CostContribution, tree_contributions},
    },
    CountedSession, MeteredForest, SessionTimestamp, TurnPlaces, add_counted, placed_at,
};

/// Why an Insights query is refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InsightsRefusal {
    /// Its interval ends before it begins.
    Backwards,
    /// Storage could not be read for the Tools the interval's Turns used.
    Storage,
}

impl SessionStore {
    /// How the Sessions living here worked over `query`'s interval. A pure
    /// read, as the metrics read is: it moves no Session's revision, writes
    /// nothing, and reads no history in. The Tools the interval's Turns used
    /// are read from the Activities storage holds, on a blocking task once
    /// the lock is let go (ADR 0056), and placed in time by the Turns that
    /// own them, which the copy taken under the lock holds.
    pub(crate) async fn usage_insights(
        &self,
        query: &InsightsQuery,
    ) -> Result<InsightsReport, InsightsRefusal> {
        let interval = query.interval;
        if interval.end < interval.start {
            return Err(InsightsRefusal::Backwards);
        }
        let (copied, repository) = {
            let state = self
                .state
                .lock()
                .expect("Session store lock is not poisoned");
            let repository = state
                .deferred
                .as_ref()
                .map(|deferred| deferred.repository.clone());
            (state.copied_sessions(), repository)
        };
        let forest = MeteredForest::index(copied);
        let roots = forest.cost_tree_heads();
        let mut fold = InsightsFold::default();
        for CountedSession { session_id, .. } in forest.counted_sessions(&roots) {
            for turn in &forest.sessions[&session_id].turns {
                if let Some(at) = placed_at(turn).filter(|at| interval.contains(*at)) {
                    fold.add_turn(session_id, turn, at);
                }
            }
        }
        for root in roots {
            for contribution in tree_contributions(&forest, root) {
                fold.add_cost(&contribution);
            }
        }
        // Only a store restored without storage, as some tests restore one,
        // lacks a repository; its Tools are in memory, and are not counted.
        if let Some(repository) = repository
            && !fold.held_turns.is_empty()
        {
            let held = std::mem::take(&mut fold.held_turns);
            let tools = repository.tool_tallies(held).await.map_err(|error| {
                tracing::warn!(%error, "reading the Tools Insights counts failed");
                InsightsRefusal::Storage
            })?;
            fold.tools = tools;
        }
        Ok(fold.into_report())
    }
}

/// The report as it is added up, a part for each of its own.
#[derive(Default)]
struct InsightsFold {
    turns: TurnTally,
    quarter_hours: QuarterHourTally,
    /// The Turns the interval holds of each Session, whose Activities are
    /// the uses of Tools it counts.
    held_turns: HashMap<SessionId, HashSet<TurnId>>,
    /// How often each Tool was used, in no order yet.
    tools: Vec<ToolTally>,
}

impl InsightsFold {
    /// Adds a Turn of `session_id` the interval holds, placed `at`.
    fn add_turn(&mut self, session_id: SessionId, turn: &Turn, at: SessionTimestamp) {
        self.turns.add(turn);
        self.quarter_hours.add_turn(session_id, turn, at);
        self.held_turns
            .entry(session_id)
            .or_default()
            .insert(turn.id);
    }

    /// Adds a Cost contribution of the counted trees, where the interval
    /// holds the Turn carrying it.
    fn add_cost(&mut self, contribution: &CostContribution) {
        self.quarter_hours.add_cost(contribution);
    }

    fn into_report(self) -> InsightsReport {
        InsightsReport {
            turns: self.turns.into_insights(),
            quarter_hours: self.quarter_hours.into_quarter_hours(),
            tools: most_used_first(self.tools),
        }
    }
}

/// `tools` the most used first, those used as often the most failed first,
/// and those alike in both as their Tools are ordered, so the order is the
/// same at every read.
fn most_used_first(mut tools: Vec<ToolTally>) -> Vec<ToolTally> {
    tools.sort_unstable_by(|left, right| {
        (right.uses, right.failed)
            .cmp(&(left.uses, left.failed))
            .then_with(|| left.tool.cmp(&right.tool))
    });
    tools
}

/// The interval's Turns by the UTC quarter-hour each is placed in, and
/// which quarter-hour holds each Turn, for the Cost it carries to land
/// beside it.
#[derive(Default)]
struct QuarterHourTally {
    /// Each quarter-hour a Turn is placed in, by its place since the epoch.
    quarters: BTreeMap<u64, QuarterHourTurns>,
    placed: TurnPlaces<u64>,
}

impl QuarterHourTally {
    fn add_turn(&mut self, session_id: SessionId, turn: &Turn, at: SessionTimestamp) {
        let place = at.0 / QUARTER_HOUR_MS;
        let quarter = self.quarters.entry(place).or_insert(QuarterHourTurns {
            start: SessionTimestamp(place * QUARTER_HOUR_MS),
            turns: 0,
            tokens: None,
            cost: None,
            is_cost_partial: false,
        });
        quarter.turns += 1;
        if let Some(tokens) = turn.usage.as_ref().and_then(Usage::blended_tokens) {
            quarter.tokens = Some(quarter.tokens.unwrap_or(0).saturating_add(tokens));
        }
        self.placed.place(session_id, turn.id, place);
    }

    fn add_cost(&mut self, contribution: &CostContribution) {
        let Some(quarter) = self
            .placed
            .of(contribution)
            .and_then(|place| self.quarters.get_mut(place))
        else {
            return;
        };
        add_counted(
            &mut quarter.cost,
            &mut quarter.is_cost_partial,
            contribution,
        );
    }

    fn into_quarter_hours(self) -> Vec<QuarterHourTurns> {
        self.quarters.into_values().collect()
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
