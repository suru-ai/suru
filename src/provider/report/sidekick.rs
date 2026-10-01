//! Sidekick Reports: the account Suru gives a Sidekick of a Session it set to
//! work — one it began, sent a Prompt, or answered — when that Session's Turn
//! settles or the Session comes to owe an Intervention (CONTEXT.md: Sidekick
//! Report). It is delivered as a Subagent Report is, and rendered here once,
//! in Suru's words, for an Agent to read: compact, naming the Session by its
//! Title and by the id the Sidekick's Tools take, saying what happened, and
//! naming the Tool that reads further or answers, without telling the
//! Sidekick what to do beyond that.

use std::fmt;

use super::{bounded, duration};
use crate::protocol::{Outlook, SessionId, SessionReference};
use crate::session_projection::agent_reading::{self, EntryNumber};

/// How the Turn a Sidekick Report tells of settled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SidekickTurnOutcome {
    Completed,
    /// Its Provider said it failed, or its Provider was lost under it.
    Failed,
    /// Someone stopped it: the user, or a Sidekick.
    Interrupted,
}

/// Which Intervention a Session came to owe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SidekickIntervention {
    /// A Questionnaire, awaiting an Answer a Sidekick may give.
    Questionnaire,
    /// An Approval, awaiting the user's Decision, which no Sidekick gives.
    Approval,
}

/// What a Sidekick Report tells of the Session it is about.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SidekickReportOccasion {
    /// A Turn of the Session settled.
    TurnSettled {
        outcome: SidekickTurnOutcome,
        /// How long the Turn worked; `None` where Suru never learned when it
        /// began or ended.
        duration_ms: Option<u64>,
        /// What a failed Turn failed with, as its Transcript says, cut like
        /// the excerpt; `None` for any other outcome, and for a failure
        /// nothing explained.
        error: Option<String>,
        /// The start of the final Message its Agent wrote in the Turn;
        /// `None` when it wrote none.
        final_message: Option<FinalMessage>,
    },
    /// The Session came to owe an Intervention: one of its own, or — where
    /// `subagent` names it — one of a Subagent's Session beneath it, which
    /// leaves the Session's work waiting just the same.
    InterventionOwed {
        intervention: SidekickIntervention,
        subagent: Option<SessionId>,
    },
}

/// The start of a Turn's final Message, at most
/// [`SidekickReport::EXCERPT_CHARS`] characters of it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FinalMessage {
    excerpt: String,
    truncated: bool,
    /// Where a reading of the Session finds the whole Message.
    item: EntryNumber,
}

/// The account of one thing that happened in a Session a Sidekick set to
/// work, as that Sidekick receives it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SidekickReport {
    /// The Session it is about, and the Server it lives on: this Sidekick's
    /// own, or a Remote.
    pub session: SessionReference,
    /// The Session's Title as it stood when the Report was raised.
    pub title: String,
    pub occasion: SidekickReportOccasion,
}

impl SidekickReport {
    /// The most characters of a final Message a Report carries: as much as a
    /// reading of a Session shows unless asked for more, since a Sidekick
    /// hears of many Sessions and pays for each Report it is given. The whole
    /// Message stays one `read_session` away.
    pub const EXCERPT_CHARS: usize = agent_reading::DEFAULT_MAX_CHARS;

    /// The Report that a Turn of `session` settled with `outcome` after
    /// `duration_ms`, excerpting `final_message` — the last Message its Agent
    /// wrote in the Turn, with the number a reading finds it by — to
    /// [`Self::EXCERPT_CHARS`], cut on a character boundary, and carrying
    /// `error` — what a failed Turn failed with — cut the same way.
    pub(crate) fn turn_settled(
        session: SessionReference,
        title: impl Into<String>,
        outcome: SidekickTurnOutcome,
        duration_ms: Option<u64>,
        error: Option<&str>,
        final_message: Option<(EntryNumber, &str)>,
    ) -> Self {
        let final_message = final_message.map(|(item, message)| {
            let (excerpt, truncated) = bounded(message, Self::EXCERPT_CHARS);
            FinalMessage {
                excerpt: excerpt.to_owned(),
                truncated,
                item,
            }
        });
        Self {
            session,
            title: title.into(),
            occasion: SidekickReportOccasion::TurnSettled {
                outcome,
                duration_ms,
                error: error.map(|error| bounded(error, Self::EXCERPT_CHARS).0.to_owned()),
                final_message,
            },
        }
    }

    /// The Report that `session` came to owe `intervention`: its own, or the
    /// one `subagent` — a Subagent's Session beneath it — owes.
    pub(crate) fn intervention_owed(
        session: SessionReference,
        title: impl Into<String>,
        intervention: SidekickIntervention,
        subagent: Option<SessionId>,
    ) -> Self {
        Self {
            session,
            title: title.into(),
            occasion: SidekickReportOccasion::InterventionOwed {
                intervention,
                subagent,
            },
        }
    }
}

