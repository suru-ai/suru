//! The Tools through which a Sidekick acts on the Sessions on its own Server:
//! `send_prompt`, `interrupt_session`, `settle_session` and
//! `unsettle_session`.
//!
//! Each is the very act a Client performs, through the same operation the
//! Session API's handler calls, so what it does and every refusal it meets are
//! a Client's, in the words the refusal gives itself. Each names the
//! Sidekick's own Session as its author: a Prompt it sends, and the Message
//! that Prompt becomes, say whose words they are wherever they are read, and
//! an act on a Session of the Sidekick Workspace — the Sidekick's own among
//! them — is refused where every act is decided (ADR 0043).

use serde_json::{Map, Value, json};

use super::{BrokerTool, BrokerTools, ToolCall, ToolRefusal, takes_only};
use crate::{
    protocol::{
        AdmitPromptRequest, Author, InitialPrompt, InterruptOutcome, PromptDelivery, PromptId,
        SessionId,
    },
    server::operations::AdmittedDelivery,
    sessions::StoreOutcome,
};

pub(super) const SEND_PROMPT_DESCRIPTION: &str = "\
Send a Prompt to a Session on this Suru server on the user's behalf, as the \
user would from that Session's composer; its Transcript shows the Prompt as \
sent by you, leading back to your Session. Takes \"session_id\", a Session's \
id as list_sessions gives it; \"prompt\", everything the Session's Agent \
needs, since it sees none of your conversation; and optionally \"delivery\": \
\"steer\", the default, to deliver it into the Turn the Session is working \
in, or \"queue\" to have it wait behind that Turn and begin the next. A \
Session that is not working takes it as a Turn of its own either way. Answers \
with JSON of the shape {\"session_id\": \"...\", \"admitted\": \"...\"}, where \
\"admitted\" says how the Session took it: \"new_turn\", \"steer\" or \
\"queued\". A Subagent's Session takes no Prompt, and no Session of the \
Sidekick Workspace, your own included, takes one from you; either is refused \
saying so.";

pub(super) const INTERRUPT_SESSION_DESCRIPTION: &str = "\
Interrupt a Session on this Suru server, as the user would: stop its working \
Turn with the Subagents it spawned and the Watches it left running, or, where \
it was working only because a Prompt waited to begin a Turn, withdraw that \
Prompt instead. Takes \"session_id\", a Session's id as list_sessions gives \
it. Answers with JSON of the shape {\"session_id\": \"...\", \"outcome\": \
\"stopped_work\"} when it stopped work, or {\"session_id\": \"...\", \
\"outcome\": \"withdrew_prompt\", \"prompt\": \"...\"} when it withdrew a \
Prompt, giving that Prompt's text. A Session with nothing running is refused \
saying so, as is any Session of the Sidekick Workspace, your own included.";

pub(super) const SETTLE_SESSION_DESCRIPTION: &str = "\
Set a Session on this Suru server aside as done for now, as the user does to \
tidy their listing: it is listed as settled until work reaches it again or it \
is unsettled. Takes \"session_id\", a Session's id as list_sessions gives it. \
Answers with JSON of the shape {\"session_id\": \"...\", \"settled\": true}; a \
Session already settled stays so. Any Session of the Sidekick Workspace, your \
own included, is refused.";

pub(super) const UNSETTLE_SESSION_DESCRIPTION: &str = "\
Bring a settled Session on this Suru server back among the active ones, as \
the user does. Takes \"session_id\", a Session's id as list_sessions gives it. \
Answers with JSON of the shape {\"session_id\": \"...\", \"settled\": false}; \
a Session already active stays so. Any Session of the Sidekick Workspace, \
your own included, is refused.";

/// What `interrupt_session`, `settle_session` and `unsettle_session` take:
/// the one Session they act on.
const SESSION_TAKES: [&str; 1] = ["session_id"];

/// The JSON Schema of `send_prompt`'s arguments.
pub(super) fn send_prompt_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "session_id": session_id_schema(),
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
        "properties": { "session_id": session_id_schema() },
        "required": SESSION_TAKES,
        "additionalProperties": false,
    })
}

fn session_id_schema() -> Value {
    json!({
        "type": "string",
        "description": "The id of a Session on this Suru server, as list_sessions gives it.",
    })
}

impl BrokerTools {
    /// The author of an act the calling Sidekick performs: the Sidekick's
    /// own Session, named by its Title as it stands now.
    fn sidekick_author(&self, call: &ToolCall) -> Author {
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
        let author = self.sidekick_author(&call);
        let admitted = match self
            .operations
            .admit_prompt(
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
                Some(author),
            )
            .await
            .map_err(|refusal| ToolRefusal::new(refusal.to_string()))?
        {
            StoreOutcome::Created(admitted) | StoreOutcome::Existing(admitted) => admitted,
        };
        let admitted = admitted
            .delivery
            .expect("a Prompt the Broker mints is new to every Session, so it is admitted afresh");
        Ok(json!({
            "session_id": send.session_id,
            "admitted": match admitted {
                AdmittedDelivery::NewTurn => "new_turn",
                AdmittedDelivery::Steer => "steer",
                AdmittedDelivery::Queued => "queued",
            },
        }))
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
        let author = self.sidekick_author(&call);
        let outcome = self
            .operations
            .interrupt_session(session_id, Some(&author))
            .await
            .map_err(|refusal| ToolRefusal::new(refusal.to_string()))?;
        Ok(match outcome {
            InterruptOutcome::StoppedWork => {
                json!({ "session_id": session_id, "outcome": "stopped_work" })
            }
            InterruptOutcome::WithdrewPrompt { prompt } => json!({
                "session_id": session_id,
                "outcome": "withdrew_prompt",
                "prompt": prompt.text,
            }),
        })
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
        let author = self.sidekick_author(&call);
        let summary = self
            .operations
            .settle_session(session_id, settled, Some(&author))
            .await
            .map_err(|refusal| ToolRefusal::new(refusal.to_string()))?;
        Ok(json!({
            "session_id": session_id,
            "settled": summary.settled_at.is_some(),
        }))
    }
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
    const TAKES: [&'static str; 3] = ["session_id", "prompt", "delivery"];
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
                json!({ "session_id": session_id, "prompt": "Go on", "origin": "studio" }),
                "send_prompt takes no argument `origin`; it takes `session_id`, `prompt`, \
                 `delivery`.",
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
        for description in [
            SEND_PROMPT_DESCRIPTION,
            INTERRUPT_SESSION_DESCRIPTION,
            SETTLE_SESSION_DESCRIPTION,
            UNSETTLE_SESSION_DESCRIPTION,
        ] {
            assert!(
                description.contains("\"session_id\"")
                    && description.contains("Sidekick Workspace, your own included"),
                "each description says what it takes and what it refuses: {description}"
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
    }
}
