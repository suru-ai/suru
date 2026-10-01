//! `begin_session`: the Tool through which a Sidekick begins a Session on its
//! own Server on the user's behalf — a Subsession.
//!
//! It begins the Session exactly as the Landing does, through the same
//! operations a Client's requests perform: the directory resolved to its
//! Workspace, a new Managed Worktree prepared first where one is asked for,
//! the Agent Selection the Landing would begin with where none is chosen,
//! and the Title derived from the first Prompt. What makes it a Subsession is
//! its author: the Session remembers the Sidekick's Session that began it,
//! its first Message names the Sidekick, and the Sidekick's Transcript gains
//! the row leading into it — which stands in for this Tool's call, so no
//! Provider records that call as a Tool Call besides. No Sidekick begins a
//! Session in the Sidekick Workspace (ADR 0043).
//!
//! The Agent is settled before anything is made: the one the Sidekick chose,
//! or the one the user's Landing would begin with, held alike to what
//! `list_providers` offers, so a Session is never begun on an Agent Suru
//! already knows cannot run it. A Provider that may be chosen and then fails
//! to start fails in the Session, as it would for the Landing.
//!
//! A new Worktree is prepared under an identity of its own, as the Landing
//! keeps one for each submission: a beginning that fails once its Worktree is
//! made keeps the Worktree and names its preparation, and the Sidekick passing
//! that name back begins in the same Worktree rather than another. The name
//! only means that preparation to the Sidekick it was given to, since the
//! identity is derived from the name and the Sidekick's own Session together.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use super::{
    BrokerTool, BrokerTools, Choosable, ToolCall, ToolRefusal, choosable, requested_selection,
    takes_only,
};
use crate::{
    protocol::{
        AgentSelection, CreateSessionRequest, ExecutionDirectory, InitialPrompt, ModelCatalog,
        PreparationId, PreparationPrompt, PrepareCheckoutRequest, PromptId, SessionId,
    },
    server::operations::PreparationRefusal,
    sessions::StoreOutcome,
};

/// What a Sidekick is told when the user's Landing has no Agent to begin a
/// Session with.
const NO_AGENT: &str = "No Provider Suru hosts can begin a Session now: each is turned off or \
    cannot be used. Ask the user to turn one on, or call list_providers to see why.";

/// What a refusal of the Landing's own Agent goes on to say the Sidekick may
/// do instead.
const CHOOSE_ANOTHER: &str =
    "Choose another Agent with `agent_selection`; list_providers says which may be chosen.";

pub(super) const DESCRIPTION: &str = "\
Begin a Session on this Suru server on the user's behalf, as the user would \
from the Landing: it works where you say, on its own, and stays the user's \
like any other Session — they may prompt, interrupt or delete it, and nothing \
it does keeps you working. Its Transcript shows its first Prompt as sent by \
you, leading back to your Session, and yours gains a row leading into it. \
Takes \"directory\", the absolute path of the directory it is to work in, \
such as a Workspace's path as list_sessions gives it; \"prompt\", everything \
its Agent needs, since it sees none of your conversation; optionally \
\"agent_selection\", the Agent to run it, shaped {\"provider\": \"...\", \
\"model\": \"...\", \"options\": {...}} with ids as list_providers gives them \
and any Model Option left out taking the Model's default — left out, it \
begins with the Agent the user's Landing would; and optionally \
\"new_worktree\": true to begin it in a new Worktree of the directory's \
Repository, which Suru creates and names from the prompt, rather than in the \
directory itself. A beginning that fails once its new Worktree is made keeps \
the Worktree and its refusal names a \"preparation\"; pass that back, with \
the same \"directory\", to begin the Session in the kept Worktree rather \
than another. Answers with JSON of the shape {\"session_id\": \"...\", \
\"directory\": \"...\", \"provider\": \"...\", \"model\": \"...\"}: the new \
Session's id, where it works, and the Agent it began with. A directory that \
does not exist, an Agent that cannot be chosen, and a directory of the \
Sidekick Workspace, where no Sidekick begins a Session, are refused saying \
why; so is a bare Repository's root, where no Session can work, unless \
\"new_worktree\" is true.";

