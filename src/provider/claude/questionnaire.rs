//! Correlated Claude can_use_tool requests. Only live native input is retained here;
//! the public Questionnaire carries the Questions, never the native transport.
//!
//! The callback envelope follows the Agent SDK's `_internal/query.py`; the
//! `questions`/`answers` shape follows Claude's user-input contract:
//! https://code.claude.com/docs/en/agent-sdk/user-input
//! Selected labels and additional custom text use its comma-separated answer
//! encoding. The untouched native input travels back beside those answers.
//! Child callbacks use the SDK control contract's tool_use_id and agent_id:
//! https://github.com/anthropics/claude-agent-sdk-python/blob/main/src/claude_agent_sdk/types.py
use super::{claude_error, transport::StreamJsonTransport};
use crate::{
    protocol::{
        Question, QuestionAnswer, QuestionChoice, Questionnaire, QuestionnaireId,
        QuestionnaireSubmission,
    },
    provider::{AttributedProviderEvent, ProviderError, ProviderEvent, ProviderEventAttribution},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Mutex};

#[derive(Default)]
pub(super) struct ClaudeQuestionnaires {
    pending: Mutex<HashMap<QuestionnaireId, NativeQuestionnaire>>,
    transport: Mutex<Option<StreamJsonTransport>>,
}
struct NativeQuestionnaire {
    attribution: ProviderEventAttribution,
    request_id: String,
    input: Value,
    questionnaire: Questionnaire,
}
#[derive(Deserialize)]
struct NativeQuestion {
    question: String,
    header: Option<String>,
    options: Vec<NativeChoice>,
    #[serde(rename = "multiSelect", default)]
    multiple: bool,
}
#[derive(Deserialize)]
struct NativeChoice {
    label: String,
    description: Option<String>,
}

impl ClaudeQuestionnaires {
    pub(super) fn connect(&self, transport: StreamJsonTransport) {
        self.clear();
        *self
            .transport
            .lock()
            .expect("Claude Questionnaire transport lock is not poisoned") = Some(transport);
    }
    pub(super) fn clear(&self) {
        self.pending
            .lock()
            .expect("Claude Questionnaire lock is not poisoned")
            .clear();
    }
    pub(super) fn settle(&self, attribution: &ProviderEventAttribution) {
        self.pending
            .lock()
            .expect("Claude Questionnaire lock is not poisoned")
            .retain(|_, request| &request.attribution != attribution);
    }

