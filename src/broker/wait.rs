//! `wait_subagents`: blocking a Broker call until a brokered Subagent it
//! waits on settles, or its bound passes (ADR 0035).
//!
//! A wait watches the Sessions it waits on through the store's own feeds of
//! their changes — the ones clients stream — and reads again how each stands
//! whenever one of their Turns moves, so it answers the moment a Subagent
//! settles without polling. While it waits it reports progress on a fixed
//! cadence, because a harness closes a Tool call it hears nothing from: that
//! is what keeps Claude's idle window open (ADR 0034). A wait answering with a
//! settle takes nothing from the Subagent Report of it, which reaches the
//! delegating Agent as ever, so the same settle may reach one Turn twice.

use std::time::Duration;

use tokio::{
    sync::broadcast::{self, error::RecvError},
    time::{Instant, MissedTickBehavior},
};

use super::tools::{ProgressReporter, ToolProgress};
use crate::protocol::{SessionChange, SessionId, SessionUpdate, TurnStatus};
use crate::sessions::{BrokeredReadError, BrokeredSubagentReading, SessionStore};

/// The timeout a wait keeps when its call names none, in seconds.
pub(super) const DEFAULT_TIMEOUT_SECONDS: u64 = 60;
/// The least a wait waits, in seconds, whatever its call asks.
pub(super) const MIN_TIMEOUT_SECONDS: u64 = 10;
/// The most a wait waits, in seconds, whatever its call asks: below the
/// 900 seconds every harness is told a Broker call may take, so a wait always
/// answers before its harness gives up on it.
pub(super) const MAX_TIMEOUT_SECONDS: u64 = 600;

/// The clock a wait keeps: how long each of the seconds its timeout counts
/// lasts, and how often it reports progress while it waits. Injectable, so a
/// test sees a wait time out and report progress at millisecond scale rather
/// than waiting either out.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WaitTimings {
    pub(crate) second: Duration,
    pub(crate) progress_every: Duration,
}

impl Default for WaitTimings {
    /// Real seconds, and progress every 30 of them: well inside the 60 seconds
    /// Claude gives a call to begin answering and the 180 of silence Copilot
    /// allows one (`docs/validation/0408-*`).
    fn default() -> Self {
        Self {
            second: Duration::from_secs(1),
            progress_every: Duration::from_secs(30),
        }
    }
}

/// `timeout_seconds` as a wait keeps it: whole seconds, no fewer than
/// [`MIN_TIMEOUT_SECONDS`] and no more than [`MAX_TIMEOUT_SECONDS`].
pub(super) fn kept_timeout(timeout_seconds: f64) -> u64 {
    let rounded = timeout_seconds.round();
    if rounded <= MIN_TIMEOUT_SECONDS as f64 {
        MIN_TIMEOUT_SECONDS
    } else if rounded >= MAX_TIMEOUT_SECONDS as f64 {
        MAX_TIMEOUT_SECONDS
    } else {
        rounded as u64
    }
}

/// How a wait ended.
#[derive(Debug)]
pub(super) enum WaitOutcome {
    /// Every Subagent waited on that has settled, as each now stands: at
    /// least one.
    Settled(Vec<BrokeredSubagentReading>),
    /// None settled before the bound passed.
    TimedOut,
}

/// Why a wait could not go on: the Subagent named could not be reached by the
/// Agent waiting.
#[derive(Debug)]
pub(super) struct Unreachable {
    pub(super) subagent: SessionId,
    pub(super) error: BrokeredReadError,
}

/// Waits, for the Agent of `caller`, until any of the brokered Subagents
/// `waited_on` has settled — answering at once where one already has — or
/// until `timeout_seconds` of `timings`' seconds have passed, reporting
/// progress through `progress` every `timings.progress_every` meanwhile, and
/// once as soon as it begins to wait, which opens the answer's stream at once.
/// Each Subagent must be a brokered Subagent beneath `caller`, as for a read;
/// the first that is not is refused.
pub(super) async fn until_one_settles(
    sessions: &SessionStore,
    timings: WaitTimings,
    caller: SessionId,
    waited_on: &[SessionId],
    timeout_seconds: u64,
    progress: Option<&ProgressReporter>,
) -> Result<WaitOutcome, Unreachable> {
    // Subscribed before the first reading, so a settle landing between the
    // reading and the wait is still heard.
    let mut feeds = waited_on
        .iter()
        .filter_map(|subagent| {
            sessions
                .subscribe(*subagent)
                .map(|feed| (*subagent, feed.updates))
        })
        .collect::<Vec<_>>();
    let began = Instant::now();
    let deadline = began
        + timings
            .second
            .saturating_mul(saturating_u32(timeout_seconds));
    // An interval takes no zero period, and a wait reporting without pause
    // would say nothing more.
    let every = timings.progress_every.max(Duration::from_millis(1));
    let mut ticks = tokio::time::interval_at(began + every, every);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut reports = ProgressReports {
        reporter: progress,
        seconds_between: every.as_secs_f64()
            / timings.second.max(Duration::from_nanos(1)).as_secs_f64(),
        waited_on: waited_on.len(),
        timeout_seconds,
        sent: 0,
    };
    loop {
        let settled = settled_among(sessions, caller, waited_on)?;
        if !settled.is_empty() {
            return Ok(WaitOutcome::Settled(settled));
        }
        if reports.sent == 0 {
            reports.send().await;
        }
        tokio::select! {
            biased;
            () = tokio::time::sleep_until(deadline) => return Ok(WaitOutcome::TimedOut),
            _ = ticks.tick(), if reports.reporter.is_some() => reports.send().await,
            () = next_turn_moving(sessions, &mut feeds) => {}
        }
    }
}

