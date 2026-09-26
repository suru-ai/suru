//! Watch Outcomes: how a Watch settled, recorded where its settling woke the
//! Agent (ADR 0030).
//!
//! A Watch that settles while a Turn is active in its Session wakes the Agent
//! into that Turn, so its outcome is recorded there at once. One that settles
//! while the Session is idle wakes the Agent into a Continuation the Provider
//! has not begun yet — Claude reports a task's end before its loop wakes — so
//! the outcome is held until the Session's next Turn opens and recorded at its
//! head, where the reader meets it before anything the Agent did about it.

use std::collections::HashMap;

use crate::{
    ansi::normalize_provider_text,
    protocol::{Activity, ActivityId, SessionChange, SessionId, TurnId, WatchOutcomeStatus},
    provider::ProviderWatchOutcome,
    sessions::SessionStore,
};

/// How one Watch settled, as its Watch Outcome will record it.
#[derive(Debug)]
pub(super) struct WatchOutcome {
    status: WatchOutcomeStatus,
    description: String,
    summary: Option<String>,
}

impl WatchOutcome {
    /// The outcome a settled Watch records, or `None` when its settling woke
    /// nothing: a Watch stopped by an interrupt or lost with its Provider
    /// process gave the Agent nothing to react to, so the Transcript has
    /// nothing to explain.
    pub(super) fn of(
        outcome: ProviderWatchOutcome,
        woke_agent: bool,
        description: &str,
        summary: Option<&str>,
    ) -> Option<Self> {
        if !woke_agent {
            return None;
        }
        let status = match outcome {
            ProviderWatchOutcome::Completed => WatchOutcomeStatus::Completed,
            ProviderWatchOutcome::Failed => WatchOutcomeStatus::Failed,
            ProviderWatchOutcome::Stopped => WatchOutcomeStatus::Stopped,
            ProviderWatchOutcome::Lost => return None,
        };
        Some(Self {
            status,
            description: normalize_provider_text(description),
            summary: summary
                .map(normalize_provider_text)
                .filter(|summary| !summary.trim().is_empty()),
        })
    }

    /// Records the outcome in a Turn of `session_id` as an Activity already
    /// settled, since the Watch it tells of has ended.
    pub(super) fn record(
        self,
        sessions: &SessionStore,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> anyhow::Result<()> {
        sessions.publish_agent_output(
            session_id,
            SessionChange::ActivityAdded {
                activity: Activity::WatchOutcome {
                    id: ActivityId::new(),
                    turn_id,
                    status: self.status,
                    description: self.description,
                    summary: self.summary,
                },
            },
        )?;
        Ok(())
    }
}

/// The Watch Outcomes waiting, by the Session whose Agent their Watches woke,
/// for that Session's next Turn to open. Held outcomes live only as long as
/// the Provider connection that reported them: a wake the connection never
/// delivered begins no Turn to explain.
#[derive(Debug, Default)]
pub(super) struct HeldWatchOutcomes {
    held: HashMap<SessionId, Vec<WatchOutcome>>,
}

impl HeldWatchOutcomes {
    /// Holds an outcome until a Turn opens in `session_id`.
    pub(super) fn hold(&mut self, session_id: SessionId, outcome: WatchOutcome) {
        self.held.entry(session_id).or_default().push(outcome);
    }

    /// Records every outcome held for `session_id` in the Turn that just
    /// opened there, in the order the Watches settled. The caller releases
    /// before projecting anything else into the Turn, so the outcomes stand
    /// at its head: the Turn is the one the wake began — or the Prompt's that
    /// won the race to it, which is where the Agent then hears of it.
    pub(super) fn release(
        &mut self,
        sessions: &SessionStore,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> anyhow::Result<()> {
        for outcome in self.held.remove(&session_id).unwrap_or_default() {
            outcome.record(sessions, session_id, turn_id)?;
        }
        Ok(())
    }

    /// Drops every held outcome, because the wake it waited on will begin no
    /// Turn: the Session was interrupted first, or the Provider connection that
    /// would have delivered it is gone.
    pub(super) fn drop_all(&mut self) {
        self.held.clear();
    }
}
