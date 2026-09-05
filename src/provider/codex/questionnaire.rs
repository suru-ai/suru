//! Correlated native user-input callbacks and secret-safe incoming content.
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
};

use super::{
    codex_error,
    wire::{RequestId, UserInputAnswer, UserInputParams, UserInputResponse},
};
use crate::{
    protocol::{
        Question, QuestionAnswer, QuestionChoice, Questionnaire, QuestionnaireId,
        QuestionnaireSubmission,
    },
    provider::ProviderError,
};

#[derive(Clone, Default)]
pub(super) struct CodexQuestionnaires(Arc<Mutex<State>>);
#[derive(Default)]
struct State {
    pending: HashMap<QuestionnaireId, NativeRequest>,
    // Retained only for this connection, so later native errors cannot echo an Answer.
    redactor: super::redaction::SecretRedactor,
}
struct NativeRequest {
    id: RequestId,
    params: UserInputParams,
    questionnaire: Questionnaire,
}

impl CodexQuestionnaires {
    pub(super) fn register(
        &self,
        id: RequestId,
        params: UserInputParams,
    ) -> Result<Questionnaire, ProviderError> {
        let mut questionnaire = normalize(&params)?;
        let mut state = self
            .0
            .lock()
            .expect("Codex questionnaire lock is not poisoned");
        for question in &mut questionnaire.questions {
            question.text = state.redactor.text(&question.text);
            question.title = question
                .title
                .as_ref()
                .map(|text| state.redactor.text(text));
            for choice in &mut question.choices {
                choice.label = state.redactor.text(&choice.label);
                choice.description = choice
                    .description
                    .as_ref()
                    .map(|text| state.redactor.text(text));
            }
        }
        if state.pending.values().any(|request| request.id == id) {
            return Err(codex_error(
                "Codex reused a live user-input request identity",
            ));
        }
        state.pending.insert(
            questionnaire.id,
            NativeRequest {
                id,
                params,
                questionnaire: questionnaire.clone(),
            },
        );
        Ok(questionnaire)
    }

    pub(super) fn take_response(
        &self,
        id: QuestionnaireId,
        submission: QuestionnaireSubmission,
    ) -> Result<(RequestId, UserInputResponse), ProviderError> {
        let mut state = self
            .0
            .lock()
            .expect("Codex questionnaire lock is not poisoned");
        let request = state
            .pending
            .get(&id)
            .ok_or_else(|| codex_error("Codex Questionnaire is unavailable"))?;
        let mut answers = BTreeMap::new();
        let mut secrets = Vec::new();
        if let QuestionnaireSubmission::Answer { answer } = submission {
            request
                .questionnaire
                .validate(&answer)
                .map_err(ProviderError::questionnaire_rejected)?;
            for ((question, native), answer) in request
                .questionnaire
                .questions
                .iter()
                .zip(&request.params.questions)
                .zip(answer.questions)
            {
                let selected = |choices: Vec<String>| {
                    choices
                        .into_iter()
                        .map(|id| {
                            if id == "other" {
                                "None of the above".into()
                            } else {
                                native
                                    .options
                                    .as_ref()
                                    .expect("selected native choices exist")
                                    [id.parse::<usize>().expect("validated choice index")]
                                .label
                                .clone()
                            }
                        })
                        .collect::<Vec<_>>()
                };
                let values = match answer {
                    QuestionAnswer::Selected { choices } => selected(choices),
                    QuestionAnswer::SelectedWithFreeform { choices, text } => {
                        let mut values = selected(choices);
                        if question.secret {
                            secrets.push(text.clone());
                            secrets.push(text.trim().to_owned());
                        }
                        values.push(format!("user_note: {}", text.trim()));
                        values
                    }
                    QuestionAnswer::Freeform { text } => {
                        if question.secret {
                            secrets.push(text.clone());
                            secrets.push(text.trim().to_owned());
                        }
                        vec![format!("user_note: {}", text.trim())]
                    }
                    QuestionAnswer::Omitted => vec![],
                    QuestionAnswer::SecretAnswered => {
                        unreachable!("validated Answers contain no history placeholders")
                    }
                };
                if question.secret {
                    secrets.extend(values.clone());
                }
                answers.insert(question.id.clone(), UserInputAnswer { answers: values });
            }
        }
        state.redactor.remember(secrets);
        let request = state
            .pending
            .remove(&id)
            .expect("validated request remains live");
        Ok((request.id, UserInputResponse { answers }))
    }