/// How each of `waited_on` stands, for the Agent of `caller`, where it has
/// settled.
fn settled_among(
    sessions: &SessionStore,
    caller: SessionId,
    waited_on: &[SessionId],
) -> Result<Vec<BrokeredSubagentReading>, Unreachable> {
    let mut settled = Vec::new();
    for subagent in waited_on {
        let reading = sessions
            .read_brokered_subagent(caller, *subagent)
            .map_err(|error| Unreachable {
                subagent: *subagent,
                error,
            })?;
        if reading.status != TurnStatus::Active {
            settled.push(reading);
        }
    }
    Ok(settled)
}

/// Waits until a Turn moves in any Session `feeds` follows — one begins or
/// settles — or until a feed has missed changes, since either may have
/// settled a Subagent waited on. A feed whose Session's record is replaced is
/// followed again from its new record, and one whose Session is gone is let
/// go; with none left, this never returns.
async fn next_turn_moving(
    sessions: &SessionStore,
    feeds: &mut Vec<(SessionId, broadcast::Receiver<SessionUpdate>)>,
) {
    loop {
        if feeds.is_empty() {
            return std::future::pending().await;
        }
        let (received, index, pending) = futures_util::future::select_all(
            feeds
                .iter_mut()
                .map(|(_, updates)| Box::pin(updates.recv())),
        )
        .await;
        drop(pending);
        match received {
            Ok(update) if !moves_a_turn(&update) => {}
            Ok(_) | Err(RecvError::Lagged(_)) => return,
            Err(RecvError::Closed) => {
                let (subagent, _) = feeds.swap_remove(index);
                if let Some(feed) = sessions.subscribe(subagent) {
                    feeds.push((subagent, feed.updates));
                }
                return;
            }
        }
    }
}

fn moves_a_turn(update: &SessionUpdate) -> bool {
    update.changes.iter().any(|change| {
        matches!(
            change,
            SessionChange::TurnAdded { .. } | SessionChange::TurnStatusChanged { .. }
        )
    })
}

/// The progress a wait reports while it waits: the seconds it has waited, of
/// its own counting, out of the seconds it may wait.
struct ProgressReports<'a> {
    reporter: Option<&'a ProgressReporter>,
    /// How many of the wait's seconds pass between two reports.
    seconds_between: f64,
    waited_on: usize,
    timeout_seconds: u64,
    sent: u32,
}

impl ProgressReports<'_> {
    async fn send(&mut self) {
        let Some(reporter) = self.reporter else {
            return;
        };
        // Counted in the progress cadence rather than read off the clock, so
        // each report says more than the last, as MCP requires, however
        // closely two of them follow.
        let waited = f64::from(self.sent) * self.seconds_between;
        self.sent = self.sent.saturating_add(1);
        let subagents = if self.waited_on == 1 {
            "Subagent"
        } else {
            "Subagents"
        };
        reporter(ToolProgress {
            progress: waited,
            total: self.timeout_seconds as f64,
            message: format!(
                "Waiting on {} {subagents} for up to {} seconds; {waited:.0} seconds so far.",
                self.waited_on, self.timeout_seconds
            ),
        })
        .await;
    }
}

fn saturating_u32(value: u64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timeout_is_kept_in_whole_seconds_between_the_floor_and_the_ceiling() {
        for (asked, kept) in [
            (60.0, 60),
            (10.0, 10),
            (600.0, 600),
            (9.4, 10),
            (-3.0, 10),
            (0.0, 10),
            (90.4, 90),
            (90.5, 91),
            (600.4, 600),
            (5_000.0, 600),
            (f64::MAX, 600),
        ] {
            assert_eq!(kept_timeout(asked), kept, "{asked} is kept as {kept}");
        }
    }

    #[test]
    fn a_wait_keeps_real_seconds_and_reports_progress_every_thirty_of_them() {
        assert_eq!(
            WaitTimings::default(),
            WaitTimings {
                second: Duration::from_secs(1),
                progress_every: Duration::from_secs(30),
            }
        );
        assert_eq!(DEFAULT_TIMEOUT_SECONDS, 60);
        assert!(
            u128::from(MAX_TIMEOUT_SECONDS) * 1_000
                < u128::from(super::super::BROKER_CALL_TIMEOUT_MS),
            "the longest wait answers before any harness gives up on the call"
        );
    }
}
