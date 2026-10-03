//! `read_session`: a Sidekick's reading of one Session, on its own Server or a
//! Remote, as compact text rendered by the projection that reads a Transcript
//! for an Agent ([`agent_reading`]). A Remote's Session, named with its
//! Origin, is fetched through the Pairing as a Client's read of it is — its
//! snapshot from the Remote's own Session API, and its Standing from the
//! Remote's own listing — and read through the same projection, so it reads
//! as the same read made on that Remote would. A Remote that does not answer
//! is named as not answering, and nothing it said before is read instead.
//!
//! Any Session may be read — the user's, a Subagent's, another Sidekick's,
//! the Sidekick's own — since what keeps a Sidekick from the Sessions of the
//! Sidekick Workspace bounds acts, never reads (ADR 0043). A Session is
//! brought into memory as the Session API brings one, so a Session persisted
//! before the Server last started, with no Provider running for it, reads as
//! well as one at work. And reading changes nothing: it is no Viewed report,
//! so the Session's Standing reads afterwards as it did before. Bringing a
//! Sidekick's Session back from storage does put right a row leading into a
//! Subsession it began that a stop lost, as any load of it does, without
//! moving how that Session stands or when it last moved.

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::{
    BrokerTool, BrokerTools, ToolCall, ToolRefusal, listed, origins,
    session_listing::standing_name, takes_only,
};
use crate::{
    protocol::{
        Outlook, QuestionnaireId, SessionId, SessionListItem, SessionSnapshot, SessionStatus,
    },
    questionnaire::Question,
    server::operations::SessionReadRefusal,
    session_projection::agent_reading::{
        self, DEFAULT_MAX_CHARS, DEFAULT_TURNS, Detail, EntryNumber, Position, ReadRefusal,
        ReadRequest, SessionReading, Window,
    },
};

pub(super) const DESCRIPTION: &str = "\
Read one Session, on this Suru server or a Remote, as compact text: how it \
stands, what in it waits on the user, and what its Transcript says. Any \
Session may be read — the user's, a Subagent's, another Sidekick's, or your \
own — and reading changes nothing. Takes \"session_id\", the Session's id as \
list_sessions or a Subagent row gives it, and \"origin\", the name of the \
Remote it lives on as its row gives it, left out for a Session on this \
server; a Subagent's Session lives on the same server as the Session it \
works beneath. By default it answers with the latest Turn's user \
Message and the Agent's final Message in it, showing at most 2000 characters \
of what they say, counted back from the end. Optional: \"turns\", how many \
Turns to read back, 1 unless given; \"max_chars\", the most characters of \
what the Turns say to show, counted back from their end: 2000 unless given, \
and at least 1; \"before\", a point to read \
back from — the \"before\" an answer gave, to read on, or a Turn number such \
as \"4\" for the Turns before Turn 4; \"detail\": \"messages\", the default, \
for the user Messages and Delegations that asked for each Turn's work and the \
Agent's final Message in it, or \"activities\" for every Message and one line \
for each Command, File Change, Tool Call, Subagent and other Activity, without \
its output; and \"item\", one Message or Activity as the transcript numbers \
it, such as \"4.7\", to read it whole, its output included — given with \
\"session_id\" and \"origin\" alone. Reasoning is never returned. \
\"max_chars\" counts only what the Session's Messages and Activities say — \
never the headings, numbers and labels the transcript sets around them — so \
however small it is, a read shows something and the read after it moves on. \
Answers with JSON of the shape {\"session_id\", \"origin\", \"title\", \
\"workspace\", \"parent\", \"begun_by\", \"status\", \"standing\", \
\"questionnaires\", \"approvals\", \"subagent_interventions\", \
\"transcript\", \"before\", \"earlier\"}: \"origin\" is left out for a \
Session on this server; \"parent\" is the Session a Subagent's Session works \
beneath, or null; \
\"begun_by\" names the Sidekick that began it, where it is a Subsession, as \
\"Sidekick\" with its Title and its Session, or null; \"status\" is \"active\" while the \
Session works or owes a Turn to a Prompt, and \"idle\" otherwise; \"standing\" \
is as list_sessions gives it; \"questionnaires\" lists each Questionnaire \
waiting on an Answer, with its \"id\", its \"item\" and its \"questions\", \
each with its \"id\", \"text\", \"choices\" (each with an \"id\" and \
\"label\"), whether it takes \"multiple\" choices, \"freeform\" text, and \
choices with free text beside them (\"combine_freeform\"), and whether it is \
\"required\"; \
\"approvals\" says of each Approval waiting on the user's Decision what it \
asks — you cannot decide one, so tell the user; and \"subagent_interventions\" \
names each Subagent's Session beneath it that waits on the user. In \
\"transcript\" a line in brackets heads each Turn with its number, how it \
stands, and what the detail left out of it, and each Message or Activity \
begins with its number, such as \"4.7 agent:\"; a user Message a Sidekick \
sent on the user's behalf, rather than the user, is \"sent by Sidekick\", \
named by its Title and its Session; a Questionnaire a Sidekick answered is \
\"answered by Sidekick\", named alike; and a Session a Sidekick began is a \
\"subsession\" line naming that Session, its Title and what it was first \
asked. A line ending in … was \
shortened to one line, and one whose text begins […] lost its start to \
\"max_chars\": read either whole with \"item\". Whenever anything before the \
transcript was left out, \"earlier\" says what and \"before\" is the point to \
pass back to read on; both are null once the transcript reaches the Session's \
start. A Session an answer names by its id alone — \"parent\", \"begun_by\", \
a Sidekick that sent a Message or answered a Questionnaire, a Subagent's or a \
Subsession's line, \"subagent_interventions\" — is on the same server as the \
Session read and is reached with the same \"origin\", while one it places on \
another server is named with the \"origin\" that reaches it from here, or \
none for this server, or said not to be reachable through this read, since a \
Remote's names for the servers it reaches are its own and never an \
\"origin\" of yours — not even where the Session's own Agent wrote or passed \
one. A session_id naming no Session at its origin is refused, and so is an \
\"origin\" naming a Remote this server is not paired with or one that does \
not answer, saying why; so is a Remote's Session too large to be read across \
the Pairing, however little the read asks of it, since it is fetched whole \
first — it is read on that Remote itself.";

