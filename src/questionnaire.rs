//! Provider-neutral structured user input. Transport callbacks never enter this model.
use crate::protocol::QuestionnaireId;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Questionnaire {
    pub id: QuestionnaireId,
    pub questions: Vec<Question>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    pub id: String,
    pub title: Option<String>,
    pub text: String,
    pub choices: Vec<QuestionChoice>,
    pub multiple: bool,
    pub freeform: bool,
    /// Whether selected choices may be accompanied by free text in one answer.
    pub combine_freeform: bool,
    pub secret: bool,
    pub required: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionChoice {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub recommended: bool,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum QuestionAnswer {
    Selected {
        choices: Vec<String>,
    },
    SelectedWithFreeform {
        choices: Vec<String>,
        text: String,
    },
    Freeform {
        text: String,
    },
    Omitted,
    /// Durable placeholder only; never a valid submission.
    SecretAnswered,
}

// Submitted input must never appear in diagnostic formatting.
impl std::fmt::Debug for QuestionAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("QuestionAnswer(<redacted>)")
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Answer {
    pub questions: Vec<QuestionAnswer>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum QuestionnaireSubmission {
    Answer { answer: Answer },
    Decline,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionnaireOutcome {
    Pending,
    /// A Client won arbitration; Provider delivery is still in progress.
    Submitting,
    /// Provider rejected delivery without consuming the live request; explicit retry is safe.
    SubmissionRejected,
    Answered,
    Declined,
    Withdrawn,
    TurnEnded,
    Unavailable,
    DeliveryUncertain,
}

impl QuestionnaireOutcome {
    pub fn is_answerable(self) -> bool {
        matches!(self, Self::Pending | Self::SubmissionRejected)
    }

    pub fn is_live(self) -> bool {
        self.is_answerable() || self == Self::Submitting
    }
}

impl Questionnaire {
    pub fn validate(&self, answer: &Answer) -> Result<(), String> {
        self.check(answer).map_err(|mismatch| mismatch.to_string())
    }

    /// Whether `answer` fits the Questionnaire — one Answer for each Question,
    /// in their order, each one its Question accepts — and, where it does
    /// not, the first way it falls short.
    pub fn check(&self, answer: &Answer) -> Result<(), AnswerMismatch> {
        if answer.questions.len() != self.questions.len() {
            return Err(AnswerMismatch::Count {
                asked: self.questions.len(),
                given: answer.questions.len(),
            });
        }
        for (index, (question, given)) in self.questions.iter().zip(&answer.questions).enumerate() {
            if let Some(unaccepted) = question.unaccepted(given) {
                return Err(AnswerMismatch::Question {
                    number: index + 1,
                    id: question.id.clone(),
                    unaccepted,
                });
            }
        }
        Ok(())
    }

    pub fn history_answer(&self, answer: &Answer) -> Answer {
        Answer {
            questions: self
                .questions
                .iter()
                .zip(&answer.questions)
                .map(|(question, value)| {
                    if question.secret && !matches!(value, QuestionAnswer::Omitted) {
                        QuestionAnswer::SecretAnswered
                    } else {
                        value.clone()
                    }
                })
                .collect(),
        }
    }
}

impl Question {
    pub fn accepts(&self, answer: &QuestionAnswer) -> bool {
        self.unaccepted(answer).is_none()
    }

    /// Why the Question does not accept `answer`, or `None` where it does:
    /// choices only where it offers them, one unless it takes `multiple`, each
    /// once; free text only where it takes it, with something in it; both at
    /// once only where it takes them together; and nothing only where it is
    /// not required.
    pub fn unaccepted(&self, answer: &QuestionAnswer) -> Option<Unaccepted> {
        match answer {
            QuestionAnswer::Selected { choices } => self.unchosen(choices),
            QuestionAnswer::SelectedWithFreeform { choices, text } => {
                if !(self.combine_freeform && self.freeform) {
                    Some(Unaccepted::NotTogether)
                } else if let Some(unchosen) = self.unchosen(choices) {
                    Some(unchosen)
                } else if text.trim().is_empty() {
                    Some(Unaccepted::EmptyText)
                } else {
                    None
                }
            }
            QuestionAnswer::Freeform { text } => {
                if !self.freeform {
                    Some(Unaccepted::NoFreeform)
                } else if text.trim().is_empty() {
                    Some(Unaccepted::EmptyText)
                } else {
                    None
                }
            }
            QuestionAnswer::Omitted => self.required.then_some(Unaccepted::Required),
            QuestionAnswer::SecretAnswered => Some(Unaccepted::Placeholder),
        }
    }

    /// Why `choices` is no choice the Question accepts, or `None` where it is.
    fn unchosen(&self, choices: &[String]) -> Option<Unaccepted> {
        if choices.is_empty() {
            return Some(Unaccepted::NoChoice);
        }
        if !self.multiple && choices.len() > 1 {
            return Some(Unaccepted::OneChoice {
                given: choices.len(),
            });
        }
        if let Some(choice) = choices
            .iter()
            .find(|id| !self.choices.iter().any(|choice| &choice.id == *id))
        {
            return Some(Unaccepted::UnofferedChoice {
                choice: choice.clone(),
                offered: self
                    .choices
                    .iter()
                    .map(|choice| choice.id.clone())
                    .collect(),
            });
        }
        choices
            .iter()
            .enumerate()
            .find(|(index, id)| choices[..*index].contains(id))
            .map(|(_, id)| Unaccepted::RepeatedChoice(id.clone()))
    }
}

/// Why an Answer does not fit the Questionnaire it was given to, in words a
/// Client's reader and a Sidekick are told alike.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AnswerMismatch {
    /// It gives `given` Answers to a Questionnaire asking `asked` Questions.
    Count { asked: usize, given: usize },
    /// The Question numbered `number`, from 1, and identified by `id`, does
    /// not accept the Answer given it.
    Question {
        number: usize,
        id: String,
        unaccepted: Unaccepted,
    },
}

/// Why a Question does not accept the Answer given it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Unaccepted {
    /// It is required, and was left unanswered.
    Required,
    /// It was given an empty list of choices.
    NoChoice,
    /// It takes one choice, and was given `given`.
    OneChoice { given: usize },
    /// It was given `choice`, which it does not offer; it offers `offered`.
    UnofferedChoice {
        choice: String,
        offered: Vec<String>,
    },
    /// It was given the same choice more than once.
    RepeatedChoice(String),
    /// It was given free text, which it does not take.
    NoFreeform,
    /// It was given choices with free text beside them, which it does not take
    /// together.
    NotTogether,
    /// It was given free text with nothing in it.
    EmptyText,
    /// It was given the placeholder history keeps for a secret Answer.
    Placeholder,
}

impl std::fmt::Display for AnswerMismatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (number, id, unaccepted) = match self {
            Self::Count { asked, given } => {
                return write!(
                    formatter,
                    "The Questionnaire asks {asked} {} and takes one Answer for each, in their \
                     order, but {given} {} given.",
                    if *asked == 1 { "Question" } else { "Questions" },
                    if *given == 1 { "was" } else { "were" },
                );
            }
            Self::Question {
                number,
                id,
                unaccepted,
            } => (number, id, unaccepted),
        };
        let question = format!("Question {number} ({id:?})");
        match unaccepted {
            Unaccepted::Required => write!(
                formatter,
                "{question} is required, so it cannot be left unanswered."
            ),
            Unaccepted::NoChoice => write!(formatter, "{question} was given no choice."),
            Unaccepted::OneChoice { given } => write!(
                formatter,
                "{question} takes one choice, but {given} were given."
            ),
            Unaccepted::UnofferedChoice { choice, offered } if offered.is_empty() => write!(
                formatter,
                "{question} offers no choices, so it cannot be given {choice:?}."
            ),
            Unaccepted::UnofferedChoice { choice, offered } => {
                let quoted = offered
                    .iter()
                    .map(|offered| format!("{offered:?}"))
                    .collect::<Vec<_>>();
                let listed = match quoted.as_slice() {
                    [only] => only.clone(),
                    [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
                    [] => unreachable!("an empty offer is said above"),
                };
                write!(
                    formatter,
                    "{question} offers no choice {choice:?}; its {} {listed}.",
                    if offered.len() == 1 {
                        "one choice is"
                    } else {
                        "choices are"
                    }
                )
            }
            Unaccepted::RepeatedChoice(choice) => write!(
                formatter,
                "{question} was given the choice {choice:?} more than once."
            ),
            Unaccepted::NoFreeform => write!(formatter, "{question} takes no free text."),
            Unaccepted::NotTogether => write!(
                formatter,
                "{question} takes its choices or free text, but not both together."
            ),
            Unaccepted::EmptyText => write!(
                formatter,
                "{question} was given free text with nothing in it."
            ),
            Unaccepted::Placeholder => write!(
                formatter,
                "{question} was given the placeholder Suru keeps for a secret Answer, which is \
                 no Answer."
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(id: &str) -> QuestionChoice {
        QuestionChoice {
            id: id.to_owned(),
            label: id.to_owned(),
            description: None,
            recommended: false,
        }
    }

    /// One of two machines, with a note beside it where wanted; required.
    fn machine() -> Question {
        Question {
            id: "machine".to_owned(),
            title: None,
            text: "Where should the tests run?".to_owned(),
            choices: vec![choice("staging"), choice("local")],
            multiple: false,
            freeform: true,
            combine_freeform: true,
            secret: false,
            required: true,
        }
    }

    fn selected(choices: &[&str]) -> QuestionAnswer {
        QuestionAnswer::Selected {
            choices: choices.iter().map(|id| (*id).to_owned()).collect(),
        }
    }

    fn freeform(text: &str) -> QuestionAnswer {
        QuestionAnswer::Freeform {
            text: text.to_owned(),
        }
    }

    #[test]
    fn a_question_says_why_it_does_not_accept_an_answer() {
        let question = machine();
        let only_choices = Question {
            freeform: false,
            combine_freeform: false,
            ..machine()
        };
        let optional = Question {
            required: false,
            ..machine()
        };
        for (question, answer, unaccepted) in [
            (&question, selected(&["staging"]), None),
            (&question, freeform("after lunch"), None),
            (
                &question,
                QuestionAnswer::SelectedWithFreeform {
                    choices: vec!["local".to_owned()],
                    text: "after lunch".to_owned(),
                },
                None,
            ),
            (&optional, QuestionAnswer::Omitted, None),
            (
                &question,
                QuestionAnswer::Omitted,
                Some(Unaccepted::Required),
            ),
            (&question, selected(&[]), Some(Unaccepted::NoChoice)),
            (
                &question,
                selected(&["staging", "local"]),
                Some(Unaccepted::OneChoice { given: 2 }),
            ),
            (
                &question,
                selected(&["remote"]),
                Some(Unaccepted::UnofferedChoice {
                    choice: "remote".to_owned(),
                    offered: vec!["staging".to_owned(), "local".to_owned()],
                }),
            ),
            (
                &Question {
                    multiple: true,
                    ..machine()
                },
                selected(&["local", "local"]),
                Some(Unaccepted::RepeatedChoice("local".to_owned())),
            ),
            (
                &only_choices,
                freeform("after lunch"),
                Some(Unaccepted::NoFreeform),
            ),
            (
                &Question {
                    combine_freeform: false,
                    ..machine()
                },
                QuestionAnswer::SelectedWithFreeform {
                    choices: vec!["local".to_owned()],
                    text: "after lunch".to_owned(),
                },
                Some(Unaccepted::NotTogether),
            ),
            (&question, freeform("  "), Some(Unaccepted::EmptyText)),
            (
                &question,
                QuestionAnswer::SecretAnswered,
                Some(Unaccepted::Placeholder),
            ),
        ] {
            assert_eq!(question.unaccepted(&answer), unaccepted);
            assert_eq!(question.accepts(&answer), unaccepted.is_none());
        }
    }

    #[test]
    fn a_mismatch_is_said_naming_the_question_and_what_it_takes() {
        let questionnaire = Questionnaire {
            id: QuestionnaireId::new(),
            questions: vec![
                machine(),
                Question {
                    id: "notes".to_owned(),
                    choices: Vec::new(),
                    required: false,
                    ..machine()
                },
            ],
        };
        let said = |questions: Vec<QuestionAnswer>| {
            questionnaire
                .check(&Answer { questions })
                .expect_err("the Answer does not fit")
                .to_string()
        };
        assert_eq!(
            said(vec![selected(&["staging"])]),
            "The Questionnaire asks 2 Questions and takes one Answer for each, in their order, \
             but 1 was given."
        );
        assert_eq!(
            said(vec![selected(&["remote"]), QuestionAnswer::Omitted]),
            "Question 1 (\"machine\") offers no choice \"remote\"; its choices are \"staging\" \
             and \"local\"."
        );
        assert_eq!(
            said(vec![selected(&["staging"]), selected(&["staging"])]),
            "Question 2 (\"notes\") offers no choices, so it cannot be given \"staging\"."
        );
        assert_eq!(
            said(vec![QuestionAnswer::Omitted, QuestionAnswer::Omitted]),
            "Question 1 (\"machine\") is required, so it cannot be left unanswered."
        );
        assert_eq!(
            questionnaire.check(&Answer {
                questions: vec![selected(&["local"]), QuestionAnswer::Omitted],
            }),
            Ok(())
        );
    }
}