/// The JSON Schema of `begin_session`'s arguments.
pub(super) fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "directory": {
                "type": "string",
                "description": "The absolute path of the directory the Session is to work in.",
            },
            "prompt": {
                "type": "string",
                "description": "Everything the Session's Agent needs to do what you ask; it \
                    sees none of your conversation.",
            },
            "agent_selection": {
                "type": "object",
                "description": "The Agent to run the Session, as list_providers gives its \
                    ids. Left out, the Session begins with the Agent the user's Landing would.",
                "properties": {
                    "provider": { "type": "string" },
                    "model": { "type": "string" },
                    "options": {
                        "type": "object",
                        "description": "Model Option id to value: a choice id for a select \
                            option, true or false for a toggle. Options left out take the \
                            Model's defaults.",
                        "additionalProperties": { "type": ["string", "boolean"] },
                    },
                },
                "required": ["provider", "model"],
                "additionalProperties": false,
            },
            "preparation": {
                "type": "string",
                "description": "The preparation a refused begin_session named, to begin the \
                    Session in the new Worktree it kept.",
            },
            "new_worktree": {
                "type": "boolean",
                "description": "true to begin the Session in a new Worktree of the \
                    directory's Repository, which Suru creates for it.",
            },
        },
        "required": BeginArguments::REQUIRED,
        "additionalProperties": false,
    })
}

impl BrokerTools {
    /// Answers `begin_session`: begins a Subsession as the Landing begins a
    /// Session — in a new Managed Worktree prepared first, where one is asked
    /// for — authored by the calling Sidekick, and says where it works and on
    /// which Agent.
    pub(super) async fn begin_session(&self, call: ToolCall) -> Result<Value, ToolRefusal> {
        let begin = BeginArguments::read(&call.arguments)?;
        let catalog = self.model_catalog.known().await;
        let selection = match &begin.agent_selection {
            Some(chosen) => {
                requested_selection(&catalog, &chosen.provider, &chosen.model, &chosen.options)?
            }
            None => landing_agent(&catalog, self.operations.landing_selection())?,
        };
        let author = self.sidekick_author(&call);
        let prompt = InitialPrompt {
            id: PromptId::new(),
            text: begin.prompt,
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
        };
        let mut execution_directory = ExecutionDirectory {
            path: begin.directory,
        };
        let mut preparation = None;
        if begin.new_worktree || begin.preparation.is_some() {
            // A fresh beginning names its preparation afresh; a retry names
            // the one its failed attempt was told of.
            let named = begin
                .preparation
                .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
            let prepared = self
                .operations
                .prepare_worktree(
                    PrepareCheckoutRequest {
                        id: preparation_for(call.caller.session_id(), &named),
                        source: execution_directory,
                        prompt: PreparationPrompt {
                            text: prompt.text.clone(),
                            skill_invocations: Vec::new(),
                            attachments: Vec::new(),
                        },
                        // The Worktree's Skills are read for the Provider
                        // the Session will run on.
                        provider: selection.provider.clone(),
                    },
                    Some(&author),
                )
                .await
                .map_err(|refusal| match refusal {
                    PreparationRefusal::SidekickWorkspace => ToolRefusal::new(refusal.to_string()),
                    PreparationRefusal::Invalid(reason) => ToolRefusal::new(format!(
                        "No new Worktree was made, so no Session was begun: {reason}."
                    )),
                })?;
            if let Some(error) = prepared.error {
                return Err(kept_worktree_refusal(
                    &format!("the new Worktree is not ready: {error}"),
                    &named,
                ));
            }
            execution_directory = prepared.preparation.destination;
            preparation = Some((prepared.preparation.id, named));
        }
        let begun = match self
            .operations
            .begin_session(
                CreateSessionRequest {
                    preparation_id: preparation.as_ref().map(|(id, _)| *id),
                    agent_selection: Some(selection),
                    execution_directory,
                    prompt,
                },
                Some(author),
            )
            .await
            .map_err(|refusal| match &preparation {
                Some((_, named)) => kept_worktree_refusal(&refusal.to_string(), named),
                None => ToolRefusal::new(refusal.to_string()),
            })? {
            StoreOutcome::Created(snapshot) | StoreOutcome::Existing(snapshot) => snapshot,
        };
        let agent = begun.session.agent_selection.as_ref();
        Ok(json!({
            "session_id": begun.session.id,
            "directory": begun.session.execution_directory.path,
            "provider": agent.map(|selection| selection.provider.as_str()),
            "model": agent.map(|selection| selection.model.as_str()),
        }))
    }
}