/// The JSON Schema of `read_session`'s arguments.
pub(super) fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "session_id": {
                "type": "string",
                "description": "The id of the Session to read, as list_sessions or a Subagent \
                    row gives it.",
            },
            "origin": origins::origin_property(),
            "turns": {
                "type": "integer",
                "minimum": 1,
                "description": "How many Turns to read back: 1 unless given.",
            },
            "max_chars": {
                "type": "integer",
                "minimum": 1,
                "description": "The most characters of what the Turns' Messages and \
                    Activities say to show, counted back from their end: 2000 unless given.",
            },
            "before": {
                "type": "string",
                "description": "A point to read back from: the `before` an answer gave, or a \
                    Turn number such as \"4\" for the Turns before Turn 4.",
            },
            "detail": {
                "type": "string",
                "enum": Detail::NAMES,
                "description": "\"messages\" (the default) for what asked for each Turn's work \
                    and the Agent's final Message; \"activities\" adds every Message and a \
                    line for each Activity.",
            },
            "item": {
                "type": "string",
                "description": "One Message or Activity as the transcript numbers it, such as \
                    \"4.7\", to read whole; given with session_id and origin alone.",
            },
        },
        "required": ["session_id"],
        "additionalProperties": false,
    })
}

/// What `read_session` was called with, each argument checked for the shape
/// its schema gives it.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct ReadArguments {
    session_id: SessionId,
    origin: Outlook,
    request: ReadRequest,
}

impl ReadArguments {
    const TAKES: [&'static str; 7] = [
        "session_id",
        "origin",
        "turns",
        "max_chars",
        "before",
        "detail",
        "item",
    ];

