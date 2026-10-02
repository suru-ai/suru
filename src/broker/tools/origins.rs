//! `origin`: which Server a Sidekick's Tool reaches, and `list_remotes`,
//! which names the Servers there are to reach besides its own.
//!
//! A Session's or a Workspace's identity is unique only within its Origin, so
//! every row a Tool answers with from a Remote names that Remote as its
//! `origin`, and a Tool that takes a Session takes the `origin` its row gave
//! beside its identity. Neither says anything for this server's own, the
//! ordinary case, which stays quiet. A listing's `origin` may also be
//! `everywhere`, ranging over this server and every Remote it is paired with.
//!
//! Every Tool reaches a Remote through this Server alone, by the operations
//! that read and act at an Origin ([`SessionOperations::sessions_in`],
//! [`SessionOperations::admit_prompt_at`] and their siblings), never by
//! speaking to the Remote or its Broker (ADR 0044). A Tool takes an `origin`
//! only where it says it does ([`BrokerTool::takes_origin`]), and reads it
//! here alone, which refuses one to any other: what a Sidekick does to a
//! Session or a Workspace it may do on a Remote, and nothing else crosses —
//! no Setting, and no Memory, which are each Server's own.
//!
//! [`SessionOperations::sessions_in`]: crate::server::operations::SessionOperations::sessions_in
//! [`SessionOperations::admit_prompt_at`]: crate::server::operations::SessionOperations::admit_prompt_at

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::{BrokerTool, BrokerTools, ToolCall, ToolRefusal, takes_no_arguments};
use crate::{
    protocol::{EVERYWHERE, Outlook, names_everywhere},
    server::operations::{
        ActRefusal, OriginRefusal, Origins, Refusal, RemoteActRefusal, SilentRemote,
    },
};

pub(super) const LIST_REMOTES_DESCRIPTION: &str = "\
List the Remotes this Suru server is paired with — the user's other \
machines running Suru, whose Sessions and Workspaces you reach through this \
server — and whether each answers now. Each is asked at once, as this call is \
made, so the answer is never what a Remote last said. Takes no arguments. \
Answers with JSON of the shape {\"remotes\": [remote, ...]}, in the order \
they were paired: each has \"name\", the name to pass as \"origin\" to \
list_sessions, read_session and list_workspaces, and \"answers\", true when \
it answered; one that did not also has \"reason\", saying why in words you \
can pass on. Nothing can be read from a Remote that does not answer until it \
does. An empty list means this server is paired with no Remote.";

/// The JSON Schema of `origin` for a Tool reaching one Server, saying what
/// at that Server the Tool reaches.
pub(super) fn origin_property() -> Value {
    json!({
        "type": "string",
        "description": "The name of the Remote the Session lives on, as its row gives it as \
            `origin`; leave it out for a Session on this server.",
    })
}

/// The JSON Schema of a listing's `origin`, `listed` saying what it lists.
pub(super) fn origins_property(listed: &str) -> Value {
    json!({
        "type": "string",
        "description": format!(
            "A Remote's name, as list_remotes gives it, to list the {listed} there; \
             \"{EVERYWHERE}\" to list this server's and every Remote's; leave it out for this \
             server's alone."
        ),
    })
}

/// The Server a Tool reaching one Server is called for: the Remote its
/// `origin` names, or this server where it names none. `everywhere`, in any
/// case, names every Server at once and so no one Server, and no Remote may
/// be named so: it is refused.
pub(super) fn origin(
    tool: BrokerTool,
    arguments: &Map<String, Value>,
) -> Result<Outlook, ToolRefusal> {
    match named_origin(tool, arguments)? {
        None => Ok(Outlook::Local),
        Some(name) if names_everywhere(&name) => Err(ToolRefusal::new(format!(
            "{}'s `origin` names the one server a Session lives on, and `{EVERYWHERE}` names no \
             one server; pass the `origin` the Session's row gave, or leave it out for a Session \
             on this server.",
            tool.name()
        ))),
        Some(name) => Ok(Outlook::Remote(name)),
    }
}

