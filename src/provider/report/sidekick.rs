//! Sidekick Reports: the account Suru gives a Sidekick of a Session it set to
//! work — one it began, sent a Prompt, or answered — when that Session's Turn
//! settles or the Session comes to owe an Intervention (CONTEXT.md: Sidekick
//! Report). It is delivered as a Subagent Report is, and rendered here once,
//! in Suru's words, for an Agent to read: compact, naming the Session by its
//! Title and by the id the Sidekick's Tools take, saying what happened, and
//! naming the Tool that reads further or answers, without telling the
//! Sidekick what to do beyond that.
//!
//! A Report always names the top-level Session it is about, as the
//! Sidekick's tree lists it, and — where what happened, happened in a
//! Subagent's Session beneath it — that Subagent's Session too, which is the
//! one to read or answer; and, for a Session of a Remote, the Remote's name,
//! which every Tool taking that Session takes beside it.
//!
//! One Report is about no single Session: the one a Sidekick is given once,
//! when a Remote it is owed Reports from stops answering — or is paired no
//! longer — before they come, naming the Sessions it was waiting on there,
//! so it is not left waiting on them.

use std::fmt;

use super::{bounded, duration};
use crate::protocol::{SessionId, SessionReference};
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
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
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
    /// The Session came to owe an Intervention, which leaves its work
    /// waiting.
    InterventionOwed { intervention: SidekickIntervention },
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

/// What a Sidekick Report is about: the top-level Session, as the
/// Sidekick's tree lists it, and the Subagent's Session beneath it where what
/// the Report tells of happened in one — the one to read or answer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SidekickReportSubject {
    /// The top-level Session, and the Server it lives on: this Sidekick's
    /// own, or a Remote.
    pub session: SessionReference,
    /// That Session's Title as it stood when the Report was raised.
    pub title: String,
    /// The Subagent's Session beneath it where what the Report tells of
    /// happened, where it happened in one: the Turn of a Subagent the
    /// Sidekick answered, or an Intervention a Subagent came to owe.
    pub subagent: Option<SessionId>,
}

/// The account a Sidekick receives of work it set to work: of one thing that
/// happened in a Session, or that a Remote holding such Sessions stopped
/// answering first.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SidekickReport {
    /// Of one thing that happened in a Session the Sidekick set to work.
    Session {
        subject: SidekickReportSubject,
        occasion: SidekickReportOccasion,
    },
    /// That a Remote stopped answering, or its Pairing ended, while the
    /// Sidekick was owed Reports of Sessions there, none of which will come.
    OriginLost(SidekickOriginLost),
}

/// Why a Remote the Sidekick was owed Reports from will give none.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SidekickOriginLoss {
    /// It stopped answering while its Pairing stands: it is Unreachable.
    StoppedAnswering,
    /// Its Pairing ended, or its name was paired anew to another Server.
    Unpaired,
}

/// A Remote that will give the Reports a Sidekick was owed from it, and the
/// Sessions there they were owed of.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SidekickOriginLost {
    /// The Remote's name, the Origin the Sidekick's Tools take.
    pub remote: String,
    pub loss: SidekickOriginLoss,
    /// The top-level Sessions there the Sidekick was owed Reports of, each
    /// by its identity and the Title last known of it, at most
    /// [`SidekickReport::NAMED_SESSIONS`] of them.
    pub sessions: Vec<(SessionId, String)>,
    /// How many more there were, left unnamed.
    pub unnamed: usize,
}

impl SidekickReport {
    /// The most characters of a final Message a Report carries: as much as a
    /// reading of a Session shows unless asked for more, since a Sidekick
    /// hears of many Sessions and pays for each Report it is given. The whole
    /// Message stays one `read_session` away.
    pub const EXCERPT_CHARS: usize = agent_reading::DEFAULT_MAX_CHARS;

    /// The most Sessions the Report of a lost Remote names, however many
    /// were owed there: the rest are counted, and a listing of that Remote
    /// finds them.
    pub const NAMED_SESSIONS: usize = 16;

    /// The Report that a Turn of `subject` settled with `outcome` after
    /// `duration_ms`, excerpting `final_message` — the last Message its Agent
    /// wrote in the Turn, with the number a reading of that Session finds it
    /// by — to [`Self::EXCERPT_CHARS`], cut on a character boundary, and
    /// carrying `error` — what a failed Turn failed with — cut the same way.
    pub(crate) fn turn_settled(
        subject: SidekickReportSubject,
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
        Self::Session {
            subject,
            occasion: SidekickReportOccasion::TurnSettled {
                outcome,
                duration_ms,
                error: error.map(|error| bounded(error, Self::EXCERPT_CHARS).0.to_owned()),
                final_message,
            },
        }
    }