    /// What a read of one entry whole takes nothing of.
    const WINDOW: [&'static str; 4] = ["turns", "max_chars", "before", "detail"];

    pub(super) fn read(arguments: &Map<String, Value>) -> Result<Self, ToolRefusal> {
        takes_only(BrokerTool::ReadSession, arguments, &Self::TAKES)?;
        let given = |argument: &str| arguments.get(argument).filter(|value| !value.is_null());
        let session_id = match given("session_id") {
            None => {
                return Err(ToolRefusal::new(
                    "read_session needs `session_id`, the id of the Session to read, as \
                     list_sessions or a Subagent row gives it.",
                ));
            }
            Some(id) => serde_json::from_value::<SessionId>(id.clone()).map_err(|_| {
                ToolRefusal::new(format!(
                    "read_session's `session_id` must be a Session's id, as list_sessions or a \
                     Subagent row gives it; {id} is not one."
                ))
            })?,
        };
        let origin = origins::origin(BrokerTool::ReadSession, arguments)?;
        if let Some(item) = given("item") {
            if let Some(window) = Self::WINDOW
                .into_iter()
                .find(|argument| given(argument).is_some())
            {
                return Err(ToolRefusal::new(format!(
                    "read_session's `item` reads one entry whole, so it takes no `{window}` \
                     beside it; call it with `session_id`, `origin` and `item` alone."
                )));
            }
            let number = item
                .as_str()
                .and_then(|spelled| spelled.parse::<EntryNumber>().ok())
                .ok_or_else(|| {
                    ToolRefusal::new(format!(
                        "read_session's `item` must name one Message or Activity as the \
                         transcript numbers it, such as \"4.7\"; {item} is not one."
                    ))
                })?;
            return Ok(Self {
                session_id,
                origin,
                request: ReadRequest::Entry(number),
            });
        }
        let turns = match given("turns") {
            None => DEFAULT_TURNS,
            Some(turns) => counted(turns).ok_or_else(|| {
                ToolRefusal::new(format!(
                    "read_session's `turns` must be a whole number of Turns, at least 1; {turns} \
                     is not. Leave it out for {DEFAULT_TURNS}."
                ))
            })?,
        };
        let max_chars = match given("max_chars") {
            None => DEFAULT_MAX_CHARS,
            Some(chars) => counted(chars).ok_or_else(|| {
                ToolRefusal::new(format!(
                    "read_session's `max_chars` must be a whole number of characters, at least \
                     1; {chars} is not. Leave it out for {DEFAULT_MAX_CHARS}."
                ))
            })?,
        };
        let before = match given("before") {
            None => None,
            Some(point) => Some(position(point).ok_or_else(|| {
                ToolRefusal::new(format!(
                    "read_session's `before` must be a point an answer gave as `before`, such as \
                     \"4.7.120\", or a Turn number, such as \"4\"; {point} is neither."
                ))
            })?),
        };
        let detail = match given("detail") {
            None => Detail::default(),
            Some(detail) => detail.as_str().and_then(Detail::named).ok_or_else(|| {
                ToolRefusal::new(format!(
                    "read_session's `detail` must be {}; {detail} is neither.",
                    listed(Detail::NAMES.into_iter()).replace(", ", " or ")
                ))
            })?,
        };
        Ok(Self {
            session_id,
            origin,
            request: ReadRequest::Window(Window {
                turns,
                max_chars,
                before,
                detail,
            }),
        })
    }
}

/// A whole number of at least 1, and more than could be counted read as the
/// most there could be.
fn counted(value: &Value) -> Option<usize> {
    value
        .as_u64()
        .filter(|number| *number > 0)
        .map(|number| usize::try_from(number).unwrap_or(usize::MAX))
}

/// The point `value` names: one spelled as an answer spells it, or a Turn's
/// number given as a number.
fn position(value: &Value) -> Option<Position> {
    match value {
        Value::String(spelled) => spelled.parse().ok(),
        Value::Number(turn) => turn.as_u64()?.to_string().parse().ok(),
        _ => None,
    }
}

/// Why a read naming something the Session does not hold was refused, saying
/// which argument named it.
fn refusal(refusal: ReadRefusal, request: &ReadRequest) -> ToolRefusal {
    let argument = match request {
        ReadRequest::Window(_) => "before",
        ReadRequest::Entry(_) => "item",
    };
    ToolRefusal::new(match refusal {
        ReadRefusal::NoSuchTurn { turn, turns: 0 } => format!(
            "read_session's `{argument}` names Turn {turn}, but the Session has no Turns yet."
        ),
        ReadRefusal::NoSuchTurn { turn, turns } => format!(
            "read_session's `{argument}` names Turn {turn}, but the Session has {turns} {}; name \
             one from 1 to {turns}.",
            if turns == 1 { "Turn" } else { "Turns" }
        ),
        ReadRefusal::NoSuchEntry { entry, entries } => format!(
            "read_session's `{argument}` names {entry}, but Turn {turn} holds {entries} {}.",
            if entries == 1 { "entry" } else { "entries" },
            turn = entry.turn(),
        ),
    })
}

/// What `read_session` answers.
#[derive(Debug, Serialize)]
struct SessionReadout<'a> {
    session_id: SessionId,
    /// The Remote the Session lives on, and nothing for one on this Server.
    #[serde(skip_serializing_if = "Option::is_none")]
    origin: Option<String>,
    title: &'a str,
    /// The path of the Workspace the Session works in — its presented root.
    workspace: String,
    /// The Session a Subagent's Session works beneath.
    parent: Option<SessionId>,
    /// The Sidekick that began a Subsession, named as the transcript names a
    /// Sidekick that sent a Message.
    begun_by: Option<String>,
    status: SessionStatus,
    standing: Option<&'static str>,
    questionnaires: Vec<QuestionnaireReadout>,
    approvals: Vec<String>,
    subagent_interventions: Vec<String>,
    transcript: String,
    before: Option<String>,
    earlier: Option<String>,
}

