//! Startup-only projection of durable Turns through the readable Session forest.

use std::collections::{BTreeMap, HashMap};

use crate::protocol::{
    Cost, CostCoverage, CostRecord, CostTotal, SessionId, SessionStatus, SessionTimestamp, Turn,
    TurnId, TurnStatus, UsageTotal,
};

use super::{
    SessionStoreState,
    projection::{derived_session_status, interval_contains, work_exceeds_coverage},
};

impl SessionStoreState {
    /// Derive each Session once, after its children. The temporary index and
    /// interval sets are discarded before the store starts accepting changes.
    /// As before, only readable trees reachable from listed roots participate:
    /// missing/unreadable parents and cyclic components do not become roots.
    pub(super) fn restore_projections(&mut self) {
        let mut children: HashMap<SessionId, Vec<SessionId>> = HashMap::new();
        let mut walk = Vec::new();
        for (&id, record) in &self.sessions {
            record_work(1);
            if let Some(parent) = record.snapshot.session.parent {
                children.entry(parent).or_default().push(id);
            } else {
                walk.push(id);
            }
        }
        let mut visit = 0;
        while visit < walk.len() {
            record_work(1);
            if let Some(descendants) = children.get(&walk[visit]) {
                walk.extend_from_slice(descendants);
            }
            visit += 1;
        }

        // Reporting lifetimes can cross Session boundaries. Establish their
        // beginning once before the bottom-up fold so a descendant report and
        // an ancestor report use the same temporal extent without rescanning
        // either subtree.
        let mut lifetime_starts = HashMap::<String, SessionTimestamp>::new();
        for &id in &walk {
            for turn in &self.sessions[&id].snapshot.turns {
                record_work(1);
                let Some(started_at) = turn.started_at else {
                    continue;
                };
                for record in turn_cost_records(turn) {
                    record_work(1);
                    if let CostCoverage::SessionSubtree { reporting_lifetime } = &record.coverage {
                        lifetime_starts
                            .entry(reporting_lifetime.clone())
                            .and_modify(|current| *current = (*current).min(started_at))
                            .or_insert(started_at);
                    }
                }
            }
        }

        let mut intervals: HashMap<SessionId, WorkingIntervals> = HashMap::new();
        let mut cost_states = HashMap::<SessionId, RestoredCostState>::new();
        for id in walk.into_iter().rev() {
            record_work(1);
            let mut working = WorkingIntervals::default();
            let mut delegated: Option<UsageTotal> = None;
            let mut cost_state = RestoredCostState::default();
            for child in children.get(&id).into_iter().flatten() {
                record_work(1);
                if let Some(usage) = self.sessions[child].summary.total_usage {
                    delegated = Some(delegated.map_or(usage, |total| total.saturating_add(usage)));
                }
                if let Some(child_intervals) = intervals.remove(child) {
                    working.merge(child_intervals);
                }
                if let Some(mut child_costs) = cost_states.remove(child) {
                    // No report from above a brokered Subagent covers its
                    // work: its own Provider actor metered it (ADR 0040).
                    if self.sessions[child].is_brokered_subagent() {
                        child_costs.seal();
                    }
                    cost_state.merge(child_costs);
                }
            }
            cost_state.add_session(id, &self.sessions[&id].snapshot.turns, &lifetime_starts);
            let total_cost = cost_state.total();
            // One Session's own Turns are few beside its tree, so its own
            // Cost is read by the live derivation itself rather than folded.
            let own_cost = self.own_cost(id);
            let record = self.sessions.get_mut(&id).expect("indexed Session exists");
            for turn in &record.snapshot.turns {
                record_work(1);
                let Some(start) = turn.started_at else {
                    continue;
                };
                // A Turn read still open was run by a process this history
                // outlived, and hydration will settle it at the last moment
                // it showed work; the reading anticipates that settlement so
                // no listing reports Working for work nothing is doing
                // (ADR 0029).
                let end = if turn.status.is_terminal() {
                    let Some(end) = turn.settled_at else { continue };
                    end
                } else {
                    turn.last_output_at.unwrap_or(start)
                };
                working.insert(start, Some(end));
            }
            let reading = working.working_since();
            record.snapshot.session.working_since = reading;
            record.summary.session.working_since = reading;
            // A Prompt admitted to begin a Turn is owed one only by the
            // process that admitted it: nothing survives a restart to start
            // it, so a restored Session is at work only where a durable Turn
            // says so, and its status is derived the same way every other
            // commit derives it (ADR 0024). A history whose Turns break that
            // derivation keeps the status it was stored with.
            // A restored Session owes no Turn (admissions belonged to the
            // process that made them) and an open Turn is read as ended above,
            // so a history the derivation can read is Idle until hydration
            // settles that Turn for real (ADR 0029).
            let status = if derived_session_status(&record.snapshot, &record.turn_start_admissions)
                .is_ok()
            {
                SessionStatus::Idle
            } else {
                record.snapshot.session.status
            };
            record.snapshot.session.status = status;
            record.summary.session.status = status;
            record.snapshot.subagent_usage = delegated;
            record.snapshot.total_cost = total_cost;
            record.snapshot.own_cost = own_cost;
            record.summary.own_cost = own_cost;
            // Reads only this Session's Turns; child totals already include
            // their descendants and preserve absent measurements versus zero.
            record_work(record.snapshot.turns.len());
            record.summary.total_usage = record.snapshot.total_usage();
            if record.snapshot.session.parent.is_some() {
                intervals.insert(id, working);
            }
            cost_states.insert(id, cost_state);
        }
    }
}

