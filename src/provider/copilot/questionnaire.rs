//! Native ask_user callbacks remain live only inside their Provider connection.
use crate::{
    protocol::{
        Question, QuestionAnswer, QuestionChoice, Questionnaire, QuestionnaireId,
        QuestionnaireSubmission,
    },
    provider::{AttributedProviderEvent, ProviderError, ProviderEvent},
};
use github_copilot_sdk::{
    SessionId,
    handler::{UserInputHandler, UserInputResponse},
};
use std::{collections::HashMap, sync::Mutex};
use tokio::sync::{mpsc, oneshot};

pub(super) struct CopilotQuestionnaires {
    events: mpsc::UnboundedSender<Result<AttributedProviderEvent, ProviderError>>,
    pending: Mutex<HashMap<QuestionnaireId, oneshot::Sender<Option<UserInputResponse>>>>,
}

impl CopilotQuestionnaires {
    pub(super) fn new(
        events: mpsc::UnboundedSender<Result<AttributedProviderEvent, ProviderError>>,
    ) -> Self {
        Self {
            events,
            pending: Mutex::new(HashMap::new()),
        }
    }

    pub(super) fn submit(
        &self,
        id: QuestionnaireId,
        submission: QuestionnaireSubmission,
    ) -> Result<(), ProviderError> {
        let response = match submission {
            QuestionnaireSubmission::Decline => None,
            QuestionnaireSubmission::Answer { answer } => match answer.questions.as_slice() {
                [QuestionAnswer::Selected { choices }] if choices.len() == 1 => {
                    Some(UserInputResponse {
                        answer: choices[0].clone(),
                        was_freeform: false,
                    })
                }
                [QuestionAnswer::Freeform { text }] => Some(UserInputResponse {
                    answer: text.clone(),
                    was_freeform: true,
                }),
                _ => return Err(ProviderError::questionnaire_rejected("Copilot requires one answer")),
            },
        };
        self.pending
            .lock()
            .expect("Copilot Questionnaire lock is not poisoned")
            .remove(&id)
            .ok_or_else(|| ProviderError::new("Questionnaire is unavailable"))?
            .send(response)
            .map_err(|_| ProviderError::new("Questionnaire is unavailable"))
    }

    pub(super) fn cancel(&self) {
        self.pending
            .lock()
            .expect("Copilot Questionnaire lock is not poisoned")
            .clear();
    }
}

#[async_trait::async_trait]
impl UserInputHandler for CopilotQuestionnaires {
    async fn handle(
        &self,
        _session_id: SessionId,
        question: String,
        choices: Option<Vec<String>>,
        allow_freeform: Option<bool>,
    ) -> Option<UserInputResponse> {
        let id = QuestionnaireId::new();
        let (response, received) = oneshot::channel();
        self.pending
            .lock()
            .expect("Copilot Questionnaire lock is not poisoned")
            .insert(id, response);
        let _withdrawal = Withdrawal { owner: self, id };
        let questionnaire = Questionnaire {
            id,
            questions: vec![Question {
                id: "answer".into(),
                title: None,
                text: question,
                choices: choices
                    .unwrap_or_default()
                    .into_iter()
                    .map(|label| QuestionChoice {
                        id: label.clone(),
                        label,
                        description: None,
                        recommended: false,
                    })
                    .collect(),
                multiple: false,
                freeform: allow_freeform.unwrap_or(true),
                combine_freeform: false,
                secret: false,
                required: true,
            }],
        };
        if self
            .events
            .send(Ok(
                ProviderEvent::QuestionnaireRequested { questionnaire }.into()
            ))
            .is_err()
        {
            self.pending
                .lock()
                .expect("Copilot Questionnaire lock is not poisoned")
                .remove(&id);
            return None;
        }
        received.await.ok().flatten()
    }
}

struct Withdrawal<'a> {
    owner: &'a CopilotQuestionnaires,
    id: QuestionnaireId,
}
impl Drop for Withdrawal<'_> {
    fn drop(&mut self) {
        if self
            .owner
            .pending
            .lock()
            .expect("Copilot Questionnaire lock is not poisoned")
            .remove(&self.id)
            .is_some()
        {
            let _ =
                self.owner.events.send(Ok(
                    ProviderEvent::QuestionnaireWithdrawn { id: self.id }.into()
                ));
        }
    }
}
