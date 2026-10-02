//! The Tools through which a Sidekick acts on Sessions — on its own Server, or
//! on a Remote its `origin` names: `send_prompt`, `interrupt_session`,
//! `settle_session`, `unsettle_session` and `answer_questionnaire`.
//!
//! Each is the very act a Client performs, through the same operation the
//! Session API's handler calls — at a Remote, carried there through the
//! Pairing as a Client's act is ([`SessionOperations::admit_prompt_at`] and
//! its siblings) — so what it does and every refusal it meets are a Client's,
//! in the words the refusal gives itself. Each names the Sidekick's own
//! Session as its author: a Prompt it sends, the Message that Prompt becomes,
//! and an Answer it gives say whose words they are wherever they are read —
//! on a Remote, as a Sidekick's on this Peer — and an act on a Session of the
//! Sidekick Workspace — the Sidekick's own among them, or a Remote's, which
//! refuses for itself — is refused where every act is decided (ADR 0043). An
//! Answer is input rather than consent, so a Sidekick gives one; no Tool
//! here, or anywhere, decides an Approval.
//!
//! [`SessionOperations::admit_prompt_at`]: crate::server::operations::SessionOperations::admit_prompt_at

use serde_json::{Map, Value, json};

use super::{BrokerTool, BrokerTools, ToolCall, ToolRefusal, origins, takes_only};
use crate::protocol::{
    AdmitPromptRequest, Answer, Author, InitialPrompt, InterruptOutcome, Outlook, PromptDelivery,
    PromptId, QuestionAnswer, QuestionnaireId, QuestionnaireSubmission, SessionId,
};

pub(super) const SEND_PROMPT_DESCRIPTION: &str = "\
Send a Prompt to a Session on the user's behalf, as the user would from that \
Session's composer; its Transcript shows the Prompt as sent by you, leading \
back to your Session — or, on a Remote, as sent by a Sidekick on this \
machine. Takes \"session_id\", a Session's id as list_sessions gives it; \
\"origin\", the Remote it lives on as its row gives it, left out for one on \
this Suru server; \"prompt\", everything the Session's Agent needs, since it \
sees none of your conversation; and optionally \"delivery\": \"steer\", the \
default, to deliver it into the Turn the Session is working in, or \"queue\" \
to have it wait behind that Turn and begin the next. A Session that is not \
working takes it as a Turn of its own either way. Answers with JSON of the \
shape {\"session_id\": \"...\", \"origin\": \"...\", \"admitted\": \"...\"}, \
naming the Session as its row does, \"origin\" left out for one on this \
server, where \"admitted\" says how the Session took it: \"new_turn\", \
\"steer\" or \"queued\". A Subagent's \
Session takes no Prompt, and no Session of the Sidekick Workspace, your own \
included, takes one from you; either is refused saying so, as is a Remote \
that does not answer, which nothing is kept to send to later.";

pub(super) const INTERRUPT_SESSION_DESCRIPTION: &str = "\
Interrupt a Session, as the user would: stop its working Turn with the \
Subagents it spawned and the Watches it left running, or, where it was \
working only because a Prompt waited to begin a Turn, withdraw that Prompt \
instead. Takes \"session_id\", a Session's id as list_sessions gives it, and \
\"origin\", the Remote it lives on as its row gives it, left out for one on \
this Suru server. Answers with JSON of the shape {\"session_id\": \"...\", \
\"origin\": \"...\", \"outcome\": \"stopped_work\"} when it stopped work, or \
{\"session_id\": \"...\", \"origin\": \"...\", \"outcome\": \
\"withdrew_prompt\", \"prompt\": \"...\"} when it withdrew a Prompt, giving \
that Prompt's text, the Session named as its row does, \"origin\" left out \
for one on this server. A Session with nothing running is \
refused saying so, as is any Session of the Sidekick Workspace, your own \
included, and a Remote that does not answer.";

pub(super) const SETTLE_SESSION_DESCRIPTION: &str = "\
Set a Session aside as done for now, as the user does to tidy their listing: \
it is listed as settled until work reaches it again or it is unsettled. Takes \
\"session_id\", a Session's id as list_sessions gives it, and \"origin\", the \
Remote it lives on as its row gives it, left out for one on this Suru server. \
Answers with JSON of the shape {\"session_id\": \"...\", \"origin\": \"...\", \
\"settled\": true}, the Session named as its row does, \"origin\" left out \
for one on this server; a Session already settled stays so. Any Session of \
the Sidekick Workspace, your own included, is refused, as is a Remote that \
does not answer.";

