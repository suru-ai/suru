//! Reports: the accounts Suru gives an Agent unasked, as that Agent's own
//! input — at the head of a Turn's input beside a Prompt, alone to wake a
//! Continuation, or as a steer of a Turn still working — through the start
//! and steer every Provider already takes (ADR 0035). There are two: the
//! Subagent Report a delegating Agent is given when a brokered Subagent's Turn
//! settles, and the Sidekick Report a Sidekick is given of a Session it set to
//! work (see [`SidekickReport`]).
//!
//! The Session store builds a Report where what it tells of happens; it is
//! rendered here, once, into the words every harness sends, so an Agent reads
//! the same Report whichever Provider it runs on. A Report stands nowhere in
//! any Transcript: what it tells of is recorded where it happened.

use std::fmt;

use crate::protocol::SessionId;

mod sidekick;

pub use sidekick::{
    FinalMessage, SidekickIntervention, SidekickReport, SidekickReportOccasion, SidekickTurnOutcome,
};

/// One account Suru gives an Agent as its own input, whichever it is: every
/// harness carries the one as it carries the other, in the words each
/// renders itself as.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Report {
    /// Of a brokered Subagent's settled stretch of work, to the Agent that
    /// delegated it.
    Subagent(SubagentReport),
    /// Of a Session a Sidekick set to work, to that Sidekick.
    Sidekick(SidekickReport),
}

impl From<SubagentReport> for Report {
    fn from(report: SubagentReport) -> Self {
        Self::Subagent(report)
    }
}

impl From<SidekickReport> for Report {
    fn from(report: SidekickReport) -> Self {
        Self::Sidekick(report)
    }
}

impl fmt::Display for Report {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Subagent(report) => report.fmt(formatter),
            Self::Sidekick(report) => report.fmt(formatter),
        }
    }
}

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
    /// What the stretch failed with, where it failed and the Subagent's
    /// Transcript says why: the text of the error row that settled it, cut
    /// like the excerpt. `None` for any other outcome, and for a failure
    /// nothing explained.
    pub error: Option<String>,
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
    /// boundary, and carrying `error` — what a failed stretch failed with —
    /// cut the same way.
    pub fn new(
        subagent: SessionId,
        name: impl Into<String>,
        outcome: SubagentReportOutcome,
        duration_ms: Option<u64>,
        final_message: Option<&str>,
        error: Option<&str>,
    ) -> Self {
        let (excerpt, truncated) =
            match final_message.map(|message| bounded(message, Self::EXCERPT_CHARS)) {
                None => (None, false),
                Some((excerpt, truncated)) => (Some(excerpt.to_owned()), truncated),
            };
        let error = error.map(|error| bounded(error, Self::EXCERPT_CHARS).0.to_owned());
        Self {
            subagent,
            name: name.into(),
            outcome,
            duration_ms,
            error,
            excerpt,
            truncated,
        }
    }
}

/// The one text a Report is delivered as, whichever harness carries it: what
/// it is and from whom, the Subagent by name and by the id the Broker's Tools
/// take — to read the rest of what it wrote, and to send it more — how its
/// stretch settled and after how long, what it failed with where it failed
/// and the Transcript says why, and its final Message as far as the excerpt
/// reaches, saying where the rest is when it was cut.
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
        if let Some(error) = &self.error {
            write!(formatter, "\n\nIt failed with: {error}")?;
        }
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

/// The start of `text`, at most `chars` characters of it, cut on a character
/// boundary, and whether that stops short of the whole: how every Report
/// bounds what it quotes.
fn bounded(text: &str, chars: usize) -> (&str, bool) {
    match text.char_indices().nth(chars) {
        None => (text, false),
        Some((cut, _)) => (&text[..cut], true),
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
            None,
        )
    }

    #[test]
    fn a_failed_report_says_what_the_stretch_failed_with_before_its_final_message() {
        let report = SubagentReport::new(
            SessionId::new(),
            "Researcher",
            SubagentReportOutcome::Failed,
            Some(453),
            None,
            Some("Provider execution failed: Copilot Model selection failed: RPC error -32603."),
        );
        assert_eq!(
            report.to_string(),
            format!(
                "Subagent Report from Suru: the Subagent \"Researcher\" you delegated to through \
                 the Broker failed after 0.4s. Its session_id is {}, which read_subagent and \
                 send_to_subagent take.\n\nIt failed with: Provider execution failed: Copilot \
                 Model selection failed: RPC error -32603.\n\nIt wrote no final Message.",
                report.subagent
            ),
            "the delegating Agent learns why, not only that, its Subagent failed"
        );
    }

    #[test]
    fn a_long_error_is_cut_like_the_excerpt() {
        let error = "x".repeat(SubagentReport::EXCERPT_CHARS + 1);
        let report = SubagentReport::new(
            SessionId::new(),
            "Researcher",
            SubagentReportOutcome::Failed,
            None,
            None,
            Some(&error),
        );
        assert_eq!(
            report.error.as_deref().map(str::len),
            Some(SubagentReport::EXCERPT_CHARS)
        );
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
