//! How a listed Session reads: its Standing, and whether it stands among the
//! settled. Both are derived wherever Sessions are listed rather than stored,
//! and every listing reads them here — the Sidebar's shelves and Rails, and a
//! Sidekick's `list_sessions` — so what a Sidekick is told of a Session is
//! what the user sees of it.

use super::{AutoSettle, SessionListItem, SessionTimestamp, TurnStatus};

/// What a listed Session says about its work: its Standing. The ordering is
/// part of the reading: when more than one input applies, the first variant
/// here is the one the Session presents.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionStanding {
    NeedsIntervention,
    Working,
    Failed,
    /// Nothing is Working, but a Watch the Agent left running may still wake
    /// it. It ranks below Failed so an unseen failure is not hidden by the
    /// waiting it left behind, and gives way to it once the failure is Viewed.
    Monitoring,
    Done,
}

/// The facts from which a Session's Standing is read.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StandingReading {
    pub needs_intervention: bool,
    pub working: bool,
    pub failed: bool,
    pub monitoring: bool,
    pub done: bool,
}

impl StandingReading {
    /// The facts `session`'s listing carries. A Session Suru could not read
    /// carries none of them, and so reads no Standing at all.
    pub fn of(session: &SessionListItem) -> Self {
        let Some(summary) = session.readable() else {
            return Self::default();
        };
        let inputs = &summary.standing_inputs;
        Self {
            needs_intervention: inputs.pending_questionnaire_count() > 0
                || inputs.pending_approval_count() > 0,
            working: summary.session.working_since.is_some(),
            failed: inputs.latest_turn_settled_as(TurnStatus::Failed),
            monitoring: summary.session.monitoring_since.is_some(),
            done: inputs.latest_turn_settled_as(TurnStatus::Completed),
        }
    }

    /// The facts as a reader who has the Session open reads them. Opening is
    /// itself a Client's Viewed report, so the outcome readings — Failed and
    /// Done — clear optimistically while the Server's stamped moment makes its
    /// round trip; live readings still describe work and stay.
    pub const fn viewed(self) -> Self {
        Self {
            failed: false,
            done: false,
            ..self
        }
    }

    /// The facts of two Sessions read as one row's: whatever holds of either.
    /// A row standing for more than one Session — a Sidekick's Session
    /// carrying the Subsessions it hides — reads this way, so it presents the
    /// highest-precedence Standing among them.
    pub const fn alongside(self, other: Self) -> Self {
        Self {
            needs_intervention: self.needs_intervention || other.needs_intervention,
            working: self.working || other.working,
            failed: self.failed || other.failed,
            monitoring: self.monitoring || other.monitoring,
            done: self.done || other.done,
        }
    }

    /// The Standing these facts present, by the precedence
    /// [`SessionStanding`] lists, or nothing where none applies.
    pub const fn standing(self) -> Option<SessionStanding> {
        if self.needs_intervention {
            Some(SessionStanding::NeedsIntervention)
        } else if self.working {
            Some(SessionStanding::Working)
        } else if self.failed {
            Some(SessionStanding::Failed)
        } else if self.monitoring {
            Some(SessionStanding::Monitoring)
        } else if self.done {
            Some(SessionStanding::Done)
        } else {
            None
        }
    }
}

impl SessionListItem {
    /// The Standing this Session presents wherever it is listed.
    pub fn standing(&self) -> Option<SessionStanding> {
        StandingReading::of(self).standing()
    }
}

impl AutoSettle {
    /// Whether `session` stands among the settled as of `now`.
    ///
    /// Two things settle one and only the first is written down. The reader's
    /// own say-so is stamped on the Session by the server and always wins.
    /// Settling on its own is derived here, from the Session's last activity
    /// and this Setting: nothing is stored for it, no clock has to fire for
    /// it, and work that moves is active again at the very next reading.
    pub fn settles(self, session: &SessionListItem, now: SessionTimestamp) -> bool {
        session.settled_at().is_some() || self.left_alone(session, now)
    }

    /// Whether `session` has been left alone long enough, as of `now`, to
    /// settle itself.
    ///
    /// The idle is measured from the Session's last activity, so this asks
    /// after work there was: a Session nothing has moved since it was made has
    /// set nothing aside — the reader made it and it is theirs to prompt — and
    /// a Session Suru could not read has no activity it can see, which is the
    /// same reason it is never settled by the marker either. A Session Working
    /// or Monitoring is not done however long ago it last moved — a long Turn
    /// or a Watch can outlast the threshold without any output — so it is
    /// never left alone.
    pub fn left_alone(self, session: &SessionListItem, now: SessionTimestamp) -> bool {
        let Some(idle) = self.idle_millis() else {
            return false;
        };
        let last_activity = session.updated_at();
        if session.readable().is_none()
            || last_activity == session.created_at()
            || session.working_since().is_some()
            || session.monitoring_since().is_some()
        {
            return false;
        }
        now.0.saturating_sub(last_activity.0) >= idle
    }
}

#[cfg(test)]
mod tests {
    use super::{SessionStanding, StandingReading};

    #[test]
    fn a_standing_is_read_by_its_full_precedence() {
        let cases = [
            (
                StandingReading {
                    needs_intervention: true,
                    working: true,
                    failed: true,
                    monitoring: true,
                    done: true,
                },
                Some(SessionStanding::NeedsIntervention),
            ),
            (
                StandingReading {
                    working: true,
                    failed: true,
                    monitoring: true,
                    done: true,
                    ..StandingReading::default()
                },
                Some(SessionStanding::Working),
            ),
            (
                StandingReading {
                    failed: true,
                    done: true,
                    ..StandingReading::default()
                },
                Some(SessionStanding::Failed),
            ),
            (
                StandingReading {
                    failed: true,
                    monitoring: true,
                    done: true,
                    ..StandingReading::default()
                },
                Some(SessionStanding::Failed),
            ),
            (
                StandingReading {
                    monitoring: true,
                    done: true,
                    ..StandingReading::default()
                },
                Some(SessionStanding::Monitoring),
            ),
            (
                StandingReading {
                    done: true,
                    ..StandingReading::default()
                },
                Some(SessionStanding::Done),
            ),
            (StandingReading::default(), None),
        ];

        for (reading, expected) in cases {
            assert_eq!(reading.standing(), expected);
        }
    }

    #[test]
    fn readings_alongside_each_other_present_the_highest_standing_of_either() {
        let working = StandingReading {
            working: true,
            ..StandingReading::default()
        };
        let needing = StandingReading {
            needs_intervention: true,
            ..StandingReading::default()
        };
        let done = StandingReading {
            done: true,
            ..StandingReading::default()
        };
        assert_eq!(
            done.alongside(needing).standing(),
            Some(SessionStanding::NeedsIntervention)
        );
        assert_eq!(
            working.alongside(done).standing(),
            Some(SessionStanding::Working)
        );
        assert_eq!(
            StandingReading::default().alongside(done).standing(),
            Some(SessionStanding::Done)
        );
        assert_eq!(
            StandingReading::default()
                .alongside(StandingReading::default())
                .standing(),
            None
        );
    }

    #[test]
    fn a_viewed_reading_keeps_live_work_and_clears_outcomes() {
        let reading = StandingReading {
            failed: true,
            monitoring: true,
            done: true,
            ..StandingReading::default()
        };
        assert_eq!(
            reading.viewed().standing(),
            Some(SessionStanding::Monitoring)
        );
    }
}
