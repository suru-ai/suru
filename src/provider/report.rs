//! Subagent Reports (ADR 0035): the account Suru gives the delegating Agent
//! when a brokered Subagent's Turn settles, delivered as that Agent's own
//! input — at the head of a Turn's input beside a Prompt, alone to wake a
//! Continuation, or as a steer of a Turn still working — through the start
//! and steer every Provider already takes.
//!
//! The Session store builds a Report as the Subagent's row settles; it is
//! rendered here, once, into the words every harness sends, so an Agent reads
//! the same Report whichever Provider it runs on. A Report stands nowhere in
//! any Transcript: the settled row is the record.

use std::fmt;

use crate::protocol::SessionId;

/// How the stretch of work a Subagent Report tells of settled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubagentReportOutcome {
    /// The Subagent finished the work it was delegated.
    Completed,
    /// Its Turn failed: its Provider said so, its Provider was lost under it,
    /// or Suru stopped before it finished (ADR 0029).
    Failed,
    /// It was stopped on its own — by the user, or by an Agent's
    /// `stop_subagent` — since the Agent that delegated it planned on the
    /// result. One stopped by an interrupt of a Session above it reports
    /// nothing, the Agent that would hear of it having been interrupted too.
    Stopped,
}

/// The account of one brokered Subagent's settled stretch of work, as the
/// Agent that delegated it receives it: which Subagent, how it settled, how
/// long it worked, and a bounded excerpt of the final Message it wrote there —
/// the rest readable through the Broker's `read_subagent`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubagentReport {
    /// The Subagent's own Session, which is the id every Broker Tool takes.
    pub subagent: SessionId,
    /// The name its row carries, as its spawn gave it.
    pub name: String,
    pub outcome: SubagentReportOutcome,
    /// How long the stretch worked, as its settled row says; `None` where
    /// Suru never learned when it ended (ADR 0029).
    pub duration_ms: Option<u64>,
    /// The start of the final Message the Subagent wrote in the stretch, at
    /// most [`Self::EXCERPT_CHARS`] characters of it; `None` when it wrote
    /// none.
    pub excerpt: Option<String>,
    /// Whether the excerpt stops short of the whole Message.
    pub truncated: bool,
}

impl SubagentReport {
    /// The most characters of the final Message a Report carries. Enough for
    /// the answer most delegated work ends with, while a Subagent that writes
    /// a report of its own cannot flood the context of the Agent it reports
    /// to: the whole Message stays one `read_subagent` away.
    pub const EXCERPT_CHARS: usize = 4_000;

    /// The Report of a stretch that settled with `outcome` after
    /// `duration_ms`, excerpting `final_message` — the last Message the
    /// Subagent wrote there — to [`Self::EXCERPT_CHARS`], cut on a character
    /// boundary.
    pub fn new(
        subagent: SessionId,
        name: impl Into<String>,
        outcome: SubagentReportOutcome,
        duration_ms: Option<u64>,
        final_message: Option<&str>,
    ) -> Self {
        let (excerpt, truncated) = match final_message {
            None => (None, false),
            Some(message) => match message.char_indices().nth(Self::EXCERPT_CHARS) {
                None => (Some(message.to_owned()), false),
                Some((cut, _)) => (Some(message[..cut].to_owned()), true),
            },
        };
        Self {
            subagent,
            name: name.into(),
            outcome,
            duration_ms,
            excerpt,
            truncated,
        }
    }
}

/// The one text a Report is delivered as, whichever harness carries it: what
/// it is and from whom, the Subagent by name and by the id the Broker's Tools
/// take — to read the rest of what it wrote, and to send it more — how its
/// stretch settled and after how long, and its final Message as far as the
/// excerpt reaches, saying where the rest is when it was cut.
impl fmt::Display for SubagentReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let settled = match self.outcome {
            SubagentReportOutcome::Completed => "completed",
            SubagentReportOutcome::Failed => "failed",
            SubagentReportOutcome::Stopped => "was stopped",
        };
        write!(
            formatter,
            "Subagent Report from Suru: the Subagent \"{}\" you delegated to through the Broker {settled}",
            one_line(&self.name),
        )?;
        if let Some(duration_ms) = self.duration_ms {
            write!(formatter, " after {}", duration(duration_ms))?;
        }
        write!(
            formatter,
            ". Its session_id is {}, which read_subagent and send_to_subagent take.",
            self.subagent
        )?;
        match &self.excerpt {
            None => formatter.write_str("\n\nIt wrote no final Message."),
            Some(excerpt) => {
                write!(formatter, "\n\nIts final Message:\n\n{excerpt}")?;
                if self.truncated {
                    write!(
                        formatter,
                        "\n\n[Cut at {} characters: read_subagent gives the whole Message.]",
                        Self::EXCERPT_CHARS
                    )?;
                }
                Ok(())
            }
        }
    }
}

