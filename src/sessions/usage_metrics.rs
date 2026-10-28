//! Usage Metrics: what the Sessions living on this Server consumed, read from
//! the Turns the store holds of every Session into the buckets a reader asks
//! for.
//!
//! Every Session's Turns are in memory from startup, its history deferred or
//! not (ADR 0022), so the read never hydrates one. It copies what it needs of
//! each Turn under the store lock and adds them up outside it, so a large
//! history holds up no other reader. A Remote's Session that a Sidekick here
//! acted on is held apart from the Sessions living here (ADR 0049), so it is
//! never counted: its own Server counts it.

use std::collections::HashMap;

use crate::protocol::{
    KeyedUsageMetrics, MAX_USAGE_METRICS_BUCKETS, ModelCatalog, SessionTimestamp, Turn, Usage,
    UsageMetricsAggregate, UsageMetricsBreakdown, UsageMetricsBucket, UsageMetricsKey,
    UsageMetricsKeyLabel, UsageMetricsQuery, UsageMetricsReport,
};

use super::{SessionStore, SessionStoreState};

/// Why a Usage Metrics query is refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UsageMetricsRefusal {
    /// It asks for more buckets than [`MAX_USAGE_METRICS_BUCKETS`].
    TooManyBuckets,
    /// A bucket ends before it begins, or begins before the one ahead of it.
    OutOfOrder,
    /// A bucket begins before the one ahead of it ends.
    Overlapping,
}

impl UsageMetricsRefusal {
    /// Refuses a query whose buckets no report could answer one by one:
    /// too many of them, or any running out of order or overlapping
    /// another, so no Turn could be placed in two.
    fn of(query: &UsageMetricsQuery) -> Option<Self> {
        if query.buckets.len() > MAX_USAGE_METRICS_BUCKETS {
            return Some(Self::TooManyBuckets);
        }
        if query.buckets.iter().any(|bucket| bucket.end < bucket.start) {
            return Some(Self::OutOfOrder);
        }
        query.buckets.windows(2).find_map(|pair| {
            if pair[1].start < pair[0].start {
                Some(Self::OutOfOrder)
            } else if pair[1].start < pair[0].end {
                Some(Self::Overlapping)
            } else {
                None
            }
        })
    }
}

/// What the read copies of one Turn: where it is placed in time, the key it
/// is broken down under, and its Usage.
struct MeteredTurn {
    placed_at: Option<SessionTimestamp>,
    key: UsageMetricsKey,
    usage: Option<Usage>,
}

impl SessionStore {
    /// What the Sessions living here consumed in each of `query`'s buckets,
    /// labelling each Model as `catalog` names it. A pure read: it moves no
    /// Session's revision, writes nothing, and reads no history in.
    pub(crate) fn usage_metrics(
        &self,
        query: &UsageMetricsQuery,
        catalog: &ModelCatalog,
    ) -> Result<UsageMetricsReport, UsageMetricsRefusal> {
        if let Some(refusal) = UsageMetricsRefusal::of(query) {
            return Err(refusal);
        }
        let turns = self
            .state
            .lock()
            .expect("Session store lock is not poisoned")
            .metered_turns(query.breakdown);
        Ok(fold(query, turns, catalog))
    }
}

impl SessionStoreState {
    /// Every Turn of every Session living here, as the read needs it.
    fn metered_turns(&self, breakdown: UsageMetricsBreakdown) -> Vec<MeteredTurn> {
        self.sessions
            .values()
            .flat_map(|record| &record.snapshot.turns)
            .map(|turn| MeteredTurn {
                placed_at: turn.started_at.or(turn.settled_at),
                key: key_of(breakdown, turn),
                usage: turn.usage.clone(),
            })
            .collect()
    }
}

fn key_of(breakdown: UsageMetricsBreakdown, turn: &Turn) -> UsageMetricsKey {
    match breakdown {
        UsageMetricsBreakdown::Model => {
            turn.agent
                .as_ref()
                .map_or(UsageMetricsKey::Unknown, |agent| UsageMetricsKey::Model {
                    provider: agent.selection.provider.clone(),
                    model: agent.selection.model.clone(),
                })
        }
    }
}

/// One bucket's figures as they are added up.
#[derive(Default)]
struct Tally {
    total: UsageMetricsAggregate,
    by_key: HashMap<UsageMetricsKey, UsageMetricsAggregate>,
}

impl Tally {
    fn add(&mut self, turn: MeteredTurn) {
        let keyed = self.by_key.entry(turn.key).or_default();
        if let Some(usage) = &turn.usage {
            self.total.add_usage(usage);
            keyed.add_usage(usage);
        }
    }

    fn into_bucket(mut self, keys: &[UsageMetricsKey]) -> UsageMetricsBucket {
        UsageMetricsBucket {
            total: self.total,
            by_key: keys
                .iter()
                .filter_map(|key| {
                    Some(KeyedUsageMetrics {
                        aggregate: self.by_key.remove(key)?,
                        key: key.clone(),
                    })
                })
                .collect(),
        }
    }
}

fn fold(
    query: &UsageMetricsQuery,
    turns: Vec<MeteredTurn>,
    catalog: &ModelCatalog,
) -> UsageMetricsReport {
    let mut buckets = query
        .buckets
        .iter()
        .map(|_| Tally::default())
        .collect::<Vec<_>>();
    let mut untimed = Tally::default();
    let mut earliest_turn_at = None::<SessionTimestamp>;
    for turn in turns {
        let Some(placed_at) = turn.placed_at else {
            untimed.add(turn);
            continue;
        };
        earliest_turn_at = Some(earliest_turn_at.map_or(placed_at, |held| held.min(placed_at)));
        // The buckets run in order without overlapping, so the one that can
        // hold the moment is the first not over before it.
        let index = query
            .buckets
            .partition_point(|bucket| bucket.end <= placed_at);
        if query
            .buckets
            .get(index)
            .is_some_and(|bucket| bucket.contains(placed_at))
        {
            buckets[index].add(turn);
        }
    }

    let mut keys = buckets
        .iter()
        .chain([&untimed])
        .flat_map(|tally| tally.by_key.keys())
        .cloned()
        .collect::<Vec<_>>();
    keys.sort_by(|left, right| key_order(left).cmp(&key_order(right)));
    keys.dedup();
    UsageMetricsReport {
        buckets: buckets
            .into_iter()
            .map(|tally| tally.into_bucket(&keys))
            .collect(),
        untimed: untimed.into_bucket(&keys),
        earliest_turn_at,
        keys: keys
            .into_iter()
            .map(|key| UsageMetricsKeyLabel {
                label: label(&key, catalog),
                key,
            })
            .collect(),
    }
}

/// The order keys stand in a report: every Model by Provider and then by
/// id, and Unknown after them all.
fn key_order(key: &UsageMetricsKey) -> (bool, &str, &str) {
    match key {
        UsageMetricsKey::Model { provider, model } => (false, provider.as_str(), model.as_str()),
        UsageMetricsKey::Unknown => (true, "", ""),
    }
}

/// The name a reader is shown for `key`: a Model's name in the Model
/// Catalog where it carries one, otherwise its id.
fn label(key: &UsageMetricsKey, catalog: &ModelCatalog) -> String {
    match key {
        UsageMetricsKey::Model { provider, model } => catalog.model(provider, model).map_or_else(
            || model.as_str().to_owned(),
            |descriptor| descriptor.display_name.clone(),
        ),
        UsageMetricsKey::Unknown => "Unknown".to_owned(),
    }
}
