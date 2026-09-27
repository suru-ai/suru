//! The Tools the Broker offers, each described once and answered in Suru's
//! own terms. Nothing here knows MCP: the transport in [`super::mcp`] turns a
//! [`BrokerTool`]'s description into what `tools/list` lists and routes a
//! `tools/call` into [`BrokerTools::call`].

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::BrokerCaller;
use crate::{
    model_catalog::ModelCatalogService,
    protocol::{
        AgentSelection, ModelAvailability, ModelCatalog, ModelDescriptor, ModelId,
        ModelOptionChoiceId, ModelOptionDescriptor, ModelOptionId, ModelOptionKind,
        ModelOptionValue, ProviderCatalogStatus, ProviderId, ProviderModelCatalog,
        ProviderUnavailability, SessionId, TurnStatus,
    },
    provider::{BrokeredStop, BrokeredSubagentRequest, ProviderOrchestrator},
    sessions::{BrokeredReadError, BrokeredSubagentReading, SessionStore},
};

/// One Tool the Broker offers. A new Tool is a variant here, its description
/// beside the others, and an arm of [`BrokerTools::call`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BrokerTool {
    ListProviders,
    SpawnSubagent,
    ReadSubagent,
    StopSubagent,
}

impl BrokerTool {
    /// Every Tool, in the order `tools/list` lists them.
    pub(super) const ALL: [Self; 4] = [
        Self::ListProviders,
        Self::SpawnSubagent,
        Self::ReadSubagent,
        Self::StopSubagent,
    ];

    pub(super) fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tool| tool.name() == name)
    }

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::ListProviders => "list_providers",
            Self::SpawnSubagent => "spawn_subagent",
            Self::ReadSubagent => "read_subagent",
            Self::StopSubagent => "stop_subagent",
        }
    }

    pub(super) fn title(self) -> &'static str {
        match self {
            Self::ListProviders => "List Providers",
            Self::SpawnSubagent => "Spawn Subagent",
            Self::ReadSubagent => "Read Subagent",
            Self::StopSubagent => "Stop Subagent",
        }
    }

    /// What the calling Agent reads about the Tool: when to use it, what it
    /// takes, and the shape of what it answers — which is stable, so an Agent
    /// may rely on it.
    pub(super) fn description(self) -> &'static str {
        match self {
            Self::ListProviders => LIST_PROVIDERS_DESCRIPTION,
            Self::SpawnSubagent => SPAWN_SUBAGENT_DESCRIPTION,
            Self::ReadSubagent => READ_SUBAGENT_DESCRIPTION,
            Self::StopSubagent => STOP_SUBAGENT_DESCRIPTION,
        }
    }

    /// The JSON Schema of the Tool's arguments.
    pub(super) fn input_schema(self) -> Map<String, Value> {
        let schema = match self {
            Self::ListProviders => json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }),
            Self::SpawnSubagent => json!({
                "type": "object",
                "properties": {
                    "provider": {
                        "type": "string",
                        "description": "The id of the Provider to run the Subagent on, as \
                            list_providers gives it.",
                    },
                    "model": {
                        "type": "string",
                        "description": "The id of one of that Provider's Models, as \
                            list_providers gives it.",
                    },
                    "options": {
                        "type": "object",
                        "description": "Model Option id to value: a choice id for a select \
                            option, true or false for a toggle. Options left out take the \
                            Model's defaults.",
                        "additionalProperties": { "type": ["string", "boolean"] },
                    },
                    "name": {
                        "type": "string",
                        "description": "A short name for the Subagent, such as the kind of \
                            work it does.",
                    },
                    "description": {
                        "type": "string",
                        "description": "A few words on what the Subagent is asked to do.",
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Everything the Subagent needs to do the work; it sees \
                            none of your conversation.",
                    },
                },
                "required": ["provider", "model", "name", "description", "prompt"],
                "additionalProperties": false,
            }),
            Self::ReadSubagent | Self::StopSubagent => json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "The session_id spawn_subagent answered with.",
                    },
                },
                "required": ["id"],
                "additionalProperties": false,
            }),
        };
        let Value::Object(schema) = schema else {
            unreachable!("every input schema is a JSON object");
        };
        schema
    }

    /// Whether the Tool only reads, changing nothing Suru holds.
    pub(super) fn is_read_only(self) -> bool {
        match self {
            Self::ListProviders | Self::ReadSubagent => true,
            Self::SpawnSubagent | Self::StopSubagent => false,
        }
    }
}

const LIST_PROVIDERS_DESCRIPTION: &str = "\
List the Providers Suru hosts, with the Models and Model Options each offers, \
so you can choose where to delegate work. Takes no arguments. Answers with \
JSON of the shape {\"providers\": [provider, ...]}, one entry per hosted \
Provider in Suru's order. Each provider has \"id\"; \"name\"; \"enabled\" \
(false when the user has turned the Provider off in Suru); \"available\" (true \
when it can be used now, false when it cannot, null when it is turned off and \
so was never checked); \"reason\" and \"detail\" when \"available\" is false, \
where reason is one of \"not_installed\", \"not_signed_in\", \
\"incompatible_version\", \"catalog_failed\" or \"checking\" and detail says \
what to tell the user; and \"models\", empty unless the Provider is available. \
Each model has \"id\", \"name\", \"description\", \"default\" (true for the \
Model the Provider uses unless told otherwise) and \"options\". Each Model \
Option has \"id\", \"name\" and \"type\": a \"select\" option has \"choices\" \
(each with \"id\" and \"name\") and the \"default\" choice id; a \"toggle\" \
option has a boolean \"default\". Only Models and choices that can be selected \
are listed. Availability is as Suru last found it; calling again does not \
re-check a Provider.";