pub(super) const UNSETTLE_SESSION_DESCRIPTION: &str = "\
Bring a settled Session back among the active ones, as the user does. Takes \
\"session_id\", a Session's id as list_sessions gives it, and \"origin\", the \
Remote it lives on as its row gives it, left out for one on this Suru server. \
Answers with JSON of the shape {\"session_id\": \"...\", \"origin\": \"...\", \
\"settled\": false}, the Session named as its row does, \"origin\" left out \
for one on this server; a Session already active stays so. Any Session of the \
Sidekick Workspace, your own included, is refused, as is a Remote that does \
not answer.";

pub(super) const ANSWER_QUESTIONNAIRE_DESCRIPTION: &str = "\
Answer a Questionnaire waiting in a Session on the user's behalf, as the user \
would from that Session's answering panel, so the Turn that asked it goes \
on; its Transcript shows the Answer as given by you, leading back to your \
Session — or, on a Remote, as given by a Sidekick on this machine. Takes \
\"session_id\", the Session's id; \"origin\", the Remote it lives on as its \
row gives it, left out for one on this Suru server; \"questionnaire_id\", the \
Questionnaire's \"id\" as read_session gives it among \"questionnaires\"; \
and \"answers\", one Answer for each of its Questions, in the order \
read_session gives them. Each Answer is an object \
with \"choices\", a list of the ids of the choices it picks, \"text\", free \
text, or both where the Question takes choices with free text beside them; \
{} leaves a Question that is not required unanswered. Pick one choice unless \
the Question takes \"multiple\", and give text only where it takes \
\"freeform\". A secret Question's Answer reaches the Agent, and Suru keeps \
only that it was answered; your own Session's record of the call keeps which \
Questionnaire it answered and how many Answers it gave, never what they said. \
Answers with JSON of the shape {\"session_id\": \
\"...\", \"origin\": \"...\", \"questionnaire_id\": \"...\", \"answered\": \
true} once the Session's Agent has the Answer, the Session named as its row \
does, \"origin\" left out for one on this server. A Questionnaire already answered or no longer \
waiting, an Answer for each Question missing, or a choice a Question does not \
offer is refused saying why, as is any Session of the Sidekick Workspace, \
your own included, and a Remote that does not answer. An Approval is no \
Questionnaire: only the user decides one.";

/// What `interrupt_session`, `settle_session` and `unsettle_session` take:
/// the one Session they act on, and the Server it lives on.
const SESSION_TAKES: [&str; 2] = ["session_id", "origin"];

/// The JSON Schema of `send_prompt`'s arguments.
pub(super) fn send_prompt_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "session_id": session_id_schema(),
            "origin": origins::origin_property(),
            "prompt": {
                "type": "string",
                "description": "Everything the Session's Agent needs to do what you ask; it \
                    sees none of your conversation.",
            },
            "delivery": {
                "type": "string",
                "enum": Delivery::NAMES,
                "description": "\"steer\" (the default) to deliver it into the Turn the \
                    Session is working in, or \"queue\" to have it wait for the next.",
            },
        },
        "required": SendArguments::REQUIRED,
        "additionalProperties": false,
    })
}

/// The JSON Schema of the arguments of a Tool taking the one Session it acts
/// on.
pub(super) fn session_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "session_id": session_id_schema(),
            "origin": origins::origin_property(),
        },
        "required": ["session_id"],
        "additionalProperties": false,
    })
}

/// The JSON Schema of `answer_questionnaire`'s arguments.
pub(super) fn answer_questionnaire_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "session_id": session_id_schema(),
            "origin": origins::origin_property(),
            "questionnaire_id": {
                "type": "string",
                "description": "The Questionnaire's id, as read_session gives it among the \
                    Session's questionnaires.",
            },
            "answers": {
                "type": "array",
                "description": "One Answer for each of the Questionnaire's Questions, in their \
                    order; {} leaves one that is not required unanswered.",
                "items": {
                    "type": "object",
                    "properties": {
                        "choices": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "The ids of the choices picked, as read_session \
                                gives them.",
                        },
                        "text": {
                            "type": "string",
                            "description": "Free text, where the Question takes it.",
                        },
                    },
                    "additionalProperties": false,
                },
            },
        },
        "required": AnswerArguments::REQUIRED,
        "additionalProperties": false,
    })
}

fn session_id_schema() -> Value {
    json!({
        "type": "string",
        "description": "The id of a Session, as list_sessions gives it.",
    })
}