/// A name held to one line, since the Report's first line names the Subagent
/// in quotes.
fn one_line(name: &str) -> &str {
    name.trim().lines().next().unwrap_or_default().trim()
}

/// `duration_ms` the way a reader says it: tenths of a second under a minute,
/// then minutes and seconds, then hours and minutes.
fn duration(duration_ms: u64) -> String {
    let seconds = duration_ms / 1_000;
    match seconds {
        0..60 => format!("{}.{}s", seconds, (duration_ms % 1_000) / 100),
        60..3_600 => format!("{}m {}s", seconds / 60, seconds % 60),
        _ => format!("{}h {}m", seconds / 3_600, (seconds % 3_600) / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(outcome: SubagentReportOutcome, final_message: Option<&str>) -> SubagentReport {
        SubagentReport::new(
            SessionId::new(),
            "Researcher",
            outcome,
            Some(83_400),
            final_message,
        )
    }

    #[test]
    fn a_final_message_within_the_bound_is_excerpted_whole() {
        let report = report(SubagentReportOutcome::Completed, Some("Three seams."));
        assert_eq!(report.excerpt.as_deref(), Some("Three seams."));
        assert!(!report.truncated);
    }

    #[test]
    fn a_long_final_message_is_cut_on_a_character_boundary_and_marked_cut() {
        let message = "é".repeat(SubagentReport::EXCERPT_CHARS + 10);
        let report = report(SubagentReportOutcome::Completed, Some(&message));
        let excerpt = report.excerpt.as_deref().expect("an excerpt");
        assert_eq!(excerpt.chars().count(), SubagentReport::EXCERPT_CHARS);
        assert!(message.starts_with(excerpt));
        assert!(report.truncated);
        assert!(
            report
                .to_string()
                .ends_with("[Cut at 4000 characters: read_subagent gives the whole Message.]"),
            "the Agent is told where the rest is"
        );
    }

    #[test]
    fn a_message_exactly_at_the_bound_is_not_cut() {
        let message = "a".repeat(SubagentReport::EXCERPT_CHARS);
        let report = report(SubagentReportOutcome::Completed, Some(&message));
        assert_eq!(report.excerpt.as_deref(), Some(message.as_str()));
        assert!(!report.truncated);
    }

    #[test]
    fn a_report_names_the_subagent_its_id_how_it_settled_and_its_final_message() {
        let report = report(SubagentReportOutcome::Completed, Some("Three seams."));
        assert_eq!(
            report.to_string(),
            format!(
                "Subagent Report from Suru: the Subagent \"Researcher\" you delegated to through \
                 the Broker completed after 1m 23s. Its session_id is {}, which read_subagent and \
                 send_to_subagent take.\n\nIts final Message:\n\nThree seams.",
                report.subagent
            )
        );
    }

    #[test]
    fn a_report_says_a_failure_or_a_stop_and_when_nothing_was_written_or_timed() {
        let mut failed = report(SubagentReportOutcome::Failed, None);
        failed.duration_ms = None;
        assert_eq!(
            failed.to_string(),
            format!(
                "Subagent Report from Suru: the Subagent \"Researcher\" you delegated to through \
                 the Broker failed. Its session_id is {}, which read_subagent and send_to_subagent \
                 take.\n\nIt wrote no final Message.",
                failed.subagent
            )
        );
        let stopped = report(SubagentReportOutcome::Stopped, Some("Halfway."));
        assert!(
            stopped
                .to_string()
                .contains("through the Broker was stopped after 1m 23s.")
        );
    }

    #[test]
    fn durations_read_as_a_person_says_them() {
        assert_eq!(duration(420), "0.4s");
        assert_eq!(duration(42_345), "42.3s");
        assert_eq!(duration(60_000), "1m 0s");
        assert_eq!(duration(3_723_000), "1h 2m");
    }
}