/// The Servers a listing is called for: the Remote its `origin` names,
/// every Server for `everywhere` in any case, or this server where it names
/// none.
pub(super) fn origins(
    tool: BrokerTool,
    arguments: &Map<String, Value>,
) -> Result<Origins, ToolRefusal> {
    Ok(match named_origin(tool, arguments)? {
        None => Origins::One(Outlook::Local),
        Some(name) if names_everywhere(&name) => Origins::Everywhere,
        Some(name) => Origins::One(Outlook::Remote(name)),
    })
}

/// The name a call's `origin` gives, where it gives one. A Remote's name has
/// nothing around it, so what is around a name given is no part of it, and
/// a name with nothing in it names this server, as leaving it out does. A
/// Tool that takes no `origin` is refused one, whatever else it is given.
fn named_origin(
    tool: BrokerTool,
    arguments: &Map<String, Value>,
) -> Result<Option<String>, ToolRefusal> {
    if !tool.takes_origin() {
        return Err(ToolRefusal::new(format!(
            "{} takes no `origin`: it reaches this Suru server alone, since a Remote shares its \
             Sessions and Workspaces and nothing else.",
            tool.name()
        )));
    }
    match arguments.get("origin") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(name)) => {
            let name = name.trim();
            Ok((!name.is_empty()).then(|| name.to_owned()))
        }
        Some(_) => Err(ToolRefusal::new(format!(
            "{}'s `origin` must be a string: the name of a Remote, as list_remotes gives it, or \
             nothing for this server.",
            tool.name()
        ))),
    }
}

/// What a Tool is told of an Origin it could not read: why, and then
/// `not_read`, a sentence saying what went unread.
pub(super) fn origin_refusal(refusal: OriginRefusal, not_read: &str) -> ToolRefusal {
    ToolRefusal::new(match refusal {
        OriginRefusal::UnknownRemote(name) => unknown_remote(&name),
        OriginRefusal::Silent(silent) => format!("{silent} {not_read}"),
    })
}

/// What a Tool is told of an act at an Origin that was refused: in this
/// server's own words where it refused, and otherwise as the Remote refused
/// it, or why the Remote could not be asked — saying whether it may have been
/// done there all the same, and that nothing is kept to do later.
pub(super) fn act_refusal<R: std::fmt::Display>(refusal: ActRefusal<R>) -> ToolRefusal {
    match refusal {
        ActRefusal::Here(refusal) => ToolRefusal::new(refusal.to_string()),
        ActRefusal::There(refusal) => remote_act_refusal(refusal),
    }
}

/// What a Tool is told of an act a Remote refused, or could not be asked.
pub(super) fn remote_act_refusal(refusal: RemoteActRefusal) -> ToolRefusal {
    ToolRefusal::new(match refusal {
        RemoteActRefusal::Origin(OriginRefusal::UnknownRemote(name)) => unknown_remote(&name),
        RemoteActRefusal::Origin(OriginRefusal::Silent(silent)) if silent.may_have_acted() => {
            format!(
                "{silent} Whether it was done there is not known, and nothing is kept to do once \
                 it answers; read what it holds then to see."
            )
        }
        RemoteActRefusal::Origin(OriginRefusal::Silent(silent)) => {
            format!("{silent} Nothing was done there, and nothing is kept to do once it answers.")
        }
        RemoteActRefusal::Refused {
            remote,
            reason: Refusal::Said(reason),
        } => format!("The Remote `{remote}` refused it: {reason}"),
        RemoteActRefusal::Refused {
            remote,
            reason: Refusal::Failed(reason),
        } => format!("The Remote `{remote}` could not do it: {reason}."),
    })
}

/// What a Tool naming a Remote this server is not paired with is told.
pub(super) fn unknown_remote(name: &str) -> String {
    format!(
        "Suru is paired with no Remote named `{name}`; list_remotes names the Remotes it is \
         paired with, and leaving `origin` out reaches this server."
    )
}

