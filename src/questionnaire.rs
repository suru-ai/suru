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
    Answered,
    Declined,
    Withdrawn,
    TurnEnded,
    Unavailable,
    DeliveryUncertain,
}

impl Questionnaire {
    pub fn validate(&self, answer: &Answer) -> Result<(), String> {
        if answer.questions.len() != self.questions.len() {
            return Err("Answer every Question before review".into());
        }
        for (index, (question, answer)) in self.questions.iter().zip(&answer.questions).enumerate()
        {
            let valid = question.accepts(answer);
            if !valid {
                return Err(format!(
                    "Question {} requires a supported answer",
                    index + 1
                ));
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
        let selected = |choices: &[String]| {
            !choices.is_empty()
                && (self.multiple || choices.len() == 1)
                && choices
                    .iter()
                    .all(|id| self.choices.iter().any(|choice| &choice.id == id))
                && choices
                    .iter()
                    .enumerate()
                    .all(|(i, id)| !choices[..i].contains(id))
        };
        match answer {
            QuestionAnswer::Selected { choices } => selected(choices),
            QuestionAnswer::SelectedWithFreeform { choices, text } => {
                self.combine_freeform
                    && self.freeform
                    && selected(choices)
                    && !text.trim().is_empty()
            }
            QuestionAnswer::Freeform { text } => self.freeform && !text.trim().is_empty(),
            QuestionAnswer::Omitted => !self.required,
            QuestionAnswer::SecretAnswered => false,
        }
    }
}