/// The one text a Sidekick Report is delivered as, whichever harness carries
/// it: what it is, the Session by its Title — and its Remote, where it lives
/// on one — and by the id the Sidekick's Tools take, then what happened. A
/// settled Turn says how it settled and after how long, what it failed with
/// where it failed and the Transcript says why, and its final Message as far
/// as the excerpt reaches, saying where the rest is when it was cut. An owed
/// Intervention says which it is, whose it is where a Subagent owes it, and
/// which Tools read and answer it.
impl fmt::Display for SidekickReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let id = self.session.session_id;
        let session = format!(
            "the Session \"{}\" you set to work{}",
            agent_reading::one_line(&self.title),
            match &self.session.origin {
                Outlook::Local => String::new(),
                Outlook::Remote(name) => format!(" on the Remote \"{name}\""),
            }
        );
        formatter.write_str("Sidekick Report from Suru: ")?;
        match &self.occasion {
            SidekickReportOccasion::TurnSettled {
                outcome,
                duration_ms,
                error,
                final_message,
            } => {
                let settled = match outcome {
                    SidekickTurnOutcome::Completed => "completed",
                    SidekickTurnOutcome::Failed => "failed",
                    SidekickTurnOutcome::Interrupted => "was interrupted",
                };
                write!(formatter, "{session} has settled its Turn, which {settled}")?;
                if let Some(duration_ms) = duration_ms {
                    write!(formatter, " after {}", duration(*duration_ms))?;
                }
                write!(
                    formatter,
                    ". Its session_id is {id}, which read_session takes."
                )?;
                if let Some(error) = error {
                    write!(formatter, "\n\nIt failed with: {error}")?;
                }
                match final_message {
                    None => formatter.write_str("\n\nIts Agent wrote no final Message in it."),
                    Some(message) => {
                        write!(
                            formatter,
                            "\n\nIts Agent's final Message:\n\n{}",
                            message.excerpt
                        )?;
                        if message.truncated {
                            write!(
                                formatter,
                                "\n\n[Cut at {} characters: read_session with item \"{}\" \
                                 gives the whole Message.]",
                                Self::EXCERPT_CHARS,
                                message.item
                            )?;
                        }
                        Ok(())
                    }
                }
            }
            SidekickReportOccasion::InterventionOwed {
                intervention,
                subagent,
            } => {
                let (asks, tools) = match intervention {
                    SidekickIntervention::Questionnaire => (
                        "asks a Questionnaire, which waits on an Answer",
                        "read_session gives its Questions, and answer_questionnaire answers it",
                    ),
                    SidekickIntervention::Approval => (
                        "asks an Approval, which waits on the user's Decision",
                        "read_session says what it asks",
                    ),
                };
                match subagent {
                    None => write!(
                        formatter,
                        "{session} {asks}. Its session_id is {id}: {tools}."
                    ),
                    Some(subagent) => write!(
                        formatter,
                        "a Subagent of {session} {asks}. The Session's session_id is {id}, and \
                         the Subagent's is {subagent}: given the Subagent's, {tools}."
                    ),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(session_id: SessionId) -> SessionReference {
        SessionReference::new(Outlook::Local, session_id)
    }

    fn item(spelled: &str) -> EntryNumber {
        spelled.parse().expect("an entry number")
    }

    fn settled(
        outcome: SidekickTurnOutcome,
        error: Option<&str>,
        final_message: Option<&str>,
    ) -> SidekickReport {
        SidekickReport::turn_settled(
            local(SessionId::new()),
            "Fix the flaky test",
            outcome,
            Some(83_400),
            error,
            final_message.map(|message| (item("2.3"), message)),
        )
    }

    #[test]
    fn a_settled_turn_names_the_session_how_it_settled_and_its_final_message() {
        let report = settled(SidekickTurnOutcome::Completed, None, Some("All 214 pass."));
        assert_eq!(
            report.to_string(),
            format!(
                "Sidekick Report from Suru: the Session \"Fix the flaky test\" you set to work \
                 has settled its Turn, which completed after 1m 23s. Its session_id is {}, which \
                 read_session takes.\n\nIts Agent's final Message:\n\nAll 214 pass.",
                report.session.session_id
            )
        );
    }

    #[test]
    fn a_failed_turn_says_what_it_failed_with_and_when_nothing_was_written_or_timed() {
        let mut report = settled(
            SidekickTurnOutcome::Failed,
            Some("Provider execution failed: the connection closed."),
            None,
        );
        let SidekickReportOccasion::TurnSettled { duration_ms, .. } = &mut report.occasion else {
            unreachable!("a settled Turn's Report");
        };
        *duration_ms = None;
        assert_eq!(
            report.to_string(),
            format!(
                "Sidekick Report from Suru: the Session \"Fix the flaky test\" you set to work \
                 has settled its Turn, which failed. Its session_id is {}, which read_session \
                 takes.\n\nIt failed with: Provider execution failed: the connection \
                 closed.\n\nIts Agent wrote no final Message in it.",
                report.session.session_id
            )
        );
        assert!(
            settled(SidekickTurnOutcome::Interrupted, None, Some("Halfway."))
                .to_string()
                .contains("has settled its Turn, which was interrupted after 1m 23s.")
        );
    }

    #[test]
    fn a_long_final_message_is_cut_on_a_character_boundary_naming_the_item_that_reads_it_whole() {
        let message = "é".repeat(SidekickReport::EXCERPT_CHARS + 10);
        let report = settled(SidekickTurnOutcome::Completed, None, Some(&message));
        let rendered = report.to_string();
        let excerpt = rendered
            .split("\n\n")
            .nth(2)
            .expect("the excerpt follows the heading");
        assert_eq!(excerpt.chars().count(), SidekickReport::EXCERPT_CHARS);
        assert!(message.starts_with(excerpt));
        assert!(
            rendered.ends_with(
                "[Cut at 2000 characters: read_session with item \"2.3\" gives the whole \
                 Message.]"
            ),
            "the Sidekick is told where the rest is: {rendered}"
        );
        let whole = "a".repeat(SidekickReport::EXCERPT_CHARS);
        assert!(
            !settled(SidekickTurnOutcome::Completed, None, Some(&whole))
                .to_string()
                .contains("[Cut at"),
            "a Message exactly at the bound is not cut"
        );
    }

    #[test]
    fn a_long_error_is_cut_like_the_excerpt() {
        let error = "x".repeat(SidekickReport::EXCERPT_CHARS + 1);
        let report = settled(SidekickTurnOutcome::Failed, Some(&error), None);
        let SidekickReportOccasion::TurnSettled { error, .. } = &report.occasion else {
            unreachable!("a settled Turn's Report");
        };
        assert_eq!(
            error.as_deref().map(str::len),
            Some(SidekickReport::EXCERPT_CHARS)
        );
    }

    #[test]
    fn an_owed_intervention_says_which_and_the_tools_that_read_and_answer_it() {
        let session_id = SessionId::new();
        let questionnaire = SidekickReport::intervention_owed(
            local(session_id),
            "Fix the flaky test",
            SidekickIntervention::Questionnaire,
            None,
        );
        assert_eq!(
            questionnaire.to_string(),
            format!(
                "Sidekick Report from Suru: the Session \"Fix the flaky test\" you set to work \
                 asks a Questionnaire, which waits on an Answer. Its session_id is {session_id}: \
                 read_session gives its Questions, and answer_questionnaire answers it."
            )
        );
        let approval = SidekickReport::intervention_owed(
            local(session_id),
            "Fix the flaky test",
            SidekickIntervention::Approval,
            None,
        );
        assert_eq!(
            approval.to_string(),
            format!(
                "Sidekick Report from Suru: the Session \"Fix the flaky test\" you set to work \
                 asks an Approval, which waits on the user's Decision. Its session_id is \
                 {session_id}: read_session says what it asks."
            )
        );
    }

    #[test]
    fn a_subagents_intervention_names_the_subagents_session_beside_the_sessions_own() {
        let session_id = SessionId::new();
        let subagent = SessionId::new();
        let report = SidekickReport::intervention_owed(
            local(session_id),
            "Fix the flaky test",
            SidekickIntervention::Questionnaire,
            Some(subagent),
        );
        assert_eq!(
            report.to_string(),
            format!(
                "Sidekick Report from Suru: a Subagent of the Session \"Fix the flaky test\" you \
                 set to work asks a Questionnaire, which waits on an Answer. The Session's \
                 session_id is {session_id}, and the Subagent's is {subagent}: given the \
                 Subagent's, read_session gives its Questions, and answer_questionnaire answers \
                 it."
            )
        );
    }

    #[test]
    fn a_title_is_held_to_one_short_line_and_a_remotes_session_names_its_remote() {
        let report = SidekickReport::intervention_owed(
            SessionReference::new(Outlook::Remote("studio".to_owned()), SessionId::new()),
            format!("{}\nand more", "t".repeat(400)),
            SidekickIntervention::Approval,
            None,
        );
        let rendered = report.to_string();
        let title = rendered.split('"').nth(1).expect("the Title is quoted");
        assert!(
            title.ends_with('…') && title.chars().count() <= 160,
            "{title}"
        );
        assert!(
            rendered.contains("you set to work on the Remote \"studio\" asks an Approval"),
            "{rendered}"
        );
    }
}