#[derive(Default)]
struct RestoredCostState {
    components: Vec<RestoredCostComponent>,
    uncovered_work: Vec<RestoredWork>,
    total_cost: Option<Cost>,
    incomplete: usize,
    /// What brokered Subagents beneath contributed, settled for good: no
    /// ancestor's report may cover it, so it no longer answers to one.
    sealed_cost: Option<Cost>,
    sealed_incomplete: usize,
}

impl RestoredCostState {
    /// Fixes everything this state holds as its own contribution, beyond the
    /// reach of any report an ancestor folds over it — what a brokered
    /// Subagent's subtree is to every Session above it.
    fn seal(&mut self) {
        self.components.clear();
        self.uncovered_work.clear();
        self.sealed_cost = self.total_cost;
        self.sealed_incomplete = self.incomplete;
    }

    fn merge(&mut self, mut child: Self) {
        if self.components.len() < child.components.len() {
            std::mem::swap(&mut self.components, &mut child.components);
        }
        record_work(child.components.len());
        self.components.extend(child.components);
        if self.uncovered_work.len() < child.uncovered_work.len() {
            std::mem::swap(&mut self.uncovered_work, &mut child.uncovered_work);
        }
        record_work(child.uncovered_work.len());
        self.uncovered_work.extend(child.uncovered_work);
        self.total_cost = add_optional(self.total_cost, child.total_cost);
        self.incomplete = self.incomplete.saturating_add(child.incomplete);
        self.sealed_cost = add_optional(self.sealed_cost, child.sealed_cost);
        self.sealed_incomplete = self
            .sealed_incomplete
            .saturating_add(child.sealed_incomplete);
    }