    pub(super) fn resolve(
        &self,
        thread_id: &str,
        request_id: &RequestId,
    ) -> Option<QuestionnaireId> {
        let mut state = self
            .0
            .lock()
            .expect("Codex questionnaire lock is not poisoned");
        let id = state.pending.iter().find_map(|(id, request)| {
            (request.params.thread_id == thread_id && &request.id == request_id).then_some(*id)
        })?;
        state.pending.remove(&id);
        Some(id)
    }

    pub(super) fn end_turn(&self, thread_id: &str, turn_id: &str) -> Vec<QuestionnaireId> {
        self.remove_where(|request| {
            request.params.thread_id == thread_id && request.params.turn_id == turn_id
        })
    }
    pub(super) fn end_thread(&self, thread_id: &str) {
        self.remove_where(|request| request.params.thread_id == thread_id);
    }
    pub(super) fn clear(&self) {
        self.remove_where(|_| true);
    }
    fn remove_where(&self, predicate: impl Fn(&NativeRequest) -> bool) -> Vec<QuestionnaireId> {
        let mut state = self
            .0
            .lock()
            .expect("Codex questionnaire lock is not poisoned");
        let mut removed = Vec::new();
        state.pending.retain(|id, request| {
            if predicate(request) {
                removed.push(*id);
                false
            } else {
                true
            }
        });
        removed
    }

    pub(super) fn redact_text(&self, text: &str) -> String {
        self.0
            .lock()
            .expect("Codex questionnaire lock is not poisoned")
            .redactor
            .text(text)
    }
    pub(super) fn redact_notification(
        &self,
        event: super::wire::NativeNotification,
    ) -> Vec<super::wire::NativeNotification> {
        self.0
            .lock()
            .expect("Codex questionnaire lock is not poisoned")
            .redactor
            .notification(event)
    }
}

fn normalize(params: &UserInputParams) -> Result<Questionnaire, ProviderError> {
    if params.item_id.is_empty()
        || params.questions.is_empty()
        || params
            .questions
            .iter()
            .enumerate()
            .any(|(index, question)| {
                question.id.is_empty()
                    || params.questions[..index]
                        .iter()
                        .any(|previous| previous.id == question.id)
            })
    {
        return Err(codex_error(
            "Codex user-input request has missing or duplicate Questions",
        ));
    }
    let questions = params
        .questions
        .iter()
        .map(|question| {
            let mut choices = question
                .options
                .as_deref()
                .unwrap_or_default()
                .iter()
                .enumerate()
                .map(|(index, option)| QuestionChoice {
                    id: index.to_string(),
                    label: option.label.clone(),
                    description: Some(option.description.clone()),
                    recommended: option.label.ends_with("(Recommended)"),
                })
                .collect::<Vec<_>>();
            if question.is_other && !choices.is_empty() {
                choices.push(QuestionChoice {
                    id: "other".into(),
                    label: "None of the above".into(),
                    description: None,
                    recommended: false,
                });
            }
            Question {
                id: question.id.clone(),
                title: Some(question.header.clone()),
                text: question.question.clone(),
                choices,
                multiple: false,
                freeform: true,
                combine_freeform: true,
                secret: question.is_secret,
                required: false,
            }
        })
        .collect();
    Ok(Questionnaire {
        id: QuestionnaireId::new(),
        questions,
    })
}
