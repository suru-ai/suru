//! The Context Fill a Compaction carries where its Provider reported none
//! (CONTEXT.md: Compaction). A count the Provider reported always stands; a
//! side it left out is read from the Session's own Context Fill instead —
//! `before` as last read before the Compaction began, `after` as first read
//! once it Settled — and stays absent where no such reading exists, because a
//! Compaction carries nothing guessed.
//!
//! Every Compaction and every Context Fill reading reaches a Session through
//! its commits, so this fills the fallback in there, the same for every
//! Provider and whichever path recorded the Compaction. Only a Compaction Suru
//! saw begin takes a `before`: one reported only ending gives no moment to read
//! from. Only a completed one awaits an `after`: one that failed or was stopped
//! freed nothing a reading could measure, and it leaves the reading before it
//! standing for the next attempt. A reading taken while a Compaction runs
//! describes neither of its sides, since the Provider may be reading its own
//! summarising call or the context it is rebuilding, but it is still the first
//! reading after any Compaction that settled before it began.
//!
//! What is still owed is read off the Session's own history rather than kept
//! beside it: a completed Compaction with no `after` is one nothing has been
//! read since, because the first reading after it would have measured it. So
//! a Compaction settled before a restart takes the first reading the next
//! process commits, as it would have in this one.

use crate::protocol::{Activity, ActivityId, ActivityStatus, SessionChange, SessionSnapshot};

/// Fills in, across one commit's `changes` to `snapshot`, each side of a
/// Compaction's Context Fill its Provider left out that a reading can stand in
/// for.
pub(super) fn measure_compactions(snapshot: &SessionSnapshot, changes: &mut Vec<SessionChange>) {
    let reads = changes.iter().any(|change| {
        matches!(
            change,
            SessionChange::ContextFillChanged {
                context_fill: Some(_)
            }
        )
    });
    let mut awaiting_after = if reads {
        awaiting_after(snapshot)
    } else {
        Vec::new()
    };
    let mut last_read = snapshot
        .session
        .context_fill
        .map(|fill| fill.occupied_tokens);
    let mut measured = Vec::with_capacity(changes.len());
    for mut change in changes.drain(..) {
        let mut after = Vec::new();
        match &mut change {
            SessionChange::ActivityAdded {
                activity:
                    Activity::Compaction {
                        id,
                        status,
                        before_tokens,
                        after_tokens,
                        ..
                    },
            } => match status {
                ActivityStatus::Active if before_tokens.is_none() => *before_tokens = last_read,
                ActivityStatus::Completed if after_tokens.is_none() => awaiting_after.push(*id),
                _ => {}
            },
            // A settle carries the Compaction's whole record, so the `before`
            // it took as it began goes with it wherever the Provider reported
            // none.
            SessionChange::CompactionSettled {
                activity_id,
                status,
                before_tokens,
                after_tokens,
                ..
            } => {
                if before_tokens.is_none() {
                    *before_tokens = recorded_before(snapshot, *activity_id);
                }
                if *status == ActivityStatus::Completed && after_tokens.is_none() {
                    awaiting_after.push(*activity_id);
                }
            }
            SessionChange::ContextFillChanged { context_fill } => {
                last_read = context_fill.map(|fill| fill.occupied_tokens);
                if let Some(after_tokens) = last_read {
                    after = awaiting_after
                        .drain(..)
                        .map(|activity_id| SessionChange::CompactionAfterMeasured {
                            activity_id,
                            after_tokens,
                        })
                        .collect();
                }
            }
            _ => {}
        }
        measured.push(change);
        measured.extend(after);
    }
    *changes = measured;
}

/// Every completed Compaction in `snapshot` still waiting for the Session's
/// next Context Fill reading.
fn awaiting_after(snapshot: &SessionSnapshot) -> Vec<ActivityId> {
    snapshot
        .activities
        .iter()
        .filter_map(|activity| match activity {
            Activity::Compaction {
                id,
                status: ActivityStatus::Completed,
                after_tokens: None,
                ..
            } => Some(*id),
            _ => None,
        })
        .collect()
}

/// The `before` the Compaction `activity_id` holds in `snapshot`.
fn recorded_before(snapshot: &SessionSnapshot, activity_id: ActivityId) -> Option<u64> {
    snapshot
        .activities
        .iter()
        .find_map(|activity| match activity {
            Activity::Compaction {
                id, before_tokens, ..
            } if *id == activity_id => *before_tokens,
            _ => None,
        })
}