/// The Agent the user's Landing would begin a Session with, `landing`, held
/// to what `list_providers` offers as a chosen Agent is: its Provider hosted,
/// turned on and usable now, and its Model one that Provider runs. A
/// refusal says what is wrong with it and what the Sidekick may do instead.
fn landing_agent(
    catalog: &ModelCatalog,
    landing: Option<AgentSelection>,
) -> Result<AgentSelection, ToolRefusal> {
    let selection = landing.ok_or_else(|| ToolRefusal::new(NO_AGENT))?;
    let provider = selection.provider.as_str();
    let unusable = |why: String| {
        ToolRefusal::new(format!(
            "The user's Landing would begin the Session with Provider `{provider}`, which {why} \
             {CHOOSE_ANOTHER}"
        ))
    };
    let Some(hosted) = catalog
        .providers
        .iter()
        .find(|hosted| hosted.provider == selection.provider)
    else {
        return Err(unusable("Suru does not host.".to_owned()));
    };
    match choosable(hosted) {
        Choosable::Disabled => Err(unusable("is turned off in Suru.".to_owned())),
        Choosable::Unavailable { detail, .. } => {
            Err(unusable(format!("cannot be used now: {detail}")))
        }
        Choosable::Models(mut models) => {
            if models.any(|model| model.id == selection.model) {
                Ok(selection)
            } else {
                Err(ToolRefusal::new(format!(
                    "The user's Landing would begin the Session with Model `{}`, which Provider \
                     `{provider}` does not offer now. {CHOOSE_ANOTHER}",
                    selection.model.as_str()
                )))
            }
        }
    }
}

/// What a Sidekick is told of a beginning that failed, `why`, once its new
/// Worktree was made: the Worktree is kept, and passing back `named` begins
/// the Session in it.
fn kept_worktree_refusal(why: &str, named: &str) -> ToolRefusal {
    let why = why.trim_end_matches('.');
    ToolRefusal::new(format!(
        "No Session was begun, because {why}. The new Worktree is kept: call begin_session \
         again with the same `directory` and \"preparation\": \"{named}\" to begin the \
         Session in it."
    ))
}