    /// The Report that `subject` came to owe `intervention`.
    pub(crate) fn intervention_owed(
        subject: SidekickReportSubject,
        intervention: SidekickIntervention,
    ) -> Self {
        Self::Session {
            subject,
            occasion: SidekickReportOccasion::InterventionOwed { intervention },
        }
    }

    /// The Report that the Remote `remote` will give none of the Reports
    /// owed of `sessions` there, for `loss`: each by its identity and Title,
    /// as many as [`Self::NAMED_SESSIONS`] named and the rest counted.
    pub(crate) fn origin_lost(
        remote: impl Into<String>,
        loss: SidekickOriginLoss,
        mut sessions: Vec<(SessionId, String)>,
    ) -> Self {
        let unnamed = sessions.len().saturating_sub(Self::NAMED_SESSIONS);
        sessions.truncate(Self::NAMED_SESSIONS);
        Self::OriginLost(SidekickOriginLost {
            remote: remote.into(),
            loss,
            sessions,
            unnamed,
        })
    }
}

/// The one text a Sidekick Report is delivered as, whichever harness carries
/// it: what it is, the Session by its Title — and its Remote, where it lives
/// on one — and by the ids the Sidekick's Tools take, and the Subagent's
/// Session beneath it where what happened, happened there, then what
/// happened. A settled Turn says how it settled and after how long, what it
/// failed with where it failed and the Transcript says why, and its final
/// Message as far as the excerpt reaches, saying where the rest is when it
/// was cut. An owed Intervention says which it is, and which Tools read and
/// answer it. A lost Remote says why, and names the Sessions there that will
/// report nothing more.
impl fmt::Display for SidekickReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Sidekick Report from Suru: ")?;
        match self {
            Self::Session { subject, occasion } => session_report(formatter, subject, occasion),
            Self::OriginLost(lost) => origin_lost(formatter, lost),
        }
    }
}

/// What happened in the Session `subject`, as [`SidekickReport`]'s text
/// says it after its opening words.
fn session_report(
    formatter: &mut fmt::Formatter<'_>,
    subject: &SidekickReportSubject,
    occasion: &SidekickReportOccasion,
) -> fmt::Result {
    let id = subject.session.session_id;
    let remote = subject.session.origin.remote_name();
    let session = format!(
        "the Session \"{}\" you set to work{}",
        agent_reading::one_line(&subject.title),
        remote.map_or_else(String::new, |name| format!(" on the Remote \"{name}\"")),
    );
    // A Remote's Session is reached by its Remote's name beside its id.
    let (session, ids, read_with) = match (subject.subagent, remote) {
        (None, None) => (
            session,
            format!("Its session_id is {id}"),
            "read_session with".to_owned(),
        ),
        (None, Some(name)) => (
            session,
            format!("Its session_id is {id} and its origin \"{name}\""),
            format!("read_session with origin \"{name}\" and"),
        ),
        (Some(subagent), None) => (
            format!("a Subagent of {session}"),
            format!("The Session's session_id is {id}, and the Subagent's is {subagent}"),
            "read_session with the Subagent's session_id and".to_owned(),
        ),
        (Some(subagent), Some(name)) => (
            format!("a Subagent of {session}"),
            format!(
                "The Session's session_id is {id}, and the Subagent's is {subagent}, each at \
                 origin \"{name}\""
            ),
            format!("read_session with the Subagent's session_id, origin \"{name}\", and"),
        ),
    };
    match occasion {
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
            write!(formatter, ". {ids}, which read_session takes.")?;
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
                            "\n\n[Cut at {} characters: {read_with} item \"{}\" gives the \
                             whole Message.]",
                            SidekickReport::EXCERPT_CHARS,
                            message.item
                        )?;
                    }
                    Ok(())
                }
            }
        }
        SidekickReportOccasion::InterventionOwed { intervention } => {
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
            let given = if subject.subagent.is_some() {
                "given the Subagent's, "
            } else {
                ""
            };
            write!(formatter, "{session} {asks}. {ids}: {given}{tools}.")
        }
    }
}

