//! Compactions: each occasion on which a Provider replaced what its Agent
//! remembers of a Session with a summary, recorded in the Turn it fell in.
//!
//! A Turn holds at most one Compaction the Provider is still summarising. A
//! Provider may restate that it is compacting for as long as it works — Claude
//! does so every half minute — so a start while one is Active is that same
//! occasion. A completion or a failure Settles the Active one; one arriving with
//! no start before it, as a Claude Subagent's does, records a Compaction settled
//! from the moment it stands. One still Active when its Turn Settles Settles
//! with it, as the Session store settles everything a Turn leaves in flight.
//!
//! A Provider reports the Compaction an interrupt cancelled as failed. Only
//! Suru knows it asked for that, so a failure reported once the Provider has
//! acknowledged Suru's interrupt of the Turn holding the Compaction Settles it
//! as interrupted instead (ADR 0039).
//!
//! A Turn a Compaction request began (ADR 0041) holds the one Compaction it
//! was begun for, which is manual, and nothing else: whatever the Provider
//! says of compacting once that one has Settled restates it. The Turn Settles
//! at the Provider's own boundary, but as its Compaction did rather than as
//! the boundary says, since a Provider may close the work with a success that
//! compacted nothing, or fail it for a cancellation Suru asked for. One that
//! never reported compacting at all fails with that said — in the Provider's
//! words where it failed the Turn — and holds no Compaction: nothing is
//! guessed. A Turn the user interrupted is stopped instead: once the Provider
//! has acknowledged Suru's interrupt, or ends the Turn as interrupted itself,
//! a Compaction it never settled was stopped, and so was a Turn it never
//! began compacting in.
//!
//! A completed Compaction keeps the summary its Provider gave, normalized like
//! any Provider text and cut to [`MAX_STORED_SUMMARY_CHARS`], with the cut
//! carried beside it as Truncation.

use crate::{
    ansi::{ProviderTextNormalizer, normalize_provider_text},
    protocol::{
        Activity, ActivityId, ActivityStatus, CompactionTrigger, SessionChange, SessionId, TurnId,
    },
    sessions::{ProviderTurnOutcome, SessionStore, TrailingCommandOutput},
};

/// Why a requested Compaction's Turn fails when its Provider ends the Turn
/// without ever reporting the Compaction it was asked for.
const NOTHING_COMPACTED: &str =
    "Provider execution failed: the Provider ended the Turn without reporting a Compaction.";

/// The most characters of a Compaction's summary Suru stores. A summary is
/// prose the Provider wrote for its Agent to carry on from, so it is capped as
/// Reasoning is rather than as a log: generous next to any real summary, which
/// runs to a few pages at most, and low enough that a Provider summarising
/// without end cannot grow one Activity without bound.
const MAX_STORED_SUMMARY_CHARS: usize = 64 * 1024;

/// How the Provider reported a Compaction ending.
pub(super) enum CompactionOutcome {
    Completed {
        before_tokens: Option<u64>,
        after_tokens: Option<u64>,
        summary: Option<String>,
    },
    /// The Provider reported it failing. `stop_requested` says whether Suru
    /// had asked the Provider to stop the work holding it, and had that
    /// acknowledged: then the failure is the cancellation Suru asked for, and
    /// the Compaction Settles interrupted, with no failure to explain.
    Failed {
        error: Option<String>,
        stop_requested: bool,
    },
}

/// A Compaction's record once it settles, as Suru stores it.
struct SettledRecord {
    status: ActivityStatus,
    before_tokens: Option<u64>,
    after_tokens: Option<u64>,
    error: Option<String>,
    summary: Option<String>,
    summary_truncated: bool,
}