/// A Questionnaire waiting on an Answer, as a read gives it: its identity,
/// which an Answer names, its entry's number, and its Questions whole.
#[derive(Debug, Serialize)]
struct QuestionnaireReadout {
    id: QuestionnaireId,
    item: String,
    questions: Vec<Question>,
}

impl BrokerTools {
    /// Answers `read_session`: the Session the call names, at its Origin —
    /// brought into memory there where it was not — read as the call asks.
    pub(super) async fn read_session(&self, call: ToolCall) -> Result<Value, ToolRefusal> {
        let arguments = ReadArguments::read(&call.arguments)?;
        let session_id = arguments.session_id;
        let read = self
            .operations
            .session_at(&arguments.origin, session_id)
            .await
            .map_err(|refusal| read_refusal(refusal, &arguments.origin, session_id))?;
        let standing = SessionListItem::Readable(Box::new(read.summary))
            .standing()
            .map(standing_name);
        let at = self.operations.reading_at(&arguments.origin);
        let reading = agent_reading::read(&read.snapshot, &arguments.request, &at)
            .map_err(|refused| refusal(refused, &arguments.request))?;
        Ok(serde_json::to_value(readout(
            origins::row_origin(arguments.origin),
            &read.snapshot,
            standing,
            reading,
        ))
        .expect("a reading of a Session always serializes"))
    }
}

/// Why a read of `session_id` at `origin` was refused, in words the Sidekick
/// can act on.
fn read_refusal(
    refusal: SessionReadRefusal,
    origin: &Outlook,
    session_id: SessionId,
) -> ToolRefusal {
    let at = match origin {
        Outlook::Local => "on this server".to_owned(),
        Outlook::Remote(name) => format!("on the Remote `{name}`"),
    };
    match refusal {
        SessionReadRefusal::Origin(refusal) => match (origin, refusal.budget_past()) {
            (Outlook::Remote(name), Some(budget)) => too_large(session_id, name, &budget),
            _ => origins::origin_refusal(refusal, &format!("Session `{session_id}` was not read.")),
        },
        SessionReadRefusal::Unloadable => ToolRefusal::new(format!(
            "Suru could not load Session `{session_id}` from its storage, so it cannot be read \
             now."
        )),
        SessionReadRefusal::Unreadable => ToolRefusal::new(format!(
            "Suru holds Session `{session_id}` {at} but could not read what it stored of it, so \
             there is nothing to read."
        )),
        SessionReadRefusal::NotFound => ToolRefusal::new(format!(
            "Suru holds no Session `{session_id}` {at}; pass a session_id, and the origin beside \
             it, as list_sessions or a Subagent row gives them."
        )),
    }
}

