//! Cost Coverage: how the frozen Costs of a Session's Turns, and of the
//! Sessions beneath it, add up to the Cost a surface states for it without
//! counting any work twice (ADR 0016, ADR 0025, ADR 0040).

use std::collections::HashMap;

use crate::protocol::{
    Cost, CostCoverage, CostRecord, CostTotal, SessionId, SessionTimestamp, Turn, TurnId,
    TurnStatus,
};

use super::SessionStoreState;

impl SessionStoreState {
    /// The tree Cost of this Session: its own and every descendant's, native
    /// and brokered, to any depth, and the tree Cost of every Subsession its
    /// Sidekick began here, and nothing above it. A Subsession is metered by
    /// a Provider actor of its own, so no report of the Sidekick's covers it
    /// and its tree Cost adds whole.
    pub(super) fn cost_total(&self, session_id: SessionId) -> Option<CostTotal> {
        let mut total = self.cost_of(&self.subtree(session_id));
        let mut unpriced = false;
        for subsession in self.subsessions_of(session_id) {
            match self.sessions[&subsession].snapshot.total_cost {
                Some(cost) => total = add_cost_totals(total, cost),
                None => {
                    unpriced |= self
                        .subtree(subsession)
                        .iter()
                        .any(|id| !self.sessions[id].snapshot.turns.is_empty());
                }
            }
        }
        with_unpriced_work(total, unpriced)
    }

    /// The own Cost of this Session: the Provider's account of its own
    /// conversation, read by the same Cost Coverage applied to its Turns
    /// alone, so each reporting lifetime's latest cumulative report counts
    /// once rather than once per Turn that carried one.
    pub(super) fn own_cost(&self, session_id: SessionId) -> Option<CostTotal> {
        self.cost_of(&[session_id])
    }

    /// Applies each Provider-declared coverage window to the frozen Cost
    /// records of `sessions`, a Session and some of what lies beneath it. A
    /// subtree report is cumulative only inside its reporting lifetime, and
    /// receipt order alone never proves that late descendant evidence falls
    /// outside a report.
    fn cost_of(&self, sessions: &[SessionId]) -> Option<CostTotal> {
        let mut records = Vec::new();
        for &owner in sessions {
            let Some(session) = self.sessions.get(&owner) else {
                continue;
            };
            for turn in &session.snapshot.turns {
                records.extend(cost_records(owner, turn));
            }
        }

        let mut lifetime_starts = HashMap::<String, SessionTimestamp>::new();
        let mut latest_subtree = HashMap::<(SessionId, String), ScopedCost>::new();
        for record in records
            .iter()
            .filter(|record| matches!(record.record.coverage, CostCoverage::SessionSubtree { .. }))
        {
            let CostCoverage::SessionSubtree { reporting_lifetime } = &record.record.coverage
            else {
                unreachable!();
            };
            if let Some(started_at) = record.turn_started_at {
                lifetime_starts
                    .entry(reporting_lifetime.clone())
                    .and_modify(|current| *current = (*current).min(started_at))
                    .or_insert(started_at);
            }
            let key = (record.owner, reporting_lifetime.clone());
            if latest_subtree
                .get(&key)
                .is_none_or(|current| current.record.recorded_at < record.record.recorded_at)
            {
                latest_subtree.insert(key, record.clone());
            }
        }
        let aggregates = latest_subtree.into_values().collect::<Vec<_>>();
        let mut overlap_is_partial = false;
        let included = aggregates
            .iter()
            .filter(|candidate| {
                let candidate_interval = cost_interval(candidate, &lifetime_starts);
                !aggregates.iter().any(|cover| {
                    if cover.owner == candidate.owner || !self.covers(cover.owner, candidate.owner)
                    {
                        return false;
                    }
                    let (Some(candidate_interval), Some(cover_interval)) =
                        (candidate_interval, cost_interval(cover, &lifetime_starts))
                    else {
                        overlap_is_partial = true;
                        return true;
                    };
                    if !intervals_overlap(candidate_interval, cover_interval) {
                        return false;
                    }
                    if !interval_contains(cover_interval, candidate_interval) {
                        overlap_is_partial = true;
                    }
                    true
                })
            })
            .collect::<Vec<_>>();

        let mut total = None;
        let mut partial = overlap_is_partial;
        for aggregate in &included {
            total = Some(add_cost(total, aggregate.record.cost));
            // A report whose lifetime never began on a timed Turn cannot say
            // what work it spans, as the startup fold also reads it.
            partial |=
                aggregate.record.is_partial || cost_interval(aggregate, &lifetime_starts).is_none();
        }

        for record in records
            .iter()
            .filter(|record| record.record.coverage == CostCoverage::Turn)
        {
            let Some(started_at) = record.turn_started_at else {
                total = Some(add_cost(total, record.record.cost));
                partial = true;
                continue;
            };
            let interval = (started_at, record.record.recorded_at);
            let overlapping = included
                .iter()
                .copied()
                .filter(|aggregate| {
                    self.covers(aggregate.owner, record.owner)
                        && cost_interval(aggregate, &lifetime_starts)
                            .is_some_and(|coverage| intervals_overlap(interval, coverage))
                })
                .max_by_key(|aggregate| aggregate.record.recorded_at);
            if let Some(aggregate) = overlapping {
                let coverage = cost_interval(aggregate, &lifetime_starts)
                    .expect("an overlapping aggregate has an interval");
                partial |= !interval_contains(coverage, interval);
            } else {
                total = Some(add_cost(total, record.record.cost));
                partial |= record.record.is_partial;
            }
        }

        if total.is_some() {
            for &owner in sessions {
                for turn in &self.sessions[&owner].snapshot.turns {
                    let covering = turn.started_at.and_then(|started_at| {
                        included
                            .iter()
                            .copied()
                            .filter(|aggregate| {
                                self.covers(aggregate.owner, owner)
                                    && cost_interval(aggregate, &lifetime_starts).is_some_and(
                                        |coverage| {
                                            coverage.0 <= started_at && started_at <= coverage.1
                                        },
                                    )
                            })
                            .max_by_key(|aggregate| aggregate.record.recorded_at)
                    });
                    if let Some(aggregate) = covering {
                        partial |= work_exceeds_coverage(
                            turn.last_output_at,
                            turn.status == TurnStatus::Active,
                            aggregate.owner == owner && aggregate.turn_id == turn.id,
                            aggregate.record.recorded_at,
                        );
                    } else if turn.cost.is_none() || turn.status == TurnStatus::Active {
                        partial = true;
                    }
                }
            }
        }

        total.map(|cost| CostTotal {
            cost,
            is_partial: partial,
        })
    }