const SPAWN_SUBAGENT_DESCRIPTION: &str = "\
Spawn a Subagent on any Provider Suru hosts, your own or another, to do a \
piece of work for you; it answers at once while the Subagent works on its own. \
Call list_providers first to learn which Providers, Models and Model Options \
may be chosen. Takes \"provider\" and \"model\", ids as list_providers gives \
them; \"options\", an object from Model Option id to a choice id for a \
\"select\" option or true or false for a \"toggle\" option, where any option \
left out takes the Model's default; \"name\", a short name for the Subagent, \
such as the kind of work it does; \"description\", a few words on what it is \
asked to do, which titles its Session; and \"prompt\", everything it needs to \
do the work, since it sees none of your conversation. By default the Subagent \
works in the same directory and checkout as you. Answers with JSON of the \
shape {\"session_id\": \"...\"}, the id of the Subagent's own Session. The \
Subagent stands as a row in your Transcript while it works and once it \
settles. A Provider that is turned off or cannot be used now, a Model it does \
not offer, or an option value the Model does not take is refused, saying what \
was wrong.";

const READ_SUBAGENT_DESCRIPTION: &str = "\
Read how a Subagent spawned with spawn_subagent is doing — one you spawned, or \
one a Subagent beneath you spawned: whether it still works, how long it has \
worked, and what it last wrote. Takes \"id\", the session_id spawn_subagent \
answered with. Answers with JSON of the shape {\"session_id\": \"...\", \
\"status\": \"...\", \"duration_ms\": ..., \"message\": ...}, describing \
the Subagent's latest stretch of work: \"status\" is \"working\" while it \
works, and once it settles \"completed\", \"failed\" or \"stopped\"; \
\"duration_ms\" is how long that stretch has worked so far, or worked in all \
once settled, and null where Suru never learned when it ended; and \
\"message\" is the latest Message the Subagent wrote in that stretch, in full \
— its final answer once settled — or null when it has written none. An id \
naming no Subagent spawned with spawn_subagent by you or by a Subagent beneath \
you is refused.";

const STOP_SUBAGENT_DESCRIPTION: &str = "\
Stop a Subagent spawned with spawn_subagent — by you, or by a Subagent beneath \
you — while it works: it stops at once, along with anything it delegated in \
turn, asking no one. Takes \"id\", the Subagent's Session id as spawn_subagent \
answered with it. Answers with JSON of the shape {\"session_id\": \"...\", \
\"stopped\": true} when its own work was stopped, once its Provider has been \
told to stop; its row in your Transcript then settles as stopped. When its own \
work had already settled it answers {\"session_id\": \"...\", \
\"stopped\": false, \"reason\": \"...\"}, the reason saying what, if \
anything, the stop did instead — Subagents it delegated to that still worked, \
or Watches it left running, are stopped, and its row stays as it settled. An \
id that names no Subagent spawned through the Broker beneath you is refused.";

/// One call of a Tool: who is calling, and the arguments as the Agent sent
/// them.
pub(super) struct ToolCall {
    pub(super) caller: BrokerCaller,
    pub(super) arguments: Map<String, Value>,
}

/// Why a Tool refused a call, in words the calling Agent reads.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct ToolRefusal(String);

impl std::fmt::Display for ToolRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl ToolRefusal {
    fn new(reason: impl Into<String>) -> Self {
        Self(reason.into())
    }
}

/// The Server-side services the Broker's Tools answer from.
#[derive(Clone)]
pub(crate) struct BrokerTools {
    model_catalog: ModelCatalogService,
    providers: ProviderOrchestrator,
    sessions: SessionStore,
}

impl BrokerTools {
    pub(crate) fn new(
        model_catalog: ModelCatalogService,
        providers: ProviderOrchestrator,
        sessions: SessionStore,
    ) -> Self {
        Self {
            model_catalog,
            providers,
            sessions,
        }
    }

    /// Answers one call of `tool` with the JSON its description promises.
    pub(super) async fn call(
        &self,
        tool: BrokerTool,
        call: ToolCall,
    ) -> Result<Value, ToolRefusal> {
        tracing::debug!(
            tool = tool.name(),
            session_id = %call.caller.session_id(),
            "Broker Tool called"
        );
        match tool {
            BrokerTool::ListProviders => {
                takes_no_arguments(tool, &call.arguments)?;
                Ok(
                    serde_json::to_value(provider_listing(self.model_catalog.known().await))
                        .expect("a Provider listing always serializes"),
                )
            }
            BrokerTool::SpawnSubagent => {
                let spawn = SpawnArguments::read(&call.arguments)?;
                let selection = requested_selection(
                    &self.model_catalog.known().await,
                    &spawn.provider,
                    &spawn.model,
                    &spawn.options,
                )?;
                let session_id = self
                    .providers
                    .spawn_brokered_subagent(
                        call.caller.session_id(),
                        BrokeredSubagentRequest {
                            selection,
                            name: spawn.name,
                            description: spawn.description,
                            delegation: spawn.prompt,
                        },
                    )
                    .map_err(ToolRefusal)?;
                Ok(json!({ "session_id": session_id }))
            }
            BrokerTool::ReadSubagent => self.read_subagent(call),
            BrokerTool::StopSubagent => {
                let subagent = StopArguments::read(&call.arguments)?.id;
                let stop = self
                    .providers
                    .stop_brokered_subagent(call.caller.session_id(), subagent)
                    .await
                    .map_err(ToolRefusal)?;
                Ok(stop_answer(subagent, stop))
            }
        }
    }

    /// Answers `read_subagent`: how the brokered Subagent the call names
    /// stands, read for the calling Agent, which may read only the brokered
    /// Subagents beneath it.
    fn read_subagent(&self, call: ToolCall) -> Result<Value, ToolRefusal> {
        let subagent = named_subagent(BrokerTool::ReadSubagent, &call.arguments, &["id"])?;
        let reading = self
            .sessions
            .read_brokered_subagent(call.caller.session_id(), subagent)
            .map_err(|error| read_refusal(error, subagent))?;
        Ok(serde_json::to_value(SubagentReadout::from(reading))
            .expect("a Subagent's reading always serializes"))
    }
}