impl BrokerTools {
    /// The author of an act the calling Sidekick performs: the Sidekick's
    /// own Session, named by its Title as it stands now.
    pub(super) fn sidekick_author(&self, call: &ToolCall) -> Author {
        let session_id = call.caller.session_id();
        Author::Sidekick {
            session_id,
            title: self.sessions.title(session_id).unwrap_or_default(),
        }
    }

    /// Answers `send_prompt`: admits the Prompt as a Client's admission does,
    /// authored by the calling Sidekick, and says how the Session took it.
    pub(super) async fn send_prompt(&self, call: ToolCall) -> Result<Value, ToolRefusal> {
        let send = SendArguments::read(&call.arguments)?;
        let origin = origins::origin(BrokerTool::SendPrompt, &call.arguments)?;
        let author = self.sidekick_author(&call);
        let admitted = self
            .operations
            .admit_prompt_at(
                &origin,
                send.session_id,
                AdmitPromptRequest {
                    prompt: InitialPrompt {
                        id: PromptId::new(),
                        text: send.prompt,
                        skill_invocations: Vec::new(),
                        attachments: Vec::new(),
                    },
                    delivery: send.delivery.into(),
                },
                author,
            )
            .await
            .map_err(|refusal| origins::act_refusal_on(refusal, send.session_id))?;
        // A Prompt the Broker mints is new to every Session, so it is
        // admitted afresh and says how.
        Ok(at_origin(
            json!({
                "session_id": send.session_id,
                "admitted": admitted.map(|admitted| admitted.name()),
            }),
            origin,
        ))
    }

    /// Answers `interrupt_session`: interrupts the Session as a Client's
    /// interrupt does, and says whether that stopped work or withdrew a
    /// Prompt.
    pub(super) async fn interrupt_session(&self, call: ToolCall) -> Result<Value, ToolRefusal> {
        let session_id = named_session(
            BrokerTool::InterruptSession,
            &call.arguments,
            &SESSION_TAKES,
        )?;
        let origin = origins::origin(BrokerTool::InterruptSession, &call.arguments)?;
        let author = self.sidekick_author(&call);
        let outcome = self
            .operations
            .interrupt_session_at(&origin, session_id, author)
            .await
            .map_err(|refusal| origins::act_refusal_on(refusal, session_id))?;
        let answer = match outcome {
            InterruptOutcome::StoppedWork => {
                json!({ "session_id": session_id, "outcome": "stopped_work" })
            }
            InterruptOutcome::WithdrewPrompt { prompt } => json!({
                "session_id": session_id,
                "outcome": "withdrew_prompt",
                "prompt": prompt.text,
            }),
        };
        Ok(at_origin(answer, origin))
    }

    /// Answers `answer_questionnaire`: answers the Questionnaire as a Client's
    /// submission does, authored by the calling Sidekick, once its Agent has
    /// the Answer.
    pub(super) async fn answer_questionnaire(&self, call: ToolCall) -> Result<Value, ToolRefusal> {
        let answering = AnswerArguments::read(&call.arguments)?;
        let origin = origins::origin(BrokerTool::AnswerQuestionnaire, &call.arguments)?;
        let author = self.sidekick_author(&call);
        self.operations
            .answer_questionnaire_at(
                &origin,
                answering.session_id,
                answering.questionnaire_id,
                QuestionnaireSubmission::Answer {
                    answer: answering.answer,
                },
                author,
            )
            .await
            .map_err(|refusal| origins::act_refusal_on(refusal, answering.session_id))?;
        Ok(at_origin(
            json!({
                "session_id": answering.session_id,
                "questionnaire_id": answering.questionnaire_id,
                "answered": true,
            }),
            origin,
        ))
    }

    /// Answers `settle_session` where `settled`, and `unsettle_session`
    /// otherwise: sets the Session aside or brings it back as a Client does,
    /// and says how it stands.
    pub(super) async fn settle_session(
        &self,
        tool: BrokerTool,
        call: ToolCall,
        settled: bool,
    ) -> Result<Value, ToolRefusal> {
        let session_id = named_session(tool, &call.arguments, &SESSION_TAKES)?;
        let origin = origins::origin(tool, &call.arguments)?;
        let author = self.sidekick_author(&call);
        let summary = self
            .operations
            .settle_session_at(&origin, session_id, settled, author)
            .await
            .map_err(|refusal| origins::act_refusal_on(refusal, session_id))?;
        Ok(at_origin(
            json!({
                "session_id": session_id,
                "settled": summary.settled_at.is_some(),
            }),
            origin,
        ))
    }
}

