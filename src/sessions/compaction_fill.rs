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
//! describes neither side, since the Provider may be reading its own
//! summarising call or the context it is rebuilding.
//!
//! A reading describes only the context as the last Compaction left it. So a
//! Compaction beginning before anything was read after a completed one takes
//! no `before` — the last reading predates the context it compacts — and the
//! earlier one stops awaiting its `after`, which the next reading would read
//! across both.

use crate::protocol::{Activity, ActivityId, ActivityStatus, SessionChange, SessionSnapshot};

/// What one Session's commits still owe a Compaction's Context Fill. Never
/// stored: a restart forgets it, and a Compaction settled before the stop
/// keeps whatever it had, as it would had nothing been read since.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct CompactionFill {
    /// The completed Compaction whose `after` waits for the Session's next
    /// Context Fill reading.
    awaiting_after: Option<ActivityId>,
}

impl CompactionFill {
    /// Fills in, across one commit's `changes` to `snapshot`, each side of a
    /// Compaction's Context Fill its Provider left out that a reading can
    /// stand in for, and answers what is still owed once those changes land.
    pub(super) fn measure(
        self,
        snapshot: &SessionSnapshot,
        changes: &mut Vec<SessionChange>,
    ) -> Self {
        let mut awaiting_after = self.awaiting_after;
        let mut last_read = snapshot
            .session
            .context_fill
            .map(|fill| fill.occupied_tokens);
        let mut measured = Vec::with_capacity(changes.len());
        for mut change in changes.drain(..) {
            let mut after = None;
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
                    ActivityStatus::Active => {
                        let read_since_the_last = awaiting_after.take().is_none();
                        if before_tokens.is_none() && read_since_the_last {
                            *before_tokens = last_read;
                        }
                    }
                    ActivityStatus::Completed => {
                        awaiting_after = after_tokens.is_none().then_some(*id);
                    }
                    ActivityStatus::Failed | ActivityStatus::Interrupted => {}
                },
                // A settle carries the Compaction's whole record, so the
                // `before` it took as it began goes with it wherever the
                // Provider reported none.
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
                    if *status == ActivityStatus::Completed {
                        awaiting_after = after_tokens.is_none().then_some(*activity_id);
                    }
                }
                SessionChange::ContextFillChanged { context_fill } => {
                    last_read = context_fill.map(|fill| fill.occupied_tokens);
                    after = last_read.and_then(|after_tokens| {
                        awaiting_after.take().map(|activity_id| {
                            SessionChange::CompactionAfterMeasured {
                                activity_id,
                                after_tokens,
                            }
                        })
                    });
                }
                _ => {}
            }
            measured.push(change);
            measured.extend(after);
        }
        *changes = measured;
        Self { awaiting_after }
    }
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