/// The brokered Subagent a call names by its `id` argument — the Session id
/// `spawn_subagent` answered with — having refused any argument `tool` does
/// not take, as `takes` lists them.
fn named_subagent(
    tool: BrokerTool,
    arguments: &Map<String, Value>,
    takes: &[&str],
) -> Result<SessionId, ToolRefusal> {
    let name = tool.name();
    if let Some(unknown) = arguments
        .keys()
        .find(|argument| !takes.contains(&argument.as_str()))
    {
        return Err(ToolRefusal::new(format!(
            "{name} takes no argument `{unknown}`; it takes {}.",
            listed(takes.iter().copied())
        )));
    }
    match arguments.get("id") {
        None | Some(Value::Null) => Err(ToolRefusal::new(format!(
            "{name} needs `id`, the session_id spawn_subagent answered with."
        ))),
        Some(id) => serde_json::from_value(id.clone()).map_err(|_| {
            ToolRefusal::new(format!(
                "{name}'s `id` must be the session_id spawn_subagent answered with; {id} is \
                 not one."
            ))
        }),
    }
}

/// Why `read_subagent` could not read `subagent`, in words the calling Agent
/// reads.
fn read_refusal(error: BrokeredReadError, subagent: SessionId) -> ToolRefusal {
    ToolRefusal::new(match error {
        BrokeredReadError::CallerNotFound => {
            "The Session calling the Broker no longer exists on this Suru server.".to_owned()
        }
        BrokeredReadError::NoSuchSession => format!(
            "Suru holds no Session `{subagent}`; pass the session_id spawn_subagent answered with."
        ),
        BrokeredReadError::NotBrokeredBeneathCaller => format!(
            "`{subagent}` is not a Subagent spawned with spawn_subagent by you or by a Subagent \
             beneath you; read_subagent reads only those."
        ),
    })
}

/// What `read_subagent` answers.
#[derive(Debug, Serialize)]
struct SubagentReadout {
    session_id: SessionId,
    status: SubagentStatus,
    duration_ms: Option<u64>,
    message: Option<String>,
}

impl From<BrokeredSubagentReading> for SubagentReadout {
    fn from(reading: BrokeredSubagentReading) -> Self {
        Self {
            session_id: reading.session_id,
            status: SubagentStatus::from(reading.status),
            duration_ms: reading.duration_ms,
            message: reading.message,
        }
    }
}

/// How a Subagent's latest stretch of work stands, spelled as
/// `read_subagent`'s description spells it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SubagentStatus {
    Working,
    Completed,
    Failed,
    /// Interrupted: stopped rather than finishing or failing.
    Stopped,
}

impl From<TurnStatus> for SubagentStatus {
    fn from(status: TurnStatus) -> Self {
        match status {
            TurnStatus::Active => Self::Working,
            TurnStatus::Completed => Self::Completed,
            TurnStatus::Failed => Self::Failed,
            TurnStatus::Interrupted => Self::Stopped,
        }
    }
}

/// What `stop_subagent` answers for `subagent`: `stopped` only where its work
/// was stopped, and otherwise the reason, saying what the stop did instead.
fn stop_answer(subagent: SessionId, stop: BrokeredStop) -> Value {
    let reason = match stop {
        BrokeredStop::StoppedWork => {
            return json!({ "session_id": subagent, "stopped": true });
        }
        BrokeredStop::StoppedDelegatedWork => {
            "The Subagent's own work had already settled, and its row stays as it settled. The \
             Subagents it delegated to, which were still working, were stopped."
        }
        BrokeredStop::StoppedWatches => {
            "The Subagent was not working: its latest work had already settled, and its row \
             stays as it settled. Only the Watches it left running, which could have woken it, \
             were stopped."
        }
        BrokeredStop::WithdrewPrompt => {
            "The Subagent had not begun: the work waiting to begin it was withdrawn before it \
             started, so there was nothing running to stop."
        }
        BrokeredStop::NothingRunning => {
            "The Subagent was not working: its latest work had already settled, so there was \
             nothing to stop."
        }
    };
    json!({ "session_id": subagent, "stopped": false, "reason": reason })
}

/// What `stop_subagent` was called with: the one Subagent to stop.
#[derive(Debug, Eq, PartialEq)]
struct StopArguments {
    id: SessionId,
}

impl StopArguments {
    const TAKES: [&'static str; 1] = ["id"];

    fn read(arguments: &Map<String, Value>) -> Result<Self, ToolRefusal> {
        if let Some(unknown) = arguments
            .keys()
            .find(|argument| !Self::TAKES.contains(&argument.as_str()))
        {
            return Err(ToolRefusal::new(format!(
                "stop_subagent takes no argument `{unknown}`; it takes `id`."
            )));
        }
        let id = match arguments.get("id") {
            Some(Value::String(id)) => id,
            None | Some(Value::Null) => {
                return Err(ToolRefusal::new("stop_subagent needs `id`."));
            }
            Some(_) => return Err(ToolRefusal::new("stop_subagent's `id` must be a string.")),
        };
        let id = uuid::Uuid::parse_str(id.trim()).map_err(|_| {
            ToolRefusal::new(format!(
                "`{id}` is not a Session id; give stop_subagent the session_id spawn_subagent \
                 answered with."
            ))
        })?;
        Ok(Self {
            id: SessionId::from_uuid(id),
        })
    }
}

/// What `spawn_subagent` was called with, each argument checked for the shape
/// its schema gives it.
#[derive(Debug, Eq, PartialEq)]
struct SpawnArguments {
    provider: String,
    model: String,
    options: Map<String, Value>,
    name: String,
    description: String,
    prompt: String,
}

impl SpawnArguments {
    const TAKES: [&'static str; 6] = [
        "provider",
        "model",
        "options",
        "name",
        "description",
        "prompt",
    ];

