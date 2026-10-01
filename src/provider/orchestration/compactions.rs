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
//! compacted nothing. One that never reported compacting at all fails with
//! that said, and holds no Compaction: nothing is guessed.

use crate::{
    ansi::normalize_provider_text,
    protocol::{
        Activity, ActivityId, ActivityStatus, CompactionTrigger, SessionChange, SessionId, TurnId,
    },
    sessions::{ProviderTurnOutcome, SessionStore, TrailingCommandOutput},
};

/// Why a requested Compaction's Turn fails when its Provider ends the Turn
/// without ever reporting the Compaction it was asked for.
const NOTHING_COMPACTED: &str =
    "Provider execution failed: the Provider ended the Turn without reporting a Compaction.";

/// How the Provider reported a Compaction ending.
pub(super) enum CompactionOutcome {
    Completed {
        before_tokens: Option<u64>,
        after_tokens: Option<u64>,
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

impl CompactionOutcome {
    /// The status the Compaction settles as, and its record once settled: the
    /// Context Fill before and after, and why it failed.
    fn into_record(self) -> (ActivityStatus, Option<u64>, Option<u64>, Option<String>) {
        match self {
            Self::Completed {
                before_tokens,
                after_tokens,
            } => (ActivityStatus::Completed, before_tokens, after_tokens, None),
            Self::Failed {
                stop_requested: true,
                ..
            } => (ActivityStatus::Interrupted, None, None, None),
            Self::Failed {
                error,
                stop_requested: false,
            } => (
                ActivityStatus::Failed,
                None,
                None,
                error
                    .map(|error| normalize_provider_text(&error))
                    .filter(|error| !error.trim().is_empty()),
            ),
        }
    }
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
        let (status, before_tokens, after_tokens, error) = outcome.into_record();
        let change = match self.active.take() {
            Some(activity_id) => SessionChange::CompactionSettled {
                activity_id,
                status,
                before_tokens,
                after_tokens,
                error,
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
                },
            },
        };
        sessions.publish_agent_output(session_id, change)?;
        if self.requested {
            self.settled = Some(status);
        }
        Ok(())
    }

    /// How the Turn a Compaction request began Settles once its Provider
    /// reports the Turn complete: as its Compaction did. One the Provider left
    /// running Settles with the Turn as failed (ADR 0039). Either way the
    /// Compaction says why, so the Turn fails with nothing stood beside it. A
    /// Provider that never reported compacting fails the Turn saying so.
    pub(super) fn requested_turn_outcome(
        &self,
        trailing_output: TrailingCommandOutput,
    ) -> ProviderTurnOutcome {
        match self.settled {
            Some(ActivityStatus::Completed) => ProviderTurnOutcome::Completed { trailing_output },
            Some(ActivityStatus::Interrupted) => {
                ProviderTurnOutcome::Interrupted { trailing_output }
            }
            Some(ActivityStatus::Failed | ActivityStatus::Active) => {
                ProviderTurnOutcome::CompactionFailed { trailing_output }
            }
            None if self.active.is_some() => {
                ProviderTurnOutcome::CompactionFailed { trailing_output }
            }
            None => ProviderTurnOutcome::Failed {
                trailing_output,
                message: NOTHING_COMPACTED.to_owned(),
            },
        }
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
            },
        }
    }
}