impl CompactionOutcome {
    /// The status the Compaction settles as, and its record once settled: the
    /// Context Fill before and after, why it failed, and the summary it left.
    fn into_record(self) -> SettledRecord {
        let unmeasured = |status, error| SettledRecord {
            status,
            before_tokens: None,
            after_tokens: None,
            error,
            summary: None,
            summary_truncated: false,
        };
        match self {
            Self::Completed {
                before_tokens,
                after_tokens,
                summary,
            } => {
                let (summary, summary_truncated) = stored_summary(summary);
                SettledRecord {
                    status: ActivityStatus::Completed,
                    before_tokens,
                    after_tokens,
                    error: None,
                    summary,
                    summary_truncated,
                }
            }
            Self::Failed {
                stop_requested: true,
                ..
            } => unmeasured(ActivityStatus::Interrupted, None),
            Self::Failed {
                error,
                stop_requested: false,
            } => unmeasured(
                ActivityStatus::Failed,
                error
                    .map(|error| normalize_provider_text(&error))
                    .filter(|error| !error.trim().is_empty()),
            ),
        }
    }
}

/// A summary as Suru stores it: normalized like any Provider text, cut to
/// [`MAX_STORED_SUMMARY_CHARS`], and whether the cut dropped any of it. A
/// summary with nothing to read is no summary.
fn stored_summary(summary: Option<String>) -> (Option<String>, bool) {
    let Some(summary) = summary.filter(|summary| !summary.trim().is_empty()) else {
        return (None, false);
    };
    let stored =
        ProviderTextNormalizer::with_max_chars(MAX_STORED_SUMMARY_CHARS).push(summary.trim());
    (Some(stored.content), stored.truncated)
}

/// The Compaction one Turn holds while its Provider summarises.
#[derive(Debug, Default)]
pub(super) struct LiveCompaction {
    /// Whether a Compaction request began the Turn, which makes the
    /// Compaction it holds manual and the only one it holds.
    requested: bool,
    active: Option<ActivityId>,
    /// How the Compaction a request began the Turn for settled, once it has.
    settled: Option<ActivityStatus>,
}

impl LiveCompaction {
    /// The Compaction of a Turn a Compaction request began, which the
    /// Provider is about to report.
    pub(super) fn requested() -> Self {
        Self {
            requested: true,
            ..Self::default()
        }
    }

    /// Whether a Compaction request began the Turn this belongs to.
    pub(super) const fn is_requested(&self) -> bool {
        self.requested
    }

    /// Whether the Turn already holds every Compaction it may: the one a
    /// request began it for, once that has Settled however it ended.
    const fn requested_compaction_settled(&self) -> bool {
        self.requested && self.settled.is_some()
    }

    /// Which kind of Compaction the Turn holds, read from the Turn rather than
    /// from anything the Provider says (ADR 0041).
    const fn trigger(&self) -> CompactionTrigger {
        if self.requested {
            CompactionTrigger::Manual
        } else {
            CompactionTrigger::Automatic
        }
    }