    fn read(arguments: &Map<String, Value>) -> Result<Self, ToolRefusal> {
        if let Some(unknown) = arguments
            .keys()
            .find(|argument| !Self::TAKES.contains(&argument.as_str()))
        {
            return Err(ToolRefusal::new(format!(
                "spawn_subagent takes no argument `{unknown}`; it takes {}.",
                Self::TAKES
                    .map(|argument| format!("`{argument}`"))
                    .join(", ")
            )));
        }
        let text = |argument: &str| match arguments.get(argument) {
            Some(Value::String(text)) => Ok(text.clone()),
            None | Some(Value::Null) => Err(ToolRefusal::new(format!(
                "spawn_subagent needs `{argument}`."
            ))),
            Some(_) => Err(ToolRefusal::new(format!(
                "spawn_subagent's `{argument}` must be a string."
            ))),
        };
        let options = match arguments.get("options") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(options)) => options.clone(),
            Some(_) => {
                return Err(ToolRefusal::new(
                    "spawn_subagent's `options` must be an object from Model Option id to value.",
                ));
            }
        };
        let read = Self {
            provider: text("provider")?,
            model: text("model")?,
            options,
            name: text("name")?,
            description: text("description")?,
            prompt: text("prompt")?,
        };
        if read.name.trim().is_empty() {
            return Err(ToolRefusal::new(
                "spawn_subagent's `name` is empty; give the Subagent a short name.",
            ));
        }
        if read.prompt.trim().is_empty() {
            return Err(ToolRefusal::new(
                "spawn_subagent's `prompt` is empty; say what the Subagent is to do.",
            ));
        }
        Ok(read)
    }
}

/// The Agent Selection a spawn asked for, checked against the Model Catalog
/// as `list_providers` reads it: the Provider hosted, turned on, and usable
/// now; the Model one it offers and will run; and each Model Option named one
/// the Model has, set to a value it takes. Options left out take the Model's
/// defaults. A refusal names what was wrong and what may be chosen instead.
fn requested_selection(
    catalog: &ModelCatalog,
    provider: &str,
    model: &str,
    options: &Map<String, Value>,
) -> Result<AgentSelection, ToolRefusal> {
    let Some(hosted) = catalog
        .providers
        .iter()
        .find(|hosted| hosted.provider.as_str() == provider)
    else {
        return Err(ToolRefusal::new(format!(
            "Suru hosts no Provider `{provider}`; the Providers it hosts are {}. Call \
             list_providers to see which may be chosen.",
            listed(
                catalog
                    .providers
                    .iter()
                    .map(|hosted| hosted.provider.as_str())
            )
        )));
    };
    let mut models = match choosable(hosted) {
        Choosable::Disabled => {
            return Err(ToolRefusal::new(format!(
                "Provider `{provider}` is turned off in Suru; ask the user to turn it back on, or \
                 choose another Provider."
            )));
        }
        Choosable::Unavailable { detail, .. } => {
            return Err(ToolRefusal::new(format!(
                "Provider `{provider}` cannot be used now: {detail} Choose another Provider."
            )));
        }
        Choosable::Models(models) => models,
    };
    let Some(descriptor) = models.find(|descriptor| descriptor.id.as_str() == model) else {
        return Err(ToolRefusal::new(format!(
            "Provider `{provider}` offers no Model `{model}`; choose one of {}.",
            listed(choosable_models(hosted).map(|descriptor| descriptor.id.as_str()))
        )));
    };
    let mut selection = descriptor.default_agent_selection();
    for (option_id, value) in options {
        let Some(option) = descriptor
            .options
            .iter()
            .find(|option| option.id.as_str() == option_id)
        else {
            return Err(ToolRefusal::new(if descriptor.options.is_empty() {
                format!(
                    "Model `{model}` has no Model Options; call spawn_subagent without `{option_id}`."
                )
            } else {
                format!(
                    "Model `{model}` has no Model Option `{option_id}`; its Model Options are {}.",
                    listed(descriptor.options.iter().map(|option| option.id.as_str()))
                )
            }));
        };
        let chosen = option_value(option, value)?;
        if let Some(selected) = selection
            .options
            .iter_mut()
            .find(|selected| selected.id == option.id)
        {
            selected.value = chosen;
        }
    }
    Ok(selection)
}

/// The value `value` sets Model Option `option` to, if it is one the option
/// takes.
fn option_value(
    option: &ModelOptionDescriptor,
    value: &Value,
) -> Result<ModelOptionValue, ToolRefusal> {
    let id = &option.id;
    match (&option.kind, value) {
        (ModelOptionKind::Select { choices, .. }, value) => {
            let offered = || {
                listed(
                    choices
                        .iter()
                        .filter(|choice| choice.availability == ModelAvailability::Available)
                        .map(|choice| choice.id.as_str()),
                )
            };
            let Value::String(choice) = value else {
                return Err(ToolRefusal::new(format!(
                    "Model Option `{id}` takes a choice id as a string: one of {}.",
                    offered()
                )));
            };
            if !choices.iter().any(|offered| {
                offered.id.as_str() == choice
                    && offered.availability == ModelAvailability::Available
            }) {
                return Err(ToolRefusal::new(format!(
                    "Model Option `{id}` has no choice `{choice}`; choose one of {}.",
                    offered()
                )));
            }
            Ok(ModelOptionValue::Select {
                choice: ModelOptionChoiceId::new(choice.clone()),
            })
        }
        (ModelOptionKind::Toggle { .. }, Value::Bool(enabled)) => {
            Ok(ModelOptionValue::Toggle { enabled: *enabled })
        }
        (ModelOptionKind::Toggle { .. }, _) => Err(ToolRefusal::new(format!(
            "Model Option `{id}` is a toggle and takes true or false."
        ))),
    }
}

/// Ids as a refusal lists them: each quoted, in the order given.
fn listed<'a>(ids: impl Iterator<Item = &'a str>) -> String {
    let listed = ids.map(|id| format!("`{id}`")).collect::<Vec<_>>();
    if listed.is_empty() {
        "none".to_owned()
    } else {
        listed.join(", ")
    }
}

