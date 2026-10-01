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

use crate::{
    ansi::normalize_provider_text,
    protocol::{
        Activity, ActivityId, ActivityStatus, CompactionTrigger, SessionChange, SessionId, TurnId,
    },
    sessions::SessionStore,
};

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
    active: Option<ActivityId>,
}

impl LiveCompaction {
    /// Records a Compaction beginning in `turn_id`, unless one is already
    /// Active there: then the Provider is only saying it is still at it.
    pub(super) fn start(
        &mut self,
        sessions: &SessionStore,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> anyhow::Result<()> {
        if self.active.is_some() {
            return Ok(());
        }
        let activity_id = ActivityId::new();
        sessions.publish_agent_output(session_id, opened(activity_id, turn_id))?;
        self.active = Some(activity_id);
        Ok(())
    }

    /// Settles the Active Compaction as the Provider reported, or records one
    /// already settled where the Provider reported no start.
    pub(super) fn settle(
        &mut self,
        sessions: &SessionStore,
        session_id: SessionId,
        turn_id: TurnId,
        outcome: CompactionOutcome,
    ) -> anyhow::Result<()> {
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
                    trigger: trigger(),
                    before_tokens,
                    after_tokens,
                    error,
                },
            },
        };
        sessions.publish_agent_output(session_id, change)?;
        Ok(())
    }
}

/// A Compaction as it opens: Active, with nothing known yet of how it ends.
fn opened(id: ActivityId, turn_id: TurnId) -> SessionChange {
    SessionChange::ActivityAdded {
        activity: Activity::Compaction {
            id,
            turn_id,
            status: ActivityStatus::Active,
            trigger: trigger(),
            before_tokens: None,
            after_tokens: None,
            error: None,
        },
    }
}

/// Which kind a Compaction is comes from the Turn holding it, and only a Turn
/// Suru began for a request holds a manual one (ADR 0041). Suru begins none
/// yet, so every Compaction is the Provider's own choice.
const fn trigger() -> CompactionTrigger {
    CompactionTrigger::Automatic
}