    /// Records a Compaction beginning in `turn_id`, unless one is already
    /// Active there — then the Provider is only saying it is still at it — or
    /// the Turn already holds every Compaction it may.
    pub(super) fn start(
        &mut self,
        sessions: &SessionStore,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> anyhow::Result<()> {
        if self.active.is_some() || self.requested_compaction_settled() {
            return Ok(());
        }
        let activity_id = ActivityId::new();
        sessions.publish_agent_output(session_id, self.opened(activity_id, turn_id))?;
        self.active = Some(activity_id);
        Ok(())
    }

    /// Settles the Active Compaction as the Provider reported, or records one
    /// already settled where the Provider reported no start. A Turn holding
    /// every Compaction it may takes nothing more.
    pub(super) fn settle(
        &mut self,
        sessions: &SessionStore,
        session_id: SessionId,
        turn_id: TurnId,
        outcome: CompactionOutcome,
    ) -> anyhow::Result<()> {
        if self.requested_compaction_settled() {
            return Ok(());
        }
        let SettledRecord {
            status,
            before_tokens,
            after_tokens,
            error,
            summary,
            summary_truncated,
        } = outcome.into_record();
        let change = match self.active.take() {
            Some(activity_id) => SessionChange::CompactionSettled {
                activity_id,
                status,
                before_tokens,
                after_tokens,
                error,
                summary,
                summary_truncated,
            },
            None => SessionChange::ActivityAdded {
                activity: Activity::Compaction {
                    id: ActivityId::new(),
                    turn_id,
                    status,
                    trigger: self.trigger(),
                    before_tokens,
                    after_tokens,
                    error,
                    summary,
                    summary_truncated,
                },
            },
        };
        sessions.publish_agent_output(session_id, change)?;
        if self.requested {
            self.settled = Some(status);
        }
        Ok(())
    }

    /// How the Turn a Compaction request began Settles once its Provider ends
    /// it: as its Compaction did. `stopped` says whether the Provider stopped
    /// the work on request: it acknowledged Suru's interrupt, or ended the
    /// Turn as interrupted itself. Then a Compaction it left running, or never
    /// reported at all, was stopped, and the Turn Settles interrupted, with
    /// nothing stood beside a stop (ADR 0039). Otherwise one left running
    /// Settles with the Turn as failed, and says why, so the Turn fails with
    /// nothing stood beside it; and a Provider that never reported compacting
    /// fails the Turn saying so.
    pub(super) fn requested_turn_outcome(
        &self,
        trailing_output: TrailingCommandOutput,
        stopped: bool,
    ) -> ProviderTurnOutcome {
        match self.settled {
            Some(ActivityStatus::Completed) => ProviderTurnOutcome::Completed { trailing_output },
            Some(ActivityStatus::Interrupted) => {
                ProviderTurnOutcome::Interrupted { trailing_output }
            }
            Some(ActivityStatus::Failed | ActivityStatus::Active) => {
                ProviderTurnOutcome::CompactionFailed { trailing_output }
            }
            None if stopped => ProviderTurnOutcome::Interrupted { trailing_output },
            None if self.active.is_some() => {
                ProviderTurnOutcome::CompactionFailed { trailing_output }
            }
            None => ProviderTurnOutcome::Failed {
                trailing_output,
                message: NOTHING_COMPACTED.to_owned(),
            },
        }
    }

    /// How the Turn a Compaction request began Settles once its Provider
    /// fails it, saying why in `message` — as Codex fails the native turn a
    /// failed compaction ran in, and Copilot the request an abort cancelled.
    /// The Compaction still decides, as at any other boundary. One the
    /// Provider left running failed for the reason the Turn did, and settles
    /// so here, unless the Turn was `stopped`: then the failure is the stop
    /// Suru asked for, and the Turn Settles interrupted with it (ADR 0039).
    /// A Provider that failed before it reported compacting at all fails the
    /// Turn in its own words.
    pub(super) fn requested_turn_failed(
        &mut self,
        sessions: &SessionStore,
        session_id: SessionId,
        turn_id: TurnId,
        trailing_output: TrailingCommandOutput,
        message: String,
        stopped: bool,
    ) -> anyhow::Result<ProviderTurnOutcome> {
        if self.settled.is_none() && !stopped {
            if self.active.is_some() {
                self.settle(
                    sessions,
                    session_id,
                    turn_id,
                    CompactionOutcome::Failed {
                        error: Some(message),
                        stop_requested: false,
                    },
                )?;
            } else {
                return Ok(ProviderTurnOutcome::Failed {
                    trailing_output,
                    message,
                });
            }
        }
        Ok(self.requested_turn_outcome(trailing_output, stopped))
    }

    /// A Compaction as it opens: Active, with nothing known yet of how it ends.
    fn opened(&self, id: ActivityId, turn_id: TurnId) -> SessionChange {
        SessionChange::ActivityAdded {
            activity: Activity::Compaction {
                id,
                turn_id,
                status: ActivityStatus::Active,
                trigger: self.trigger(),
                before_tokens: None,
                after_tokens: None,
                error: None,
                summary: None,
                summary_truncated: false,
            },
        }
    }
}