fn takes_no_arguments(tool: BrokerTool, arguments: &Map<String, Value>) -> Result<(), ToolRefusal> {
    if arguments.is_empty() {
        return Ok(());
    }
    let named = arguments.keys().cloned().collect::<Vec<_>>().join(", ");
    Err(ToolRefusal(format!(
        "{} takes no arguments; call it again without {named}.",
        tool.name()
    )))
}

/// What `list_providers` answers.
#[derive(Debug, Serialize)]
struct ProviderListing {
    providers: Vec<ListedProvider>,
}

#[derive(Debug, Serialize)]
struct ListedProvider {
    id: ProviderId,
    name: String,
    enabled: bool,
    /// `None` for a Provider the user turned off: Suru never checked it, so
    /// there is no Availability to report.
    available: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<UnavailableReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    models: Vec<ListedModel>,
}

/// Why a listed Provider cannot be used now: one of the conditions the
/// Tool's description names, spelled as it names them.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum UnavailableReason {
    NotInstalled,
    NotSignedIn,
    IncompatibleVersion,
    /// Suru could not learn the Provider's Models, for no reason it can name.
    CatalogFailed,
    /// Suru is still asking the Provider for its Models for the first time.
    Checking,
}

impl From<ProviderUnavailability> for UnavailableReason {
    fn from(reason: ProviderUnavailability) -> Self {
        match reason {
            ProviderUnavailability::NotInstalled => Self::NotInstalled,
            ProviderUnavailability::NotSignedIn => Self::NotSignedIn,
            ProviderUnavailability::IncompatibleVersion => Self::IncompatibleVersion,
        }
    }
}

#[derive(Debug, Serialize)]
struct ListedModel {
    id: ModelId,
    name: String,
    description: String,
    default: bool,
    options: Vec<ListedOption>,
}

#[derive(Debug, Serialize)]
struct ListedOption {
    id: ModelOptionId,
    name: String,
    #[serde(flatten)]
    kind: ListedOptionKind,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ListedOptionKind {
    Select {
        choices: Vec<ListedChoice>,
        default: ModelOptionChoiceId,
    },
    Toggle {
        default: bool,
    },
}

#[derive(Debug, Serialize)]
struct ListedChoice {
    id: ModelOptionChoiceId,
    name: String,
}

/// The Model Catalog as an Agent choosing where to delegate reads it: every
/// hosted Provider, whether the user has it on, whether it can be used now and
/// why not, and — only where it can — the Models and Model Option choices that
/// may be selected. A Model or choice the Provider lists but will not run is
/// left out, since naming it could only be refused.
fn provider_listing(catalog: ModelCatalog) -> ProviderListing {
    ProviderListing {
        providers: catalog.providers.into_iter().map(listed_provider).collect(),
    }
}

/// Whether an Agent may choose from a Provider's catalog now: the one reading
/// `list_providers` reports and `spawn_subagent` checks a spawn against, so
/// neither offers what the other refuses.
enum Choosable<'a> {
    /// The user has turned the Provider off, so Suru never checked it.
    Disabled,
    /// It cannot be used now, for the reason given, which says what to tell
    /// the user.
    Unavailable {
        reason: UnavailableReason,
        detail: String,
    },
    /// It can be used now, and these are the Models it will run.
    Models(Box<dyn Iterator<Item = &'a ModelDescriptor> + 'a>),
}

fn choosable(catalog: &ProviderModelCatalog) -> Choosable<'_> {
    match &catalog.status {
        ProviderCatalogStatus::Disabled => Choosable::Disabled,
        ProviderCatalogStatus::Unavailable { reason, message } => Choosable::Unavailable {
            reason: UnavailableReason::from(*reason),
            detail: message.clone(),
        },
        ProviderCatalogStatus::Failed { message } => Choosable::Unavailable {
            reason: UnavailableReason::CatalogFailed,
            detail: message.clone(),
        },
        // A first discovery still in flight has nothing to offer yet; a
        // re-check of Models already known leaves them selectable.
        ProviderCatalogStatus::Refreshing if catalog.models.is_empty() => Choosable::Unavailable {
            reason: UnavailableReason::Checking,
            detail: "Suru is still asking this Provider for its Models; ask again shortly."
                .to_owned(),
        },
        ProviderCatalogStatus::Fresh
        | ProviderCatalogStatus::Warning { .. }
        | ProviderCatalogStatus::Stale { .. }
        | ProviderCatalogStatus::Refreshing => {
            Choosable::Models(Box::new(choosable_models(catalog)))
        }
    }
}

/// The Models a usable Provider will run: a Model it lists but will not run
/// is left out, since naming it could only be refused.
fn choosable_models(catalog: &ProviderModelCatalog) -> impl Iterator<Item = &ModelDescriptor> {
    catalog
        .models
        .iter()
        .filter(|model| model.availability == ModelAvailability::Available)
}

fn listed_provider(catalog: ProviderModelCatalog) -> ListedProvider {
    let listed = |available, reason, detail, models| ListedProvider {
        id: catalog.provider.clone(),
        name: catalog.display_name.clone(),
        enabled: !matches!(catalog.status, ProviderCatalogStatus::Disabled),
        available,
        reason,
        detail,
        models,
    };
    match choosable(&catalog) {
        Choosable::Disabled => listed(None, None, None, Vec::new()),
        Choosable::Unavailable { reason, detail } => {
            listed(Some(false), Some(reason), Some(detail), Vec::new())
        }
        Choosable::Models(models) => {
            listed(Some(true), None, None, models.map(listed_model).collect())
        }
    }
}

fn listed_model(model: &ModelDescriptor) -> ListedModel {
    ListedModel {
        id: model.id.clone(),
        name: model.display_name.clone(),
        description: model.description.clone(),
        default: model.is_default,
        options: model.options.iter().map(listed_option).collect(),
    }
}