    /// Whether a whole-tree amount `reporter`'s Provider reported can cover
    /// `worker`'s work: only where `worker` is `reporter` or a native
    /// Subagent beneath it, reached without crossing a brokered Session. A
    /// brokered Subagent's work is metered by a Provider actor of its own
    /// (ADR 0035), so no report from above it ever holds that work (ADR
    /// 0040).
    fn covers(&self, reporter: SessionId, mut worker: SessionId) -> bool {
        loop {
            if reporter == worker {
                return true;
            }
            let Some(record) = self.sessions.get(&worker) else {
                return false;
            };
            if record.is_brokered_subagent() {
                return false;
            }
            let Some(parent) = record.snapshot.session.parent else {
                return false;
            };
            worker = parent;
        }
    }
}

#[derive(Clone)]
struct ScopedCost {
    owner: SessionId,
    turn_id: TurnId,
    turn_started_at: Option<SessionTimestamp>,
    record: CostRecord,
}

fn cost_records(owner: SessionId, turn: &Turn) -> Vec<ScopedCost> {
    let Some(cost) = turn.cost else {
        return Vec::new();
    };
    let (Some(basis), Some(details)) = (turn.cost_basis, turn.cost_details.as_ref()) else {
        return Vec::new();
    };
    let mut records = details
        .prior
        .iter()
        .cloned()
        .map(|record| ScopedCost {
            owner,
            turn_id: turn.id,
            turn_started_at: turn.started_at,
            record,
        })
        .collect::<Vec<_>>();
    records.push(ScopedCost {
        owner,
        turn_id: turn.id,
        turn_started_at: turn.started_at,
        record: CostRecord {
            cost,
            basis,
            coverage: details.coverage.clone(),
            recorded_at: details.recorded_at,
            is_partial: details.is_partial,
        },
    });
    records
}

fn reporting_lifetime(record: &CostRecord) -> Option<&str> {
    match &record.coverage {
        CostCoverage::Turn => None,
        CostCoverage::SessionSubtree { reporting_lifetime } => Some(reporting_lifetime),
    }
}

fn cost_interval(
    cost: &ScopedCost,
    lifetime_starts: &HashMap<String, SessionTimestamp>,
) -> Option<(SessionTimestamp, SessionTimestamp)> {
    let lifetime = reporting_lifetime(&cost.record)?;
    Some((*lifetime_starts.get(lifetime)?, cost.record.recorded_at))
}

pub(super) fn intervals_overlap(
    left: (SessionTimestamp, SessionTimestamp),
    right: (SessionTimestamp, SessionTimestamp),
) -> bool {
    left.0 <= right.1 && right.0 <= left.1
}

pub(super) fn interval_contains(
    coverage: (SessionTimestamp, SessionTimestamp),
    interval: (SessionTimestamp, SessionTimestamp),
) -> bool {
    coverage.0 <= interval.0 && coverage.1 >= interval.1
}

pub(super) fn work_exceeds_coverage(
    last_output_at: Option<SessionTimestamp>,
    active: bool,
    is_report_carrier: bool,
    coverage_end: SessionTimestamp,
) -> bool {
    last_output_at.is_some_and(|output| output > coverage_end) || active && !is_report_carrier
}

fn add_cost(total: Option<Cost>, cost: Cost) -> Cost {
    total.map_or(cost, |total| total.saturating_add(cost))
}

/// Two tree Costs stated as one: their sum, partial where either is.
pub(super) fn add_cost_totals(total: Option<CostTotal>, cost: CostTotal) -> Option<CostTotal> {
    Some(total.map_or(cost, |total| CostTotal {
        cost: total.cost.saturating_add(cost.cost),
        is_partial: total.is_partial || cost.is_partial,
    }))
}

/// A known total marked partial where work beside it is unpriced; an unknown
/// total stays absent rather than becoming a zero nobody measured.
pub(super) fn with_unpriced_work(total: Option<CostTotal>, unpriced: bool) -> Option<CostTotal> {
    total.map(|total| CostTotal {
        is_partial: total.is_partial || unpriced,
        ..total
    })
}