/// What a read of the Session `session_id` on the Remote `name` is refused
/// with where that Session runs past `budget`, the most this server reads of
/// one answer from a Remote. Every read of a Remote's Session fetches it
/// whole before taking what was asked of it, so none reads it from here,
/// however little it asks; where it is read is its own Server.
fn too_large(session_id: SessionId, name: &str, budget: &str) -> ToolRefusal {
    ToolRefusal::new(format!(
        "Session `{session_id}` on the Remote `{name}` is too large to be read across the \
         Pairing: it runs past the {budget} this server reads of one answer from a Remote. Every \
         read of a Remote's Session fetches the whole Session before taking what was asked of \
         it, so no narrower read — fewer \"turns\", a smaller \"max_chars\", an earlier \
         \"before\" or one \"item\" — reads it from here either. It can be read on `{name}` \
         itself, where the user can open it, as they can from a Client turned toward that \
         Remote."
    ))
}

fn readout<'a>(
    origin: Option<String>,
    snapshot: &'a SessionSnapshot,
    standing: Option<&'static str>,
    reading: SessionReading,
) -> SessionReadout<'a> {
    SessionReadout {
        session_id: snapshot.session.id,
        origin,
        title: &snapshot.title,
        workspace: snapshot
            .session
            .workspace
            .path
            .to_string_lossy()
            .into_owned(),
        parent: snapshot.session.parent,
        begun_by: reading.begun_by,
        status: snapshot.session.status,
        standing,
        questionnaires: reading
            .questionnaires
            .into_iter()
            .map(|open| QuestionnaireReadout {
                id: open.questionnaire.id,
                item: open.entry.to_string(),
                questions: open.questionnaire.questions,
            })
            .collect(),
        approvals: reading.approvals,
        subagent_interventions: reading.subagent_interventions,
        transcript: reading.transcript,
        before: reading.before.map(|before| before.to_string()),
        earlier: reading.earlier,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(arguments: Value) -> Result<ReadArguments, ToolRefusal> {
        let Value::Object(arguments) = arguments else {
            panic!("arguments are an object");
        };
        ReadArguments::read(&arguments)
    }

    fn refusal_of(called: Value) -> String {
        arguments(called).expect_err("refused").to_string()
    }

    #[test]
    fn a_session_id_alone_asks_for_the_latest_turn_at_the_default_cap() {
        let session_id = SessionId::new();
        assert_eq!(
            arguments(json!({ "session_id": session_id })),
            Ok(ReadArguments {
                session_id,
                origin: Outlook::Local,
                request: ReadRequest::Window(Window::default()),
            })
        );
        assert_eq!(
            Window::default().turns,
            1,
            "the description promises one Turn"
        );
        assert_eq!(
            Window::default().max_chars,
            2_000,
            "the description promises 2000 characters"
        );
        assert_eq!(Window::default().detail, Detail::Messages);
    }

    #[test]
    fn every_argument_is_read_as_its_schema_gives_it() {
        let session_id = SessionId::new();
        assert_eq!(
            arguments(json!({
                "session_id": session_id,
                "turns": 3,
                "max_chars": 9_000,
                "before": "4.7.120",
                "detail": "activities",
            })),
            Ok(ReadArguments {
                session_id,
                origin: Outlook::Local,
                request: ReadRequest::Window(Window {
                    turns: 3,
                    max_chars: 9_000,
                    before: Some("4.7.120".parse().expect("a point")),
                    detail: Detail::Activities,
                }),
            })
        );
        assert_eq!(
            arguments(json!({ "session_id": session_id, "max_chars": 1 })).map(|read| read.request),
            Ok(ReadRequest::Window(Window {
                max_chars: 1,
                ..Window::default()
            })),
            "any cap of at least a character is honoured"
        );
        assert_eq!(
            arguments(json!({ "session_id": session_id, "before": 4 })).map(|read| read.request),
            arguments(json!({ "session_id": session_id, "before": "4" })).map(|read| read.request),
            "a Turn's number may be given as a number"
        );
        assert_eq!(
            arguments(json!({ "session_id": session_id, "item": "4.7" })),
            Ok(ReadArguments {
                session_id,
                origin: Outlook::Local,
                request: ReadRequest::Entry("4.7".parse().expect("a number")),
            })
        );
        assert_eq!(
            arguments(json!({
                "session_id": session_id,
                "origin": "workstation",
                "item": "4.7",
            }))
            .map(|read| read.origin),
            Ok(Outlook::Remote("workstation".to_owned())),
            "an entry is read whole at the Session's own Origin"
        );
    }

    #[test]
    fn what_a_read_does_not_take_is_refused_saying_what_it_takes_instead() {
        let session_id = SessionId::new();
        for (called, says) in [
            (json!({}), "needs `session_id`"),
            (
                json!({ "session_id": "nonsense" }),
                "must be a Session's id",
            ),
            (
                json!({ "session_id": session_id, "colour": "red" }),
                "takes no argument `colour`",
            ),
            (
                json!({ "session_id": session_id, "turns": 0 }),
                "at least 1",
            ),
            (
                json!({ "session_id": session_id, "turns": "two" }),
                "at least 1",
            ),
            (
                json!({ "session_id": session_id, "max_chars": 0 }),
                "at least 1",
            ),
            (
                json!({ "session_id": session_id, "detail": "everything" }),
                "`messages` or `activities`",
            ),
            (
                json!({ "session_id": session_id, "before": "yesterday" }),
                "a Turn number",
            ),
            (
                json!({ "session_id": session_id, "item": "4" }),
                "such as \"4.7\"",
            ),
            (
                json!({ "session_id": session_id, "item": "4.7", "turns": 2 }),
                "takes no `turns` beside it",
            ),
        ] {
            let refusal = refusal_of(called.clone());
            assert!(
                refusal.contains(says),
                "{called} is refused saying {says:?}: {refusal}"
            );
        }
    }

    #[test]
    fn the_schema_takes_what_the_arguments_read() {
        let schema = input_schema();
        let mut properties = schema["properties"]
            .as_object()
            .expect("the schema names its properties")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        properties.sort_unstable();
        let mut takes = ReadArguments::TAKES.to_vec();
        takes.sort_unstable();
        assert_eq!(properties, takes);
        assert_eq!(schema["required"], json!(["session_id"]));
        assert_eq!(schema["properties"]["max_chars"]["minimum"], json!(1));
    }

    #[test]
    fn the_description_names_every_argument_and_detail_a_read_takes() {
        for name in ReadArguments::TAKES.into_iter().chain(Detail::NAMES) {
            assert!(
                DESCRIPTION.contains(&format!("\"{name}\"")),
                "read_session's description names {name}"
            );
        }
        assert!(DESCRIPTION.contains(&DEFAULT_MAX_CHARS.to_string()));
        assert!(
            DESCRIPTION.contains(
                "\"max_chars\" counts only what the Session's Messages and Activities say"
            ),
            "the description says what the cap counts"
        );
        assert!(DESCRIPTION.contains("Reasoning is never returned"));
        assert!(DESCRIPTION.contains("\"sent by Sidekick\""));
        assert!(DESCRIPTION.contains("\"begun_by\"") && DESCRIPTION.contains("\"subsession\""));
        assert!(DESCRIPTION.contains("\"answered by Sidekick\""));
        assert!(
            DESCRIPTION.contains(
                "A Session an answer names by its id alone — \"parent\", \"begun_by\", a \
                 Sidekick that sent a Message or answered a Questionnaire, a Subagent's or a \
                 Subsession's line, \"subagent_interventions\" — is on the same server as the \
                 Session read and is reached with the same \"origin\", while one it places on \
                 another server is named with the \"origin\" that reaches it from here, or none \
                 for this server, or said not to be reachable through this read"
            ),
            "the description says which Sessions a read names inherit its origin, and which do \
             not"
        );
    }
}