    fn add_session(
        &mut self,
        owner: SessionId,
        turns: &[Turn],
        lifetime_starts: &HashMap<String, SessionTimestamp>,
    ) {
        let mut reports = HashMap::<String, (TurnId, CostRecord)>::new();
        for turn in turns {
            record_work(1);
            for record in turn_cost_records(turn) {
                record_work(1);
                match &record.coverage {
                    CostCoverage::Turn => self.push_component(RestoredCostComponent {
                        owner,
                        reporting_lifetime: None,
                        cost: record.cost,
                        interval: turn
                            .started_at
                            .map(|started_at| (started_at, record.recorded_at)),
                        is_partial: record.is_partial || turn.started_at.is_none(),
                    }),
                    CostCoverage::SessionSubtree { reporting_lifetime } => {
                        let replace = reports
                            .get(reporting_lifetime)
                            .is_none_or(|(_, current)| current.recorded_at < record.recorded_at);
                        if replace {
                            reports.insert(reporting_lifetime.clone(), (turn.id, record));
                        }
                    }
                }
            }
            if turn.cost.is_none()
                || turn.last_output_at.is_some()
                || turn.status == TurnStatus::Active
            {
                self.push_work(RestoredWork {
                    owner,
                    turn_id: turn.id,
                    started_at: turn.started_at,
                    last_output_at: turn.last_output_at,
                    active: turn.status == TurnStatus::Active,
                    // A settled Turn with its own known price is complete on
                    // its own. Retain its work evidence only so an ancestor
                    // report can decide whether suppressing that price leaves
                    // a temporal gap.
                    covered: turn.cost.is_some() && turn.status != TurnStatus::Active,
                });
            }
        }
        let mut reports = reports
            .into_iter()
            .map(|(lifetime, (turn_id, record))| RestoredReport {
                interval: lifetime_starts
                    .get(&lifetime)
                    .copied()
                    .map(|start| (start, record.recorded_at)),
                lifetime,
                turn_id,
                partial: record.is_partial,
                cost: record.cost,
            })
            .collect::<Vec<_>>();
        if reports.is_empty() {
            return;
        }
        reports.sort_unstable_by_key(|report| {
            report
                .interval
                .map(|interval| interval.0)
                .unwrap_or(SessionTimestamp(u64::MAX))
        });
        let report_index = ReportIndex::new(&reports);
        let untimed_report = reports.iter().position(|report| report.interval.is_none());
        let furthest_report = reports
            .iter()
            .enumerate()
            .max_by_key(|(_, report)| report.interval.map(|(_, end)| end))
            .map(|(index, _)| index)
            .expect("a non-empty report set has a furthest report");
        self.components.retain(|component| {
            record_work(1);
            if component.owner == owner && component.reporting_lifetime.is_some() {
                return true;
            }
            if component.reporting_lifetime.is_some()
                && component.owner != owner
                && component.interval.is_none()
            {
                reports[furthest_report].partial = true;
                return false;
            }
            if component.reporting_lifetime.is_some()
                && component.owner != owner
                && let Some(report_index) = untimed_report
            {
                reports[report_index].partial = true;
                return false;
            }
            let Some(interval) = component.interval else {
                return true;
            };
            let Some(report_index) = report_index.overlapping(interval) else {
                return true;
            };
            reports[report_index].partial |=
                !interval_contains(reports[report_index].interval.unwrap(), interval);
            false
        });
        self.uncovered_work.retain_mut(|work| {
            record_work(1);
            let Some(started_at) = work.started_at else {
                return true;
            };
            let Some(report_index) = report_index.overlapping((started_at, started_at)) else {
                return true;
            };
            let report = &reports[report_index];
            let coverage = report
                .interval
                .expect("the report index contains timed reports");
            let incomplete = work_exceeds_coverage(
                work.last_output_at,
                work.active,
                work.owner == owner && work.turn_id == report.turn_id,
                coverage.1,
            );
            work.covered = !incomplete;
            incomplete || work.active
        });
        self.recalculate();
        for report in reports {
            self.push_component(RestoredCostComponent {
                owner,
                reporting_lifetime: Some(report.lifetime),
                cost: report.cost,
                interval: report.interval,
                is_partial: report.partial || report.interval.is_none(),
            });
        }
    }

    fn total(&self) -> Option<CostTotal> {
        let cost = self.total_cost?;
        Some(CostTotal {
            cost,
            is_partial: self.incomplete > 0,
        })
    }

    fn push_component(&mut self, component: RestoredCostComponent) {
        self.total_cost = Some(
            self.total_cost
                .map_or(component.cost, |total| total.saturating_add(component.cost)),
        );
        self.incomplete = self
            .incomplete
            .saturating_add(usize::from(component.is_partial));
        self.components.push(component);
    }

    fn push_work(&mut self, work: RestoredWork) {
        self.incomplete = self.incomplete.saturating_add(usize::from(!work.covered));
        self.uncovered_work.push(work);
    }

    fn recalculate(&mut self) {
        self.total_cost = self.sealed_cost;
        self.incomplete = self.sealed_incomplete;
        for component in &self.components {
            record_work(1);
            self.total_cost = Some(
                self.total_cost
                    .map_or(component.cost, |total| total.saturating_add(component.cost)),
            );
            self.incomplete = self
                .incomplete
                .saturating_add(usize::from(component.is_partial));
        }
        for work in &self.uncovered_work {
            record_work(1);
            self.incomplete = self.incomplete.saturating_add(usize::from(!work.covered));
        }
    }
}

fn add_optional(left: Option<Cost>, right: Option<Cost>) -> Option<Cost> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.saturating_add(right)),
        (left, right) => left.or(right),
    }
}

struct RestoredCostComponent {
    owner: SessionId,
    reporting_lifetime: Option<String>,
    cost: Cost,
    interval: Option<(SessionTimestamp, SessionTimestamp)>,
    is_partial: bool,
}

struct RestoredReport {
    lifetime: String,
    turn_id: TurnId,
    cost: Cost,
    interval: Option<(SessionTimestamp, SessionTimestamp)>,
    partial: bool,
}