/// The identity of the Worktree preparation the Sidekick of `sidekick` named
/// `named`: the same each time that Sidekick passes the same name, so a retry
/// rejoins what a failed attempt kept, and its own, so the name means nothing
/// to any other Sidekick.
fn preparation_for(sidekick: SessionId, named: &str) -> PreparationId {
    let mut hasher = blake3::Hasher::new();
    for part in [
        "suru begin_session preparation".as_bytes(),
        sidekick.to_string().as_bytes(),
        named.as_bytes(),
    ] {
        hasher.update(&(part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    PreparationId(uuid::Builder::from_random_bytes(bytes).into_uuid())
}

/// The Agent a call of `begin_session` chose to run the Session, as it named
/// it, before it is checked against what may be chosen.
#[derive(Debug, Eq, PartialEq)]
struct ChosenAgent {
    provider: String,
    model: String,
    options: Map<String, Value>,
}

/// What `begin_session` was called with: where the Session is to work, what
/// it is first asked, the Agent chosen to run it where one was, whether it is
/// to work in a new Worktree, and the preparation of one a failed attempt kept,
/// where it names one.
#[derive(Debug, Eq, PartialEq)]
struct BeginArguments {
    directory: PathBuf,
    prompt: String,
    agent_selection: Option<ChosenAgent>,
    new_worktree: bool,
    preparation: Option<String>,
}

impl BeginArguments {
    /// Everything a call may name.
    const TAKES: [&'static str; 5] = [
        "directory",
        "prompt",
        "agent_selection",
        "new_worktree",
        "preparation",
    ];
    /// What a call must name: the Agent and the Worktree may be left to the
    /// Landing's defaults.
    const REQUIRED: [&'static str; 2] = ["directory", "prompt"];
    /// Everything an Agent Selection may name.
    const SELECTION_TAKES: [&'static str; 3] = ["provider", "model", "options"];

    fn read(arguments: &Map<String, Value>) -> Result<Self, ToolRefusal> {
        takes_only(BrokerTool::BeginSession, arguments, &Self::TAKES)?;
        let directory = match arguments.get("directory") {
            Some(Value::String(directory)) => PathBuf::from(directory),
            None | Some(Value::Null) => {
                return Err(ToolRefusal::new(
                    "begin_session needs `directory`, the absolute path of the directory the \
                     Session is to work in.",
                ));
            }
            Some(_) => {
                return Err(ToolRefusal::new(
                    "begin_session's `directory` must be a path, as a string.",
                ));
            }
        };
        if !directory.is_absolute() {
            return Err(ToolRefusal::new(format!(
                "begin_session's `directory` must be an absolute path; {} is not one.",
                directory.display()
            )));
        }
        if !is_directory(&directory) {
            return Err(ToolRefusal::new(format!(
                "There is no directory {} on this Suru server for a Session to work in.",
                directory.display()
            )));
        }
        let prompt = match arguments.get("prompt") {
            Some(Value::String(prompt)) => prompt.clone(),
            None | Some(Value::Null) => {
                return Err(ToolRefusal::new(
                    "begin_session needs `prompt`, what the Session's Agent is to do.",
                ));
            }
            Some(_) => {
                return Err(ToolRefusal::new(
                    "begin_session's `prompt` must be a string.",
                ));
            }
        };
        if prompt.trim().is_empty() {
            return Err(ToolRefusal::new(
                "begin_session's `prompt` is empty; say what the Session's Agent is to do.",
            ));
        }
        let agent_selection = match arguments.get("agent_selection") {
            None | Some(Value::Null) => None,
            Some(Value::Object(selection)) => Some(Self::chosen_agent(selection)?),
            Some(_) => {
                return Err(ToolRefusal::new(
                    "begin_session's `agent_selection` must be an object naming `provider` and \
                     `model`, as list_providers gives them.",
                ));
            }
        };
        let new_worktree = match arguments.get("new_worktree") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(new_worktree)) => *new_worktree,
            Some(other) => {
                return Err(ToolRefusal::new(format!(
                    "begin_session's `new_worktree` must be true or false; {other} is neither."
                )));
            }
        };
        // A name Suru gave: thirty-two hexadecimal digits, as a refusal
        // spelled it.
        let preparation = match arguments.get("preparation") {
            None | Some(Value::Null) => None,
            Some(Value::String(named))
                if named.len() == 32 && named.bytes().all(|digit| digit.is_ascii_hexdigit()) =>
            {
                Some(named.to_ascii_lowercase())
            }
            Some(other) => {
                return Err(ToolRefusal::new(format!(
                    "begin_session's `preparation` must be one a refused begin_session named; \
                     {other} is not one."
                )));
            }
        };
        Ok(Self {
            directory,
            prompt,
            agent_selection,
            new_worktree,
            preparation,
        })
    }

    /// The Agent `selection` names, having refused anything else in it.
    fn chosen_agent(selection: &Map<String, Value>) -> Result<ChosenAgent, ToolRefusal> {
        if let Some(unknown) = selection
            .keys()
            .find(|named| !Self::SELECTION_TAKES.contains(&named.as_str()))
        {
            return Err(ToolRefusal::new(format!(
                "begin_session's `agent_selection` takes no `{unknown}`; it takes `provider`, \
                 `model` and `options`."
            )));
        }
        let text = |named: &str| match selection.get(named) {
            Some(Value::String(text)) => Ok(text.clone()),
            None | Some(Value::Null) => Err(ToolRefusal::new(format!(
                "begin_session's `agent_selection` needs `{named}`, as list_providers gives it."
            ))),
            Some(_) => Err(ToolRefusal::new(format!(
                "begin_session's `agent_selection` `{named}` must be a string."
            ))),
        };
        let options = match selection.get("options") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(options)) => options.clone(),
            Some(_) => {
                return Err(ToolRefusal::new(
                    "begin_session's `agent_selection` `options` must be an object from Model \
                     Option id to value.",
                ));
            }
        };
        Ok(ChosenAgent {
            provider: text("provider")?,
            model: text("model")?,
            options,
        })
    }
}

/// Whether `path` names a directory, through any symlink, on this Server.
fn is_directory(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_dir())
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

    /// A path no platform reads as absolute, for the refusal of one.
    const RELATIVE: &str = "work/suru";

    #[test]
    fn begin_arguments_leave_the_agent_and_the_worktree_to_the_landing_unless_named() {
        let directory = tempfile::tempdir().expect("create a directory to work in");
        let path = directory.path().to_str().expect("the directory is UTF-8");
        assert_eq!(
            BeginArguments::read(&arguments(json!({
                "directory": path,
                "prompt": "Fix the flaky login test",
            }))),
            Ok(BeginArguments {
                directory: directory.path().to_owned(),
                prompt: "Fix the flaky login test".to_owned(),
                agent_selection: None,
                new_worktree: false,
                preparation: None,
            })
        );
        assert_eq!(
            BeginArguments::read(&arguments(json!({
                "directory": path,
                "prompt": "Fix the flaky login test",
                "agent_selection": {
                    "provider": "codex",
                    "model": "gpt-5",
                    "options": { "effort": "high" },
                },
                "new_worktree": true,
            }))),
            Ok(BeginArguments {
                directory: directory.path().to_owned(),
                prompt: "Fix the flaky login test".to_owned(),
                agent_selection: Some(ChosenAgent {
                    provider: "codex".to_owned(),
                    model: "gpt-5".to_owned(),
                    options: arguments(json!({ "effort": "high" })),
                }),
                new_worktree: true,
                preparation: None,
            })
        );
    }

    #[test]
    fn begin_arguments_are_refused_in_words_the_sidekick_can_act_on() {
        let directory = tempfile::tempdir().expect("create a directory to work in");
        let path = directory.path().to_str().expect("the directory is UTF-8");
        let missing = directory.path().join("gone");
        for (sent, says) in [
            (
                json!({ "prompt": "Go" }),
                "begin_session needs `directory`, the absolute path of the directory the \
                 Session is to work in."
                    .to_owned(),
            ),
            (
                json!({ "directory": 7, "prompt": "Go" }),
                "begin_session's `directory` must be a path, as a string.".to_owned(),
            ),
            (
                json!({ "directory": RELATIVE, "prompt": "Go" }),
                format!(
                    "begin_session's `directory` must be an absolute path; {} is not one.",
                    Path::new(RELATIVE).display()
                ),
            ),
            (
                json!({ "directory": missing, "prompt": "Go" }),
                format!(
                    "There is no directory {} on this Suru server for a Session to work in.",
                    missing.display()
                ),
            ),
            (
                json!({ "directory": path }),
                "begin_session needs `prompt`, what the Session's Agent is to do.".to_owned(),
            ),
            (
                json!({ "directory": path, "prompt": "  \n" }),
                "begin_session's `prompt` is empty; say what the Session's Agent is to do."
                    .to_owned(),
            ),
            (
                json!({ "directory": path, "prompt": "Go", "agent_selection": "codex" }),
                "begin_session's `agent_selection` must be an object naming `provider` and \
                 `model`, as list_providers gives them."
                    .to_owned(),
            ),
            (
                json!({
                    "directory": path,
                    "prompt": "Go",
                    "agent_selection": { "provider": "codex" },
                }),
                "begin_session's `agent_selection` needs `model`, as list_providers gives it."
                    .to_owned(),
            ),
            (
                json!({
                    "directory": path,
                    "prompt": "Go",
                    "agent_selection": { "provider": "codex", "model": "gpt-5", "effort": "high" },
                }),
                "begin_session's `agent_selection` takes no `effort`; it takes `provider`, \
                 `model` and `options`."
                    .to_owned(),
            ),
            (
                json!({ "directory": path, "prompt": "Go", "new_worktree": "yes" }),
                "begin_session's `new_worktree` must be true or false; \"yes\" is neither."
                    .to_owned(),
            ),
            (
                json!({ "directory": path, "prompt": "Go", "origin": "studio" }),
                "begin_session takes no argument `origin`; it takes `directory`, `prompt`, \
                 `agent_selection`, `new_worktree`, `preparation`."
                    .to_owned(),
            ),
            (
                json!({ "directory": path, "prompt": "Go", "preparation": "the-first-one" }),
                "begin_session's `preparation` must be one a refused begin_session named; \
                 \"the-first-one\" is not one."
                    .to_owned(),
            ),
        ] {
            assert_eq!(
                BeginArguments::read(&arguments(sent.clone())),
                Err(ToolRefusal::new(says)),
                "{sent}"
            );
        }
    }

    #[test]
    fn a_named_preparation_is_the_same_one_only_to_the_sidekick_it_was_named_to() {
        let named = "0123456789abcdef0123456789abcdef";
        let (sidekick, another) = (SessionId::new(), SessionId::new());
        assert_eq!(
            preparation_for(sidekick, named),
            preparation_for(sidekick, named),
            "a retry passing the name back rejoins the preparation it names"
        );
        assert_ne!(
            preparation_for(sidekick, named),
            preparation_for(another, named),
            "the name means nothing to another Sidekick"
        );
        assert_ne!(
            preparation_for(sidekick, named),
            preparation_for(sidekick, "fedcba9876543210fedcba9876543210")
        );
        let directory = tempfile::tempdir().expect("create a directory to work in");
        assert_eq!(
            BeginArguments::read(&arguments(json!({
                "directory": directory.path(),
                "prompt": "Go",
                "preparation": named.to_ascii_uppercase(),
            })))
            .map(|begin| begin.preparation),
            Ok(Some(named.to_owned())),
            "a name is read as the refusal spelled it, whatever its case"
        );
    }

    #[test]
    fn begin_sessions_schema_requires_what_its_description_says_it_takes() {
        let schema = input_schema();
        assert_eq!(schema["required"], json!(["directory", "prompt"]));
        assert_eq!(
            schema["properties"]["agent_selection"]["required"],
            json!(["provider", "model"])
        );
        for argument in BeginArguments::TAKES {
            assert!(
                schema["properties"].get(argument).is_some()
                    && DESCRIPTION.contains(&format!("\"{argument}\"")),
                "{argument} is in the schema and the description"
            );
        }
        for answered in [
            "\"session_id\"",
            "\"directory\"",
            "\"provider\"",
            "\"model\"",
        ] {
            assert!(DESCRIPTION.contains(answered), "{answered}");
        }
        assert!(DESCRIPTION.contains("Sidekick Workspace"));
        assert!(DESCRIPTION.contains("bare Repository's root"));
    }
}
