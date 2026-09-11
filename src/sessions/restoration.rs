//! Startup-only projection of durable Turns through the readable Session forest.

use std::collections::{BTreeMap, HashMap};

use crate::protocol::{SessionId, SessionTimestamp, UsageTotal};

use super::SessionStoreState;

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

        let mut intervals: HashMap<SessionId, WorkingIntervals> = HashMap::new();
        for id in walk.into_iter().rev() {
            record_work(1);
            let mut working = WorkingIntervals::default();
            let mut delegated: Option<UsageTotal> = None;
            for child in children.get(&id).into_iter().flatten() {
                record_work(1);
                if let Some(usage) = self.sessions[child].summary.total_usage {
                    delegated = Some(delegated.map_or(usage, |total| total.saturating_add(usage)));
                }
                if let Some(child_intervals) = intervals.remove(child) {
                    working.merge(child_intervals);
                }
            }
            let record = self.sessions.get_mut(&id).expect("indexed Session exists");
            for turn in &record.snapshot.turns {
                record_work(1);
                let Some(start) = turn.started_at else {
                    continue;
                };
                let end = if turn.status.is_terminal() {
                    let Some(end) = turn.settled_at else { continue };
                    Some(end)
                } else {
                    None
                };
                working.insert(start, end);
            }
            let reading = working.working_since();
            record.snapshot.session.working_since = reading;
            record.summary.session.working_since = reading;
            // A Prompt admitted to begin a Turn is owed one only by the
            // process that admitted it: nothing survives a restart to start
            // it, so a restored Session is at work only where a durable Turn
            // says so, and its status says the same thing its Working reading
            // does (ADR 0024).
            let status = if record
                .snapshot
                .turns
                .iter()
                .any(|turn| turn.status == crate::protocol::TurnStatus::Active)
            {
                crate::protocol::SessionStatus::Active
            } else {
                crate::protocol::SessionStatus::Idle
            };
            record.snapshot.session.status = status;
            record.summary.session.status = status;
            record.snapshot.subagent_usage = delegated;
            // Reads only this Session's Turns; child totals already include
            // their descendants and preserve absent measurements versus zero.
            record_work(record.snapshot.turns.len());
            record.summary.total_usage = record.snapshot.total_usage();
            if record.snapshot.session.parent.is_some() {
                intervals.insert(id, working);
            }
        }
    }
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