fn listed_option(option: &ModelOptionDescriptor) -> ListedOption {
    ListedOption {
        id: option.id.clone(),
        name: option.label.clone(),
        kind: match &option.kind {
            ModelOptionKind::Select { choices, default } => ListedOptionKind::Select {
                choices: choices
                    .iter()
                    .filter(|choice| choice.availability == ModelAvailability::Available)
                    .map(|choice| ListedChoice {
                        id: choice.id.clone(),
                        name: choice.label.clone(),
                    })
                    .collect(),
                default: default.clone(),
            },
            ModelOptionKind::Toggle { default } => ListedOptionKind::Toggle { default: *default },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ModelOptionChoice, ModelOptionRole};

    fn catalog(status: ProviderCatalogStatus, models: Vec<ModelDescriptor>) -> ModelCatalog {
        ModelCatalog {
            providers: vec![ProviderModelCatalog {
                provider: ProviderId::new("codex"),
                display_name: "Codex".to_owned(),
                models,
                status,
            }],
        }
    }

    fn listed(status: ProviderCatalogStatus, models: Vec<ModelDescriptor>) -> Value {
        serde_json::to_value(provider_listing(catalog(status, models)))
            .expect("a Provider listing serializes")["providers"][0]
            .clone()
    }

    fn model(id: &str, availability: ModelAvailability) -> ModelDescriptor {
        let choice = |id: &str, availability| ModelOptionChoice {
            id: ModelOptionChoiceId::new(id),
            label: id.to_uppercase(),
            description: Some("not listed".to_owned()),
            availability,
        };
        ModelDescriptor {
            provider: ProviderId::new("codex"),
            id: ModelId::new(id),
            display_name: id.to_uppercase(),
            description: String::new(),
            is_default: false,
            availability,
            options: vec![ModelOptionDescriptor {
                id: ModelOptionId::new("effort"),
                label: "Effort".to_owned(),
                description: Some("not listed".to_owned()),
                role: ModelOptionRole::ReasoningEffort,
                kind: ModelOptionKind::Select {
                    choices: vec![
                        choice("low", ModelAvailability::Available),
                        choice("max", ModelAvailability::Unavailable),
                    ],
                    default: ModelOptionChoiceId::new("low"),
                },
            }],
        }
    }

    #[test]
    fn only_the_models_and_choices_a_provider_will_run_are_offered() {
        let listed = listed(
            ProviderCatalogStatus::Fresh,
            vec![
                model("runs", ModelAvailability::Available),
                model("refused", ModelAvailability::Unavailable),
            ],
        );
        assert_eq!(
            listed["models"],
            json!([{
                "id": "runs",
                "name": "RUNS",
                "description": "",
                "default": false,
                "options": [{
                    "id": "effort",
                    "name": "Effort",
                    "type": "select",
                    "choices": [{ "id": "low", "name": "LOW" }],
                    "default": "low",
                }],
            }])
        );
    }

    #[test]
    fn a_provider_with_known_models_is_available_whatever_its_catalog_is_doing() {
        for status in [
            ProviderCatalogStatus::Fresh,
            ProviderCatalogStatus::Warning {
                message: "update soon".to_owned(),
            },
            ProviderCatalogStatus::Stale {
                message: "the last check failed".to_owned(),
            },
            ProviderCatalogStatus::Refreshing,
        ] {
            let listed = listed(
                status.clone(),
                vec![model("runs", ModelAvailability::Available)],
            );
            assert_eq!(listed["available"], json!(true), "{status:?}");
            assert_eq!(listed["enabled"], json!(true), "{status:?}");
            assert_eq!(listed["models"].as_array().map(Vec::len), Some(1));
            assert!(listed.get("reason").is_none(), "{status:?}");
        }
    }

    #[test]
    fn a_provider_that_cannot_be_used_says_why_and_offers_nothing() {
        let known = vec![model("runs", ModelAvailability::Available)];
        for (status, models, reason, detail) in [
            (
                ProviderCatalogStatus::Unavailable {
                    reason: ProviderUnavailability::NotInstalled,
                    message: "install the Codex CLI".to_owned(),
                },
                known.clone(),
                "not_installed",
                "install the Codex CLI",
            ),
            (
                ProviderCatalogStatus::Unavailable {
                    reason: ProviderUnavailability::IncompatibleVersion,
                    message: "upgrade the Codex CLI".to_owned(),
                },
                Vec::new(),
                "incompatible_version",
                "upgrade the Codex CLI",
            ),
            (
                ProviderCatalogStatus::Failed {
                    message: "model/list timed out".to_owned(),
                },
                Vec::new(),
                "catalog_failed",
                "model/list timed out",
            ),
        ] {
            let listed = listed(status.clone(), models);
            assert_eq!(listed["available"], json!(false), "{status:?}");
            assert_eq!(listed["reason"], json!(reason), "{status:?}");
            assert_eq!(listed["detail"], json!(detail), "{status:?}");
            assert_eq!(listed["models"], json!([]), "{status:?}");
        }

        let checking = listed(ProviderCatalogStatus::Refreshing, Vec::new());
        assert_eq!(checking["available"], json!(false));
        assert_eq!(checking["reason"], json!("checking"));
    }

    #[test]
    fn the_description_names_every_reason_a_provider_may_be_unavailable_for() {
        for reason in [
            UnavailableReason::NotInstalled,
            UnavailableReason::NotSignedIn,
            UnavailableReason::IncompatibleVersion,
            UnavailableReason::CatalogFailed,
            UnavailableReason::Checking,
        ] {
            let spelled = serde_json::to_value(reason)
                .expect("a reason serializes")
                .to_string();
            assert!(
                LIST_PROVIDERS_DESCRIPTION.contains(&spelled),
                "list_providers' description names {spelled}"
            );
        }
    }

    #[test]
    fn a_disabled_provider_reports_no_availability_because_it_was_never_checked() {
        assert_eq!(
            listed(ProviderCatalogStatus::Disabled, Vec::new()),
            json!({
                "id": "codex",
                "name": "Codex",
                "enabled": false,
                "available": null,
                "models": [],
            })
        );
    }

    #[test]
    fn every_tool_is_found_by_the_name_it_is_listed_under() {
        for tool in BrokerTool::ALL {
            assert_eq!(BrokerTool::named(tool.name()), Some(tool));
            assert_eq!(tool.input_schema()["type"], json!("object"));
        }
        assert_eq!(BrokerTool::named("spawn_everything"), None);
    }

    #[test]
    fn stop_arguments_are_read_as_their_schema_gives_them() {
        let id = SessionId::new();
        assert_eq!(
            StopArguments::read(&options(json!({ "id": id }))),
            Ok(StopArguments { id })
        );
        for (value, says) in [
            (json!({}), "needs `id`"),
            (json!({ "id": null }), "needs `id`"),
            (json!({ "id": 7 }), "`id` must be a string"),
            (json!({ "id": "the researcher" }), "is not a Session id"),
            (
                json!({ "id": id, "force": true }),
                "takes no argument `force`",
            ),
        ] {
            let refusal = StopArguments::read(&options(value))
                .expect_err("the arguments are refused")
                .to_string();
            assert!(refusal.contains(says), "{says:?} is said in {refusal:?}");
        }
    }

    #[test]
    fn stop_subagent_says_stopped_only_where_the_subagents_work_was_stopped() {
        let id = SessionId::new();
        assert_eq!(
            stop_answer(id, BrokeredStop::StoppedWork),
            json!({ "session_id": id, "stopped": true })
        );
        for (stop, says) in [
            (
                BrokeredStop::StoppedDelegatedWork,
                "The Subagents it delegated to, which were still working, were stopped",
            ),
            (
                BrokeredStop::StoppedWatches,
                "Only the Watches it left running",
            ),
            (BrokeredStop::WithdrewPrompt, "had not begun"),
            (BrokeredStop::NothingRunning, "nothing to stop"),
        ] {
            let answer = stop_answer(id, stop);
            assert_eq!(answer["session_id"], json!(id));
            assert_eq!(answer["stopped"], json!(false), "{stop:?}");
            assert!(
                answer["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.contains(says)),
                "{stop:?} says {says:?}: {answer}"
            );
        }
    }

    #[test]
    fn stop_subagents_schema_requires_what_its_description_says_it_takes() {
        let schema = BrokerTool::StopSubagent.input_schema();
        for argument in StopArguments::TAKES {
            assert!(
                schema["properties"]
                    .as_object()
                    .is_some_and(|properties| properties.contains_key(argument)),
                "the schema takes {argument}"
            );
            assert!(
                STOP_SUBAGENT_DESCRIPTION.contains(&format!("\"{argument}\"")),
                "the description says what {argument} is"
            );
        }
        assert_eq!(schema["required"], json!(StopArguments::TAKES));
        assert_eq!(schema["additionalProperties"], json!(false));
        assert!(
            !BrokerTool::StopSubagent.is_read_only(),
            "a stop changes what Suru holds"
        );
    }

    /// Codex's catalog with one Model that runs, carrying a select option
    /// with a choice it will not run and a toggle, and one Model it lists but
    /// will not run.
    fn spawnable(status: ProviderCatalogStatus) -> ModelCatalog {
        let mut runs = model("runs", ModelAvailability::Available);
        runs.options.push(ModelOptionDescriptor {
            id: ModelOptionId::new("fast"),
            label: "Fast".to_owned(),
            description: None,
            role: ModelOptionRole::Speed,
            kind: ModelOptionKind::Toggle { default: false },
        });
        catalog(
            status,
            vec![runs, model("refused", ModelAvailability::Unavailable)],
        )
    }

    fn options(options: Value) -> Map<String, Value> {
        let Value::Object(options) = options else {
            panic!("options are an object");
        };
        options
    }

    fn refused(catalog: &ModelCatalog, provider: &str, model: &str, chosen: Value) -> String {
        requested_selection(catalog, provider, model, &options(chosen))
            .expect_err("the spawn is refused")
            .to_string()
    }

    #[test]
    fn a_spawns_model_options_are_its_models_defaults_with_what_it_named_set() {
        let catalog = spawnable(ProviderCatalogStatus::Fresh);
        let selection = |chosen| {
            requested_selection(&catalog, "codex", "runs", &options(chosen))
                .expect("the spawn is taken")
        };
        let choice = |id: &str| ModelOptionValue::Select {
            choice: ModelOptionChoiceId::new(id),
        };
        let values = |selection: AgentSelection| {
            selection
                .options
                .into_iter()
                .map(|option| (option.id.to_string(), option.value))
                .collect::<Vec<_>>()
        };

        assert_eq!(
            values(selection(json!({}))),
            [
                ("effort".to_owned(), choice("low")),
                (
                    "fast".to_owned(),
                    ModelOptionValue::Toggle { enabled: false }
                ),
            ],
            "every option left out takes the Model's default"
        );
        assert_eq!(
            values(selection(json!({ "fast": true }))),
            [
                ("effort".to_owned(), choice("low")),
                (
                    "fast".to_owned(),
                    ModelOptionValue::Toggle { enabled: true }
                ),
            ],
            "an option named is set, the rest defaulted"
        );
        let chosen = selection(json!({}));
        assert_eq!(
            (chosen.provider, chosen.model),
            (ProviderId::new("codex"), ModelId::new("runs"))
        );
    }

    #[test]
    fn a_spawn_is_refused_anything_list_providers_would_not_offer_saying_what() {
        let fresh = spawnable(ProviderCatalogStatus::Fresh);
        for (catalog, provider, model, chosen, says) in [
            (
                &fresh,
                "claude",
                "runs",
                json!({}),
                "Suru hosts no Provider `claude`; the Providers it hosts are `codex`",
            ),
            (
                &fresh,
                "codex",
                "refused",
                json!({}),
                "Provider `codex` offers no Model `refused`; choose one of `runs`",
            ),
            (
                &fresh,
                "codex",
                "runs",
                json!({ "effort": "max" }),
                "Model Option `effort` has no choice `max`; choose one of `low`",
            ),
            (
                &fresh,
                "codex",
                "runs",
                json!({ "effort": true }),
                "Model Option `effort` takes a choice id as a string: one of `low`",
            ),
            (
                &fresh,
                "codex",
                "runs",
                json!({ "fast": "on" }),
                "Model Option `fast` is a toggle and takes true or false",
            ),
            (
                &fresh,
                "codex",
                "runs",
                json!({ "speed": "fast" }),
                "Model `runs` has no Model Option `speed`; its Model Options are `effort`, `fast`",
            ),
            (
                &spawnable(ProviderCatalogStatus::Disabled),
                "codex",
                "runs",
                json!({}),
                "Provider `codex` is turned off in Suru",
            ),
            (
                &spawnable(ProviderCatalogStatus::Unavailable {
                    reason: ProviderUnavailability::NotSignedIn,
                    message: "sign in to the Codex CLI.".to_owned(),
                }),
                "codex",
                "runs",
                json!({}),
                "Provider `codex` cannot be used now: sign in to the Codex CLI. Choose another Provider.",
            ),
            (
                &catalog(ProviderCatalogStatus::Refreshing, Vec::new()),
                "codex",
                "runs",
                json!({}),
                "Suru is still asking this Provider for its Models",
            ),
        ] {
            let refusal = refused(catalog, provider, model, chosen);
            assert!(refusal.contains(says), "{says:?} is said in {refusal:?}");
        }
    }

    #[test]
    fn a_model_already_known_may_be_spawned_on_while_its_catalog_is_checked_again() {
        requested_selection(
            &spawnable(ProviderCatalogStatus::Refreshing),
            "codex",
            "runs",
            &Map::new(),
        )
        .expect("a re-check leaves known Models selectable, as list_providers lists them");
    }

    #[test]
    fn spawn_arguments_are_read_as_their_schema_gives_them() {
        let arguments = |value| options(value);
        let read = SpawnArguments::read(&arguments(json!({
            "provider": "codex",
            "model": "runs",
            "name": "Researcher",
            "description": "",
            "prompt": "Map the seams.",
        })))
        .expect("every required argument is given");
        assert_eq!(read.options, Map::new(), "options may be left out");
        assert_eq!(read.description, "", "and a description may be empty");

        for (value, says) in [
            (
                json!({ "model": "runs", "name": "R", "description": "", "prompt": "p" }),
                "needs `provider`",
            ),
            (
                json!({ "provider": 7, "model": "runs", "name": "R", "description": "", "prompt": "p" }),
                "`provider` must be a string",
            ),
            (
                json!({ "provider": "codex", "model": "runs", "name": " ", "description": "", "prompt": "p" }),
                "`name` is empty",
            ),
            (
                json!({ "provider": "codex", "model": "runs", "name": "R", "description": "", "prompt": "\n" }),
                "`prompt` is empty",
            ),
            (
                json!({ "provider": "codex", "model": "runs", "name": "R", "description": "", "prompt": "p", "options": [] }),
                "`options` must be an object",
            ),
            (
                json!({ "provider": "codex", "model": "runs", "name": "R", "description": "", "prompt": "p", "cwd": "/" }),
                "takes no argument `cwd`",
            ),
        ] {
            let refusal = SpawnArguments::read(&arguments(value))
                .expect_err("the arguments are refused")
                .to_string();
            assert!(refusal.contains(says), "{says:?} is said in {refusal:?}");
        }
    }

    #[test]
    fn the_read_description_names_every_status_a_subagent_may_stand_at() {
        for status in [
            TurnStatus::Active,
            TurnStatus::Completed,
            TurnStatus::Failed,
            TurnStatus::Interrupted,
        ] {
            let spelled = serde_json::to_value(SubagentStatus::from(status))
                .expect("a status serializes")
                .to_string();
            assert!(
                READ_SUBAGENT_DESCRIPTION.contains(&spelled),
                "read_subagent's description names {spelled}"
            );
        }
        for field in ["session_id", "status", "duration_ms", "message"] {
            assert!(
                READ_SUBAGENT_DESCRIPTION.contains(&format!("\"{field}\"")),
                "read_subagent's description gives the shape of {field}"
            );
        }
    }

    #[test]
    fn a_read_names_its_subagent_by_the_id_spawn_answered_with() {
        let id = SessionId::new();
        assert_eq!(
            named_subagent(
                BrokerTool::ReadSubagent,
                &options(json!({ "id": id })),
                &["id"]
            ),
            Ok(id)
        );
        for (arguments, says) in [
            (json!({}), "read_subagent needs `id`"),
            (json!({ "id": null }), "read_subagent needs `id`"),
            (json!({ "id": 7 }), "`id` must be the session_id"),
            (json!({ "id": "Researcher" }), "`id` must be the session_id"),
            (
                json!({ "id": id, "tail": 3 }),
                "read_subagent takes no argument `tail`; it takes `id`.",
            ),
        ] {
            let refusal = named_subagent(BrokerTool::ReadSubagent, &options(arguments), &["id"])
                .expect_err("the arguments are refused")
                .to_string();
            assert!(refusal.contains(says), "{says:?} is said in {refusal:?}");
        }
    }

    #[test]
    fn spawn_subagents_schema_requires_what_its_description_says_it_takes() {
        let schema = BrokerTool::SpawnSubagent.input_schema();
        let properties = schema["properties"]
            .as_object()
            .expect("the schema names its properties");
        for argument in SpawnArguments::TAKES {
            assert!(
                properties.contains_key(argument),
                "the schema takes {argument}"
            );
            assert!(
                SPAWN_SUBAGENT_DESCRIPTION.contains(&format!("\"{argument}\"")),
                "the description says what {argument} is"
            );
        }
        assert_eq!(schema["additionalProperties"], json!(false));
    }
}