/// `answer`, what an act on a Session at `origin` answers, naming that
/// Session as its row does: by its Remote beside its id, where it lives on
/// one, since the id alone may name another Session on another Server.
fn at_origin(mut answer: Value, origin: Outlook) -> Value {
    if let (Value::Object(named), Some(origin)) = (&mut answer, origins::row_origin(origin)) {
        named.insert("origin".to_owned(), json!(origin));
    }
    answer
}

/// How `send_prompt` asks for its Prompt to be delivered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Delivery {
    Steer,
    Queue,
}

impl Delivery {
    const NAMES: [&'static str; 2] = ["steer", "queue"];
}

impl From<Delivery> for PromptDelivery {
    fn from(delivery: Delivery) -> Self {
        match delivery {
            Delivery::Steer => Self::Steer,
            Delivery::Queue => Self::Queue,
        }
    }
}

/// What `send_prompt` was called with: the Session to send to, what it is
/// asked, and how the Prompt is to be delivered.
#[derive(Debug, Eq, PartialEq)]
struct SendArguments {
    session_id: SessionId,
    prompt: String,
    delivery: Delivery,
}

impl SendArguments {
    /// Everything a call may name.
    const TAKES: [&'static str; 4] = ["session_id", "origin", "prompt", "delivery"];
    /// What a call must name: the delivery may be left to the default.
    const REQUIRED: [&'static str; 2] = ["session_id", "prompt"];

    fn read(arguments: &Map<String, Value>) -> Result<Self, ToolRefusal> {
        let session_id = named_session(BrokerTool::SendPrompt, arguments, &Self::TAKES)?;
        // A Prompt that says nothing is refused where every Prompt is, in the
        // words a Client is refused with.
        let prompt = match arguments.get("prompt") {
            Some(Value::String(prompt)) => prompt.clone(),
            None | Some(Value::Null) => {
                return Err(ToolRefusal::new(
                    "send_prompt needs `prompt`, what the Session's Agent is to do.",
                ));
            }
            Some(_) => {
                return Err(ToolRefusal::new("send_prompt's `prompt` must be a string."));
            }
        };
        let delivery = match arguments.get("delivery") {
            None | Some(Value::Null) => Delivery::Steer,
            Some(Value::String(named)) if named == "steer" => Delivery::Steer,
            Some(Value::String(named)) if named == "queue" => Delivery::Queue,
            Some(other) => {
                return Err(ToolRefusal::new(format!(
                    "send_prompt's `delivery` must be `steer` or `queue`; {other} is neither."
                )));
            }
        };
        Ok(Self {
            session_id,
            prompt,
            delivery,
        })
    }
}

/// What a Transcript keeps of the arguments an `answer_questionnaire` call
/// was made with — in the Sidekick's own Tool Call, which is stored, sent to
/// every Client, and read by any Sidekick: the Session and Questionnaire it
/// named, where each is an identity, and how many Answers it gave, never what
/// any of them said. Which Questions are secret is the Questionnaire's to say,
/// not the call's, so every Answer is withheld alike, and so is whatever else
/// the call carried.
pub(super) fn recorded_answer_arguments(arguments: &Value) -> Value {
    const WITHHELD: &str = "withheld";
    let Value::Object(arguments) = arguments else {
        return Value::Object(Map::new());
    };
    let identity = |given: &Value, is_one: bool| {
        if is_one {
            given.clone()
        } else {
            json!(WITHHELD)
        }
    };
    let mut recorded = Map::new();
    if let Some(given) = arguments.get("session_id") {
        let is_one = serde_json::from_value::<SessionId>(given.clone()).is_ok();
        recorded.insert("session_id".to_owned(), identity(given, is_one));
    }
    // A Remote's name is no Answer's, so the Origin named is kept as given,
    // where it is a name.
    if let Some(given) = arguments.get("origin") {
        recorded.insert("origin".to_owned(), identity(given, given.is_string()));
    }
    if let Some(given) = arguments.get("questionnaire_id") {
        let is_one = serde_json::from_value::<QuestionnaireId>(given.clone()).is_ok();
        recorded.insert("questionnaire_id".to_owned(), identity(given, is_one));
    }
    if let Some(answers) = arguments.get("answers") {
        recorded.insert(
            "answers".to_owned(),
            json!(match answers {
                Value::Array(answers) if answers.len() == 1 => "1 Answer withheld".to_owned(),
                Value::Array(answers) => format!("{} Answers withheld", answers.len()),
                _ => WITHHELD.to_owned(),
            }),
        );
    }
    Value::Object(recorded)
}

/// What `answer_questionnaire` was called with: the Session, its
/// Questionnaire, and the Answer, one for each Question in their order.
#[derive(Debug, Eq, PartialEq)]
struct AnswerArguments {
    session_id: SessionId,
    questionnaire_id: QuestionnaireId,
    answer: Answer,
}

impl AnswerArguments {
    /// Everything a call may name.
    const TAKES: [&'static str; 4] = ["session_id", "origin", "questionnaire_id", "answers"];
    /// What a call must name: the Origin may be left to this server.
    const REQUIRED: [&'static str; 3] = ["session_id", "questionnaire_id", "answers"];
    /// What each Answer among `answers` may name.
    const ANSWER_TAKES: [&'static str; 2] = ["choices", "text"];

    /// The call's arguments, each checked for the shape its schema gives it.
    /// A call may carry a secret anywhere — even as the name it gives an
    /// argument — so no refusal repeats what it was given: it says which
    /// argument, where, and what was expected.
    fn read(arguments: &Map<String, Value>) -> Result<Self, ToolRefusal> {
        // Unlike other Tools' refusals, this one names no argument it was not
        // meant to be given: a call may carry a secret as the name of one.
        if arguments
            .keys()
            .any(|argument| !Self::TAKES.contains(&argument.as_str()))
        {
            return Err(ToolRefusal::new(
                "answer_questionnaire takes only `session_id`, `origin`, `questionnaire_id` and \
                 `answers`, and was given an argument besides them.",
            ));
        }
        let session_id = match arguments.get("session_id") {
            None | Some(Value::Null) => {
                return Err(ToolRefusal::new(
                    "answer_questionnaire needs `session_id`, the id of a Session as \
                     list_sessions gives it.",
                ));
            }
            Some(id) => serde_json::from_value(id.clone()).map_err(|_| {
                ToolRefusal::new(
                    "answer_questionnaire's `session_id` must be the id of a Session as \
                     list_sessions gives it, and what was given is not one.",
                )
            })?,
        };
        let questionnaire_id = match arguments.get("questionnaire_id") {
            None | Some(Value::Null) => {
                return Err(ToolRefusal::new(
                    "answer_questionnaire needs `questionnaire_id`, the id read_session gives \
                     the Questionnaire.",
                ));
            }
            Some(id) => serde_json::from_value(id.clone()).map_err(|_| {
                ToolRefusal::new(
                    "answer_questionnaire's `questionnaire_id` must be a Questionnaire's id as \
                     read_session gives it, and what was given is not one.",
                )
            })?,
        };
        let answers = match arguments.get("answers") {
            None | Some(Value::Null) => {
                return Err(ToolRefusal::new(
                    "answer_questionnaire needs `answers`, one Answer for each of the \
                     Questionnaire's Questions, in their order.",
                ));
            }
            Some(Value::Array(answers)) => answers,
            Some(other) => {
                return Err(ToolRefusal::new(format!(
                    "answer_questionnaire's `answers` must be a list, one Answer for each of the \
                     Questionnaire's Questions, in their order, and it was given {}.",
                    kind_of(other)
                )));
            }
        };
        let questions = answers
            .iter()
            .enumerate()
            .map(|(index, given)| question_answer(index + 1, given))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            session_id,
            questionnaire_id,
            answer: Answer { questions },
        })
    }
}

/// The Answer numbered `number` among `answers` reads as: the choices it picks,
/// the free text it gives, both, or — given neither — the Question left
/// unanswered. Text with nothing in it is no text, so a blank one beside a
/// choice is the choice alone. Whether the Question takes what it reads as is
/// for the operation to judge, in the words a Client is told. Any of it may be
/// a secret Question's, so a refusal names what it is, never what it says —
/// not even a name it gives what it says under.
fn question_answer(number: usize, given: &Value) -> Result<QuestionAnswer, ToolRefusal> {
    let given = match given {
        Value::Null => return Ok(QuestionAnswer::Omitted),
        Value::Object(given) => given,
        other => {
            return Err(ToolRefusal::new(format!(
                "Answer {number} in `answers` must be an object giving `choices`, `text`, or \
                 both, and it was given {}.",
                kind_of(other)
            )));
        }
    };
    if given
        .keys()
        .any(|key| !AnswerArguments::ANSWER_TAKES.contains(&key.as_str()))
    {
        return Err(ToolRefusal::new(format!(
            "Answer {number} in `answers` names something other than `choices` and `text`, \
             which are all an Answer takes."
        )));
    }
    let choices = match given.get("choices") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(choices)) => choices
            .iter()
            .map(|choice| choice.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                ToolRefusal::new(format!(
                    "Answer {number}'s `choices` must be a list of the ids of the Question's \
                     choices, as read_session gives them, and it was given a list holding \
                     something other than strings."
                ))
            })?,
        Some(other) => {
            return Err(ToolRefusal::new(format!(
                "Answer {number}'s `choices` must be a list of the ids of the Question's \
                 choices, as read_session gives them, and it was given {}.",
                kind_of(other)
            )));
        }
    };
    let text = match given.get("text") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.clone()).filter(|text| !text.trim().is_empty()),
        Some(other) => {
            return Err(ToolRefusal::new(format!(
                "Answer {number}'s `text` must be a string, and it was given {}.",
                kind_of(other)
            )));
        }
    };
    Ok(match (choices.is_empty(), text) {
        (true, None) => QuestionAnswer::Omitted,
        (true, Some(text)) => QuestionAnswer::Freeform { text },
        (false, None) => QuestionAnswer::Selected { choices },
        (false, Some(text)) => QuestionAnswer::SelectedWithFreeform { choices, text },
    })
}