/// That the Remote `lost` names will report nothing more of the Sessions
/// there, as [`SidekickReport`]'s text says it after its opening words.
fn origin_lost(formatter: &mut fmt::Formatter<'_>, lost: &SidekickOriginLost) -> fmt::Result {
    let remote = &lost.remote;
    match lost.loss {
        SidekickOriginLoss::StoppedAnswering => {
            write!(formatter, "the Remote \"{remote}\" stopped answering")?;
        }
        SidekickOriginLoss::Unpaired => {
            write!(formatter, "the Pairing with the Remote \"{remote}\" ended")?;
        }
    }
    formatter.write_str(
        " while you were owed Reports of the Sessions you set to work there, so none will come \
         of them: ",
    )?;
    for (index, (session_id, title)) in lost.sessions.iter().enumerate() {
        if index > 0 {
            formatter.write_str(", ")?;
        }
        let title = agent_reading::one_line(title);
        if title.is_empty() {
            write!(formatter, "the Session {session_id}")?;
        } else {
            write!(formatter, "\"{title}\" (session_id {session_id})")?;
        }
    }
    if lost.unnamed > 0 {
        write!(formatter, ", and {} more", lost.unnamed)?;
    }
    match lost.loss {
        SidekickOriginLoss::StoppedAnswering => write!(
            formatter,
            ". read_session with origin \"{remote}\" reads them once it answers again, which \
             list_remotes tells."
        ),
        SidekickOriginLoss::Unpaired => {
            formatter.write_str(". Nothing of them can be read unless it is paired again.")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Outlook;

    /// The top-level Session a Report of one is about.
    fn session_of(report: &SidekickReport) -> SessionId {
        let SidekickReport::Session { subject, .. } = report else {
            unreachable!("a Report of a Session");
        };
        subject.session.session_id
    }

    fn local(session_id: SessionId) -> SessionReference {
        SessionReference::new(Outlook::Local, session_id)
    }

    /// The top-level Session `session_id` of this Server, titled `title`, or
    /// `subagent` beneath it.
    fn subject(
        session_id: SessionId,
        title: impl Into<String>,
        subagent: Option<SessionId>,
    ) -> SidekickReportSubject {
        SidekickReportSubject {
            session: local(session_id),
            title: title.into(),
            subagent,
        }
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
            subject(SessionId::new(), "Fix the flaky test", None),
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
                session_of(&report)
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
        let SidekickReport::Session {
            occasion: SidekickReportOccasion::TurnSettled { duration_ms, .. },
            ..
        } = &mut report
        else {
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
                session_of(&report)
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
        let SidekickReport::Session {
            occasion: SidekickReportOccasion::TurnSettled { error, .. },
            ..
        } = &report
        else {
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
            subject(session_id, "Fix the flaky test", None),
            SidekickIntervention::Questionnaire,
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
            subject(session_id, "Fix the flaky test", None),
            SidekickIntervention::Approval,
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
            subject(session_id, "Fix the flaky test", Some(subagent)),
            SidekickIntervention::Questionnaire,
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
    fn a_subagents_settled_turn_names_its_session_beside_the_sessions_own_and_reads_it_there() {
        let session_id = SessionId::new();
        let subagent = SessionId::new();
        let long = "a".repeat(SidekickReport::EXCERPT_CHARS + 1);
        let report = SidekickReport::turn_settled(
            subject(session_id, "Fix the flaky test", Some(subagent)),
            SidekickTurnOutcome::Completed,
            Some(4_200),
            None,
            Some((item("1.3"), &long)),
        );
        assert_eq!(
            report.to_string(),
            format!(
                "Sidekick Report from Suru: a Subagent of the Session \"Fix the flaky test\" you \
                 set to work has settled its Turn, which completed after 4.2s. The Session's \
                 session_id is {session_id}, and the Subagent's is {subagent}, which \
                 read_session takes.\n\nIts Agent's final Message:\n\n{}\n\n[Cut at 2000 \
                 characters: read_session with the Subagent's session_id and item \"1.3\" gives \
                 the whole Message.]",
                &long[..SidekickReport::EXCERPT_CHARS]
            )
        );
    }

    #[test]
    fn a_title_is_held_to_one_short_line_and_a_remotes_session_names_its_remote() {
        let report = SidekickReport::intervention_owed(
            SidekickReportSubject {
                session: SessionReference::new(
                    Outlook::Remote("studio".to_owned()),
                    SessionId::new(),
                ),
                title: format!("{}\nand more", "t".repeat(400)),
                subagent: None,
            },
            SidekickIntervention::Approval,
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

    /// The Session `session_id` of the Remote `studio`, titled as a test
    /// asks, or `subagent` beneath it.
    fn at_studio(session_id: SessionId, subagent: Option<SessionId>) -> SidekickReportSubject {
        SidekickReportSubject {
            session: SessionReference::new(Outlook::Remote("studio".to_owned()), session_id),
            title: "Fix the parser".to_owned(),
            subagent,
        }
    }

    #[test]
    fn a_remotes_session_is_named_by_its_origin_beside_its_id_wherever_a_tool_takes_it() {
        let session_id = SessionId::new();
        let long = "a".repeat(SidekickReport::EXCERPT_CHARS + 1);
        let settled = SidekickReport::turn_settled(
            at_studio(session_id, None),
            SidekickTurnOutcome::Completed,
            Some(4_200),
            None,
            Some((item("1.3"), &long)),
        );
        assert_eq!(
            settled.to_string(),
            format!(
                "Sidekick Report from Suru: the Session \"Fix the parser\" you set to work on the \
                 Remote \"studio\" has settled its Turn, which completed after 4.2s. Its \
                 session_id is {session_id} and its origin \"studio\", which read_session \
                 takes.\n\nIts Agent's final Message:\n\n{}\n\n[Cut at 2000 characters: \
                 read_session with origin \"studio\" and item \"1.3\" gives the whole Message.]",
                &long[..SidekickReport::EXCERPT_CHARS]
            )
        );
        let subagent = SessionId::new();
        let asked = SidekickReport::intervention_owed(
            at_studio(session_id, Some(subagent)),
            SidekickIntervention::Questionnaire,
        );
        assert_eq!(
            asked.to_string(),
            format!(
                "Sidekick Report from Suru: a Subagent of the Session \"Fix the parser\" you set \
                 to work on the Remote \"studio\" asks a Questionnaire, which waits on an Answer. \
                 The Session's session_id is {session_id}, and the Subagent's is {subagent}, each \
                 at origin \"studio\": given the Subagent's, read_session gives its Questions, and \
                 answer_questionnaire answers it."
            )
        );
    }

    #[test]
    fn a_lost_remote_names_why_and_each_session_owed_there_once() {
        let (parser, deps) = (SessionId::new(), SessionId::new());
        let lost = SidekickReport::origin_lost(
            "studio",
            SidekickOriginLoss::StoppedAnswering,
            vec![
                (parser, "Fix the parser".to_owned()),
                (deps, format!("Bump deps\n{}", "and more")),
            ],
        );
        assert_eq!(
            lost.to_string(),
            format!(
                "Sidekick Report from Suru: the Remote \"studio\" stopped answering while you \
                 were owed Reports of the Sessions you set to work there, so none will come of \
                 them: \"Fix the parser\" (session_id {parser}), \"Bump deps…\" (session_id \
                 {deps}). read_session with origin \"studio\" reads them once it answers again, \
                 which list_remotes tells."
            )
        );
        let unpaired = SidekickReport::origin_lost(
            "studio",
            SidekickOriginLoss::Unpaired,
            vec![(parser, String::new())],
        );
        assert_eq!(
            unpaired.to_string(),
            format!(
                "Sidekick Report from Suru: the Pairing with the Remote \"studio\" ended while you \
                 were owed Reports of the Sessions you set to work there, so none will come of \
                 them: the Session {parser}. Nothing of them can be read unless it is paired \
                 again."
            )
        );
    }

    #[test]
    fn a_lost_remote_names_so_many_sessions_and_counts_the_rest() {
        let owed = (0..SidekickReport::NAMED_SESSIONS + 3)
            .map(|index| (SessionId::new(), format!("Session {index}")))
            .collect::<Vec<_>>();
        let rendered = SidekickReport::origin_lost(
            "studio",
            SidekickOriginLoss::StoppedAnswering,
            owed.clone(),
        )
        .to_string();
        assert_eq!(
            rendered.matches("(session_id ").count(),
            SidekickReport::NAMED_SESSIONS
        );
        assert!(
            rendered.contains(&format!(
                "\"Session {}\" (session_id {}), and 3 more. ",
                SidekickReport::NAMED_SESSIONS - 1,
                owed[SidekickReport::NAMED_SESSIONS - 1].0
            )),
            "{rendered}"
        );
    }
}