/// A Remote that did not answer a listing ranging Everywhere, as the listing
/// names it.
#[derive(Debug, Serialize)]
pub(super) struct Unanswered {
    origin: String,
    reason: String,
}

impl From<SilentRemote> for Unanswered {
    fn from(silent: SilentRemote) -> Self {
        Self {
            reason: silent.to_string(),
            origin: silent.name,
        }
    }
}

/// The name a row from `origin` carries as its `origin`: a Remote's, and
/// none for this server's own.
pub(super) fn row_origin(origin: Outlook) -> Option<String> {
    match origin {
        Outlook::Local => None,
        Outlook::Remote(name) => Some(name),
    }
}

/// What `list_remotes` answers.
#[derive(Debug, Serialize)]
struct RemoteListing {
    remotes: Vec<ListedRemote>,
}

#[derive(Debug, Serialize)]
struct ListedRemote {
    name: String,
    answers: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

impl BrokerTools {
    /// Answers `list_remotes`: every Remote this server is paired with, and
    /// whether each answers now.
    pub(super) async fn list_remotes(&self, call: &ToolCall) -> Result<Value, ToolRefusal> {
        takes_no_arguments(BrokerTool::ListRemotes, &call.arguments)?;
        let remotes = self
            .operations
            .remote_answers()
            .await
            .into_iter()
            .map(|(name, answer)| ListedRemote {
                name,
                answers: answer.is_ok(),
                reason: answer.err().map(|silent| silent.to_string()),
            })
            .collect();
        Ok(serde_json::to_value(RemoteListing { remotes })
            .expect("a listing of Remotes always serializes"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(arguments: Value) -> Map<String, Value> {
        let Value::Object(arguments) = arguments else {
            panic!("arguments are an object");
        };
        arguments
    }

    #[test]
    fn an_origin_names_a_remote_and_none_names_this_server() {
        let read = |called| origin(BrokerTool::ReadSession, &arguments(called));
        assert_eq!(read(json!({})), Ok(Outlook::Local));
        assert_eq!(read(json!({ "origin": null })), Ok(Outlook::Local));
        assert_eq!(read(json!({ "origin": "  " })), Ok(Outlook::Local));
        assert_eq!(
            read(json!({ "origin": " workstation " })),
            Ok(Outlook::Remote("workstation".to_owned())),
            "no Remote's name has anything around it"
        );
        for everywhere in ["everywhere", "Everywhere"] {
            let refusal = read(json!({ "origin": everywhere })).expect_err("refused");
            assert!(
                refusal
                    .to_string()
                    .contains("`everywhere` names no one server"),
                "a Tool reaching one Server is refused every Server: {refusal}"
            );
        }
        let refusal = read(json!({ "origin": 7 })).expect_err("refused");
        assert!(
            refusal
                .to_string()
                .starts_with("read_session's `origin` must be a string"),
            "{refusal}"
        );
    }

    #[test]
    fn a_listings_origin_may_range_everywhere() {
        let read = |called| origins(BrokerTool::ListSessions, &arguments(called));
        assert_eq!(read(json!({})), Ok(Origins::One(Outlook::Local)));
        assert_eq!(
            read(json!({ "origin": "everywhere" })),
            Ok(Origins::Everywhere)
        );
        assert_eq!(
            read(json!({ "origin": "EVERYWHERE" })),
            Ok(Origins::Everywhere),
            "in any case, since no Remote may be named so"
        );
        assert_eq!(
            read(json!({ "origin": "workstation" })),
            Ok(Origins::One(Outlook::Remote("workstation".to_owned())))
        );
    }

    #[test]
    fn a_listings_origin_says_everywhere_is_the_word_for_every_server() {
        assert!(
            origins_property("Sessions")["description"]
                .as_str()
                .is_some_and(|described| described.contains("\"everywhere\"")),
        );
        assert!(LIST_REMOTES_DESCRIPTION.contains("\"answers\""));
        assert!(LIST_REMOTES_DESCRIPTION.contains("\"reason\""));
    }
}