struct ReportIndex {
    starts: Vec<SessionTimestamp>,
    max_end_tree: Vec<Option<(SessionTimestamp, usize)>>,
    leaf_count: usize,
}

impl ReportIndex {
    fn new(reports: &[RestoredReport]) -> Self {
        let timed_reports = reports
            .iter()
            .enumerate()
            .filter_map(|(index, report)| report.interval.map(|_| index))
            .collect::<Vec<_>>();
        let starts = timed_reports
            .iter()
            .map(|&index| reports[index].interval.unwrap().0)
            .collect::<Vec<_>>();
        let leaf_count = timed_reports.len().max(1).next_power_of_two();
        let mut max_end_tree = vec![None; leaf_count * 2];
        for (offset, &index) in timed_reports.iter().enumerate() {
            max_end_tree[leaf_count + offset] = Some((reports[index].interval.unwrap().1, index));
        }
        for node in (1..leaf_count).rev() {
            max_end_tree[node] = max_end_tree[node * 2].max(max_end_tree[node * 2 + 1]);
        }
        Self {
            starts,
            max_end_tree,
            leaf_count,
        }
    }

    fn overlapping(&self, interval: (SessionTimestamp, SessionTimestamp)) -> Option<usize> {
        let upper = self.starts.partition_point(|start| *start <= interval.1);
        let mut left = self.leaf_count;
        let mut right = self.leaf_count + upper;
        let mut furthest = None;
        while left < right {
            if left % 2 == 1 {
                furthest = furthest.max(self.max_end_tree[left]);
                left += 1;
            }
            if right % 2 == 1 {
                right -= 1;
                furthest = furthest.max(self.max_end_tree[right]);
            }
            left /= 2;
            right /= 2;
        }
        furthest
            .filter(|(end, _)| *end >= interval.0)
            .map(|(_, report_index)| report_index)
    }
}

struct RestoredWork {
    owner: SessionId,
    turn_id: TurnId,
    started_at: Option<SessionTimestamp>,
    last_output_at: Option<SessionTimestamp>,
    active: bool,
    covered: bool,
}

fn turn_cost_records(turn: &Turn) -> Vec<CostRecord> {
    let Some(cost) = turn.cost else {
        return Vec::new();
    };
    let (Some(basis), Some(details)) = (turn.cost_basis, turn.cost_details.as_ref()) else {
        return Vec::new();
    };
    let mut records = details.prior.clone();
    records.push(CostRecord {
        cost,
        basis,
        coverage: details.coverage.clone(),
        recorded_at: details.recorded_at,
        is_partial: details.is_partial,
    });
    records
}

/// Disjoint, ordered components of the union of durable Turn intervals.
/// Closed components matter: an ancestor's interval may bridge them to live
/// work. Moving the smaller set into the larger avoids copying a descendant's
/// complete history at every level of a deep tree (O(T log² T) overall).
#[derive(Default)]
struct WorkingIntervals(BTreeMap<SessionTimestamp, Option<SessionTimestamp>>);

impl WorkingIntervals {
    fn merge(&mut self, mut other: Self) {
        if self.0.len() < other.0.len() {
            std::mem::swap(self, &mut other);
        }
        for (start, end) in other.0 {
            self.insert(start, end);
        }
    }

    fn insert(&mut self, mut start: SessionTimestamp, mut end: Option<SessionTimestamp>) {
        record_work(1);
        if let Some((&previous_start, &previous_end)) = self.0.range(..=start).next_back()
            && previous_end.is_none_or(|previous_end| start <= previous_end)
        {
            self.0.remove(&previous_start);
            start = previous_start;
            end = merged_end(end, previous_end);
        }
        while let Some((&next_start, &next_end)) = self.0.range(start..).next() {
            record_work(1);
            if end.is_some_and(|end| end < next_start) {
                break;
            }
            self.0.remove(&next_start);
            end = merged_end(end, next_end);
        }
        self.0.insert(start, end);
    }

    fn working_since(&self) -> Option<SessionTimestamp> {
        self.0
            .last_key_value()
            .and_then(|(&start, end)| end.is_none().then_some(start))
    }
}

fn merged_end(
    left: Option<SessionTimestamp>,
    right: Option<SessionTimestamp>,
) -> Option<SessionTimestamp> {
    Some(left?.max(right?))
}

// Count actual record/Turn visits and interval operations at the restoration
// boundary in tests; production builds erase this instrumentation entirely.
#[inline]
fn record_work(_count: usize) {
    #[cfg(test)]
    super::restoration_tests::record_work(_count);
}