    pub(super) async fn receive(
        &self,
        message: &Value,
        attribution: Option<ProviderEventAttribution>,
    ) -> Result<Option<Vec<AttributedProviderEvent>>, ProviderError> {
        match message.get("type").and_then(Value::as_str) {
            Some("control_cancel_request") => {
                let Some(request_id) = message.get("request_id").and_then(Value::as_str) else {
                    return Ok(Some(vec![]));
                };
                let mut pending = self
                    .pending
                    .lock()
                    .expect("Claude Questionnaire lock is not poisoned");
                let id = pending
                    .iter()
                    .find_map(|(id, q)| (q.request_id == request_id).then_some(*id));
                Ok(Some(
                    id.into_iter()
                        .map(|id| {
                            let native = pending.remove(&id).expect("pending correlation exists");
                            AttributedProviderEvent {
                                attribution: native.attribution,
                                event: ProviderEvent::QuestionnaireWithdrawn { id },
                            }
                        })
                        .collect(),
                ))
            }
            Some("control_request") => {
                let request_id = message
                    .get("request_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        claude_error("Claude question control request has no identity")
                    })?;
                let request = &message["request"];
                if request["subtype"] != "can_use_tool" {
                    return Ok(None);
                }
                let input = request["input"].clone();
                // The callback is also Claude's permission seam. Other tools retain
                // Suru's established full-auto posture, without becoming Questions.
                if request["tool_name"] != "AskUserQuestion" {
                    self.respond(
                        request_id,
                        json!({"behavior":"allow", "updatedInput":input}),
                    )
                    .await?;
                    return Ok(Some(vec![]));
                }
                let Some(attribution) = attribution else {
                    self.respond(request_id, json!({"behavior":"deny", "message":"The owning Subagent could not be identified", "interrupt":false})).await?;
                    return Ok(Some(vec![]));
                };
                let native: Vec<NativeQuestion> =
                    serde_json::from_value(input["questions"].clone()).map_err(|_| {
                        claude_error("Claude sent malformed Questionnaire Questions")
                    })?;
                if native.is_empty() {
                    return Err(claude_error("Claude sent an empty Questionnaire"));
                }
                let mut questions = Vec::new();
                for question in native {
                    // Native answers are keyed by text; duplicates cannot carry
                    // two independent answers and must never be silently collapsed.
                    if question.question.trim().is_empty()
                        || questions
                            .iter()
                            .any(|q: &Question| q.text == question.question)
                    {
                        return Err(claude_error(
                            "Claude sent ambiguous Questionnaire Questions",
                        ));
                    }
                    let mut choices = Vec::new();
                    for option in question.options {
                        if choices
                            .iter()
                            .any(|choice: &QuestionChoice| choice.id == option.label)
                        {
                            return Err(claude_error(
                                "Claude sent duplicate Questionnaire choices",
                            ));
                        }
                        choices.push(QuestionChoice {
                            id: option.label.clone(),
                            recommended: option
                                .label
                                .to_ascii_lowercase()
                                .ends_with("(recommended)"),
                            label: option.label,
                            description: option.description,
                        });
                    }
                    questions.push(Question {
                        id: questions.len().to_string(),
                        title: question.header,
                        text: question.question,
                        choices,
                        multiple: question.multiple,
                        freeform: true,
                        combine_freeform: question.multiple,
                        secret: false,
                        required: true,
                    });
                }
                let questionnaire = Questionnaire {
                    id: QuestionnaireId::new(),
                    questions,
                };
                let mut pending = self
                    .pending
                    .lock()
                    .expect("Claude Questionnaire lock is not poisoned");
                if pending.values().any(|q| q.request_id == request_id) {
                    return Err(claude_error(
                        "Claude reused a live Questionnaire correlation",
                    ));
                }
                pending.insert(
                    questionnaire.id,
                    NativeQuestionnaire {
                        attribution: attribution.clone(),
                        request_id: request_id.to_owned(),
                        input,
                        questionnaire: questionnaire.clone(),
                    },
                );
                Ok(Some(vec![AttributedProviderEvent {
                    attribution,
                    event: ProviderEvent::QuestionnaireRequested { questionnaire },
                }]))
            }
            _ => Ok(None),
        }
    }

    pub(super) async fn submit(
        &self,
        id: QuestionnaireId,
        submission: QuestionnaireSubmission,
    ) -> Result<(), ProviderError> {
        let native = {
            let mut pending = self
                .pending
                .lock()
                .expect("Claude Questionnaire lock is not poisoned");
            let native = pending
                .get(&id)
                .ok_or_else(|| claude_error("Claude Questionnaire is unavailable"))?;
            if let QuestionnaireSubmission::Answer { answer } = &submission {
                native
                    .questionnaire
                    .validate(answer)
                    .map_err(ProviderError::questionnaire_rejected)?;
            }
            pending.remove(&id).expect("validated live Questionnaire")
        };
        let response = match submission {
            QuestionnaireSubmission::Decline => {
                json!({"behavior":"deny", "message":"User declined the Questionnaire", "interrupt":false})
            }
            QuestionnaireSubmission::Answer { answer } => {
                let mut answers = serde_json::Map::new();
                for (question, answer) in
                    native.questionnaire.questions.iter().zip(answer.questions)
                {
                    let value = match answer {
                        QuestionAnswer::Selected { choices } => choices.join(", "),
                        QuestionAnswer::SelectedWithFreeform { mut choices, text } => {
                            choices.push(text);
                            choices.join(", ")
                        }
                        QuestionAnswer::Freeform { text } => text,
                        _ => {
                            return Err(claude_error(
                                "Claude requires an answer to every Question",
                            ));
                        }
                    };
                    answers.insert(question.text.clone(), Value::String(value));
                }
                let mut input = native.input;
                input["answers"] = Value::Object(answers);
                json!({"behavior":"allow", "updatedInput": input})
            }
        };
        self.respond(&native.request_id, response).await
    }

    async fn respond(&self, request_id: &str, response: Value) -> Result<(), ProviderError> {
        let transport = self
            .transport
            .lock()
            .expect("Claude Questionnaire transport lock is not poisoned")
            .clone()
            .ok_or_else(|| claude_error("Claude Questionnaire transport is unavailable"))?;
        transport.send(&json!({"type":"control_response", "response":{"subtype":"success", "request_id":request_id, "response":response}})).await
    }
}
