//! Usage Metrics: what the Sessions living on this Server consumed, read from
//! the Turns the store holds of every Session into the buckets a reader asks
//! for.
//!
//! Every Session's Turns are in memory from startup, its history deferred or
//! not (ADR 0022), so the read never hydrates one. It copies what it needs of
//! each Turn under the store lock and adds them up outside it, so a large
//! history holds up no other reader. Cost is read through Cost Coverage, as
//! the tree Costs a listing shows are (ADR 0054): each contribution lands
//! where the Turn carrying it does, so over all of time the buckets add up to
//! the Server's root Sessions' tree Costs. A Remote's Session that a Sidekick here
//! acted on is held apart from the Sessions living here (ADR 0049), so it is
//! never counted: its own Server counts it.

use std::collections::HashMap;

use crate::protocol::{
    CostBasis, KeyedUsageMetrics, MAX_USAGE_METRICS_BUCKETS, ModelCatalog, SessionId,
    SessionTimestamp, Turn, TurnId, Usage, UsageMetricsAggregate, UsageMetricsBreakdown,
    UsageMetricsBucket, UsageMetricsKey, UsageMetricsKeyLabel, UsageMetricsQuery,
    UsageMetricsReport,
};

use super::{SessionStore, SessionStoreState, cost_coverage::CostContribution};

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

/// What the read copies of one Turn: which it is, where it is placed in
/// time, the key it is broken down under, and its Usage.
struct MeteredTurn {
    session_id: SessionId,
    turn_id: TurnId,
    placed_at: Option<SessionTimestamp>,
    key: UsageMetricsKey,
    usage: Option<Usage>,
}

/// What the read copies out of the store: every Turn living here, and what
/// each adds to the Server's Cost.
struct Metered {
    turns: Vec<MeteredTurn>,
    costs: Vec<CostContribution>,
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
        let metered = self
            .state
            .lock()
            .expect("Session store lock is not poisoned")
            .metered(query.breakdown);
        Ok(fold(query, metered, catalog))
    }
}

impl SessionStoreState {
    /// Every Turn of every Session living here, as the read needs it, and
    /// the Cost contributions of every tree of Sessions here.
    fn metered(&self, breakdown: UsageMetricsBreakdown) -> Metered {
        let turns = self
            .sessions
            .iter()
            .flat_map(|(session_id, record)| {
                record.snapshot.turns.iter().map(|turn| MeteredTurn {
                    session_id: *session_id,
                    turn_id: turn.id,
                    placed_at: turn.started_at.or(turn.settled_at),
                    key: key_of(breakdown, turn),
                    usage: turn.usage.clone(),
                })
            })
            .collect();
        let costs = self
            .sessions
            .keys()
            .filter(|session_id| self.heads_a_cost_tree(**session_id))
            .flat_map(|root| self.tree_cost_contributions(*root))
            .collect();
        Metered { turns, costs }
    }

    /// Whether this Session's tree Cost is one of those the Server's Cost
    /// adds up: a root Session's, unless it is a Subsession whose Sidekick's
    /// tree Cost already carries it. Every Session reached from a root is
    /// then counted once: a Subagent in its root's tree, and a Subsession in
    /// its Sidekick's.
    fn heads_a_cost_tree(&self, session_id: SessionId) -> bool {
        let is_root = |session_id: SessionId| {
            self.sessions
                .get(&session_id)
                .is_some_and(|record| record.snapshot.session.parent.is_none())
        };
        is_root(session_id)
            && self.sessions[&session_id]
                .summary
                .session
                .sidekick()
                .is_none_or(|sidekick| !is_root(sidekick))
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

    fn add_cost(&mut self, key: UsageMetricsKey, contribution: &CostContribution) {
        add_contribution(&mut self.total, contribution);
        add_contribution(self.by_key.entry(key).or_default(), contribution);
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

/// Adds what one Turn contributes to a Cost: its amount, and its Estimated
/// portion, where it counts one, and its partial mark.
fn add_contribution(aggregate: &mut UsageMetricsAggregate, contribution: &CostContribution) {
    if let Some(amount) = contribution.amount {
        let add = |held: Option<crate::protocol::Cost>| {
            Some(held.map_or(amount.cost, |held| held.saturating_add(amount.cost)))
        };
        aggregate.cost = add(aggregate.cost);
        if amount.basis == CostBasis::Estimated {
            aggregate.estimated_cost = add(aggregate.estimated_cost);
        }
    }
    aggregate.is_cost_partial |= contribution.is_partial;
}

fn fold(query: &UsageMetricsQuery, metered: Metered, catalog: &ModelCatalog) -> UsageMetricsReport {
    let mut buckets = query
        .buckets
        .iter()
        .map(|_| Tally::default())
        .collect::<Vec<_>>();
    let mut untimed = Tally::default();
    // The bucket a moment falls in, if any: the buckets run in order without
    // overlapping, so the one that can hold it is the first not over before it.
    let bucket_of = |placed_at: SessionTimestamp| {
        let index = query
            .buckets
            .partition_point(|bucket| bucket.end <= placed_at);
        query
            .buckets
            .get(index)
            .is_some_and(|bucket| bucket.contains(placed_at))
            .then_some(index)
    };
    let mut earliest_turn_at = None::<SessionTimestamp>;
    // Where each Turn landed, for the Cost it carries to land beside it.
    let mut placed = HashMap::with_capacity(metered.turns.len());
    for turn in metered.turns {
        let place = turn.placed_at.map(bucket_of);
        placed.insert((turn.session_id, turn.turn_id), (place, turn.key.clone()));
        match place {
            None => untimed.add(turn),
            Some(index) => {
                let placed_at = turn.placed_at.expect("a placed Turn is timed");
                earliest_turn_at =
                    Some(earliest_turn_at.map_or(placed_at, |held| held.min(placed_at)));
                if let Some(index) = index {
                    buckets[index].add(turn);
                }
            }
        }
    }
    for contribution in &metered.costs {
        // Every contribution is of a Turn the store holds; one that were not
        // would still count, with nothing to place it by.
        let (place, key) = placed
            .get(&(contribution.session_id, contribution.turn_id))
            .cloned()
            .unwrap_or((None, UsageMetricsKey::Unknown));
        match place {
            None => untimed.add_cost(key, contribution),
            Some(Some(index)) => buckets[index].add_cost(key, contribution),
            Some(None) => {}
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