/// What kind of JSON `value` is, as a refusal names it in place of the value.
fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "nothing",
        Value::Bool(_) => "true or false",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

/// The Session a call of `tool` names by its `session_id` argument, having
/// refused any argument `tool` does not take, as `takes` lists them.
fn named_session(
    tool: BrokerTool,
    arguments: &Map<String, Value>,
    takes: &[&str],
) -> Result<SessionId, ToolRefusal> {
    let name = tool.name();
    takes_only(tool, arguments, takes)?;
    match arguments.get("session_id") {
        None | Some(Value::Null) => Err(ToolRefusal::new(format!(
            "{name} needs `session_id`, the id of a Session as list_sessions gives it."
        ))),
        Some(id) => serde_json::from_value(id.clone()).map_err(|_| {
            ToolRefusal::new(format!(
                "{name}'s `session_id` must be the id of a Session as list_sessions gives it; \
                 {id} is not one."
            ))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(value: Value) -> Map<String, Value> {
        let Value::Object(arguments) = value else {
            panic!("arguments are an object");
        };
        arguments
    }

    #[test]
    fn send_arguments_steer_unless_asked_to_queue() {
        let session_id = SessionId::new();
        assert_eq!(
            SendArguments::read(&arguments(json!({
                "session_id": session_id,
                "prompt": "Pick this back up",
            }))),
            Ok(SendArguments {
                session_id,
                prompt: "Pick this back up".to_owned(),
                delivery: Delivery::Steer,
            })
        );
        assert_eq!(
            SendArguments::read(&arguments(json!({
                "session_id": session_id,
                "prompt": "Then this",
                "delivery": "queue",
            })))
            .map(|send| send.delivery),
            Ok(Delivery::Queue)
        );
    }

    #[test]
    fn send_arguments_are_refused_in_words_the_sidekick_can_act_on() {
        let session_id = SessionId::new();
        for (sent, says) in [
            (
                json!({ "prompt": "Go on" }),
                "send_prompt needs `session_id`, the id of a Session as list_sessions gives it.",
            ),
            (
                json!({ "session_id": "not-a-session", "prompt": "Go on" }),
                "send_prompt's `session_id` must be the id of a Session as list_sessions gives \
                 it; \"not-a-session\" is not one.",
            ),
            (
                json!({ "session_id": session_id }),
                "send_prompt needs `prompt`, what the Session's Agent is to do.",
            ),
            (
                json!({ "session_id": session_id, "prompt": 7 }),
                "send_prompt's `prompt` must be a string.",
            ),
            (
                json!({ "session_id": session_id, "prompt": "Go on", "delivery": "later" }),
                "send_prompt's `delivery` must be `steer` or `queue`; \"later\" is neither.",
            ),
            (
                json!({ "session_id": session_id, "prompt": "Go on", "after": "lunch" }),
                "send_prompt takes no argument `after`; it takes `session_id`, `origin`, \
                 `prompt`, `delivery`.",
            ),
        ] {
            assert_eq!(
                SendArguments::read(&arguments(sent.clone())),
                Err(ToolRefusal::new(says)),
                "{sent}"
            );
        }
    }

    #[test]
    fn each_tool_acting_on_a_session_requires_what_its_description_says_it_takes() {
        assert_eq!(
            send_prompt_schema()["required"],
            json!(["session_id", "prompt"])
        );
        assert_eq!(
            send_prompt_schema()["properties"]["delivery"]["enum"],
            json!(["steer", "queue"])
        );
        assert_eq!(session_schema()["required"], json!(["session_id"]));
        for schema in [
            send_prompt_schema(),
            session_schema(),
            answer_questionnaire_schema(),
        ] {
            assert!(
                schema["properties"].get("origin").is_some(),
                "each Tool acting on a Session takes the Origin it lives on: {schema}"
            );
        }
        for description in [
            SEND_PROMPT_DESCRIPTION,
            INTERRUPT_SESSION_DESCRIPTION,
            SETTLE_SESSION_DESCRIPTION,
            UNSETTLE_SESSION_DESCRIPTION,
            ANSWER_QUESTIONNAIRE_DESCRIPTION,
        ] {
            assert!(
                description.contains("\"session_id\"")
                    && description.contains("\"origin\"")
                    && description.contains("Sidekick Workspace, your own included")
                    && description.contains("Remote that does not answer"),
                "each description says what it takes and what it refuses: {description}"
            );
            assert!(
                description.contains("{\"session_id\": \"...\", \"origin\": \"...\",")
                    && description.contains("\"origin\" left out for one on this server"),
                "and that its answer names the Session by its Remote too: {description}"
            );
        }
        assert!(
            SEND_PROMPT_DESCRIPTION.contains("\"prompt\"")
                && SEND_PROMPT_DESCRIPTION.contains("\"delivery\"")
        );
        for outcome in ["\"new_turn\"", "\"steer\"", "\"queued\""] {
            assert!(SEND_PROMPT_DESCRIPTION.contains(outcome), "{outcome}");
        }
        for outcome in ["\"stopped_work\"", "\"withdrew_prompt\""] {
            assert!(INTERRUPT_SESSION_DESCRIPTION.contains(outcome), "{outcome}");
        }
        assert_eq!(
            answer_questionnaire_schema()["required"],
            json!(["session_id", "questionnaire_id", "answers"])
        );
        for named in AnswerArguments::TAKES
            .into_iter()
            .chain(AnswerArguments::ANSWER_TAKES)
            .chain(["\"answered\": true", "multiple", "freeform"])
        {
            assert!(
                ANSWER_QUESTIONNAIRE_DESCRIPTION.contains(named),
                "answer_questionnaire's description names {named}"
            );
        }
        assert!(
            ANSWER_QUESTIONNAIRE_DESCRIPTION.contains("only the user decides one"),
            "and says an Approval is not a Sidekick's to decide"
        );
    }

    #[test]
    fn an_act_names_a_remotes_session_by_its_remote_and_this_servers_by_its_id_alone() {
        let session_id = SessionId::new();
        let answer = json!({ "session_id": session_id, "settled": true });
        assert_eq!(at_origin(answer.clone(), Outlook::Local), answer);
        assert_eq!(
            at_origin(answer, Outlook::Remote("workstation".to_owned())),
            json!({ "session_id": session_id, "origin": "workstation", "settled": true })
        );
    }

    #[test]
    fn each_answer_reads_as_choices_text_both_or_neither() {
        let session_id = SessionId::new();
        let questionnaire_id = QuestionnaireId::new();
        let selected = |choices: &[&str]| {
            choices
                .iter()
                .map(|choice| (*choice).to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            AnswerArguments::read(&arguments(json!({
                "session_id": session_id,
                "questionnaire_id": questionnaire_id,
                "answers": [
                    { "choices": ["staging"] },
                    { "text": "after the backup" },
                    { "choices": ["unit", "doc"], "text": "in that order" },
                    {},
                    null,
                    { "choices": [], "text": "  " },
                    { "choices": ["local"], "text": "" },
                ],
            }))),
            Ok(AnswerArguments {
                session_id,
                questionnaire_id,
                answer: Answer {
                    questions: vec![
                        QuestionAnswer::Selected {
                            choices: selected(&["staging"]),
                        },
                        QuestionAnswer::Freeform {
                            text: "after the backup".to_owned(),
                        },
                        QuestionAnswer::SelectedWithFreeform {
                            choices: selected(&["unit", "doc"]),
                            text: "in that order".to_owned(),
                        },
                        QuestionAnswer::Omitted,
                        QuestionAnswer::Omitted,
                        QuestionAnswer::Omitted,
                        QuestionAnswer::Selected {
                            choices: selected(&["local"]),
                        },
                    ],
                },
            })
        );
    }

    /// A Transcript records which Session and Questionnaire a call named, and
    /// how many Answers it gave, and nothing of what any Answer or anything
    /// else the call carried said.
    #[test]
    fn a_call_is_recorded_by_its_session_and_questionnaire_and_the_count_of_its_answers() {
        let session_id = SessionId::new();
        let questionnaire_id = QuestionnaireId::new();
        assert_eq!(
            recorded_answer_arguments(&json!({
                "session_id": session_id,
                "questionnaire_id": questionnaire_id,
                "answers": [{ "text": "tok-1" }, { "choices": ["eu"] }],
            })),
            json!({
                "session_id": session_id,
                "questionnaire_id": questionnaire_id,
                "answers": "2 Answers withheld",
            })
        );
        assert_eq!(
            recorded_answer_arguments(&json!({ "answers": [null] })),
            json!({ "answers": "1 Answer withheld" })
        );
        let malformed = recorded_answer_arguments(&json!({
            "session_id": "tok-1",
            "questionnaire_id": { "tok": "tok-1" },
            "answers": "tok-1",
            "tok-1": "tok-1",
        }));
        assert_eq!(
            malformed,
            json!({
                "session_id": "withheld",
                "questionnaire_id": "withheld",
                "answers": "withheld",
            }),
            "what is not an identity, and what the Tool does not take, is withheld"
        );
        assert_eq!(recorded_answer_arguments(&json!(["tok-1"])), json!({}));
    }

    #[test]
    fn answer_arguments_are_refused_in_words_the_sidekick_can_act_on() {
        let session_id = SessionId::new();
        let questionnaire_id = QuestionnaireId::new();
        for (sent, says) in [
            (
                json!({ "session_id": session_id, "answers": [] }),
                "answer_questionnaire needs `questionnaire_id`, the id read_session gives the \
                 Questionnaire.",
            ),
            (
                json!({ "session_id": session_id, "questionnaire_id": 4, "answers": [] }),
                "answer_questionnaire's `questionnaire_id` must be a Questionnaire's id as \
                 read_session gives it, and what was given is not one.",
            ),
            (
                json!({ "session_id": session_id, "questionnaire_id": questionnaire_id }),
                "answer_questionnaire needs `answers`, one Answer for each of the \
                 Questionnaire's Questions, in their order.",
            ),
            (
                json!({
                    "session_id": session_id,
                    "questionnaire_id": questionnaire_id,
                    "answers": { "machine": "staging" },
                }),
                "answer_questionnaire's `answers` must be a list, one Answer for each of the \
                 Questionnaire's Questions, in their order, and it was given an object.",
            ),
            (
                json!({
                    "session_id": session_id,
                    "questionnaire_id": questionnaire_id,
                    "answers": ["staging"],
                }),
                "Answer 1 in `answers` must be an object giving `choices`, `text`, or both, and \
                 it was given a string.",
            ),
            (
                json!({
                    "session_id": session_id,
                    "questionnaire_id": questionnaire_id,
                    "answers": [{}, { "choices": "staging" }],
                }),
                "Answer 2's `choices` must be a list of the ids of the Question's choices, as \
                 read_session gives them, and it was given a string.",
            ),
            (
                json!({
                    "session_id": session_id,
                    "questionnaire_id": questionnaire_id,
                    "answers": [{ "text": 7 }],
                }),
                "Answer 1's `text` must be a string, and it was given a number.",
            ),
            (
                json!({
                    "session_id": session_id,
                    "questionnaire_id": questionnaire_id,
                    "answers": [{ "choice": "staging" }],
                }),
                "Answer 1 in `answers` names something other than `choices` and `text`, which \
                 are all an Answer takes.",
            ),
            (
                json!({
                    "session_id": session_id,
                    "questionnaire_id": questionnaire_id,
                    "answers": [],
                    "decision": "accept",
                }),
                "answer_questionnaire takes only `session_id`, `origin`, `questionnaire_id` and \
                 `answers`, and was given an argument besides them.",
            ),
        ] {
            assert_eq!(
                AnswerArguments::read(&arguments(sent.clone())),
                Err(ToolRefusal::new(says)),
                "{sent}"
            );
        }
    }
}
