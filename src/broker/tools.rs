//! The Tools the Broker offers, each described once and answered in Suru's
//! own terms. Nothing here knows MCP: the transport in [`super::mcp`] turns a
//! [`BrokerTool`]'s description into what `tools/list` lists and routes a
//! `tools/call` into [`BrokerTools::call`].
//!
//! Which caller is offered a Tool is part of the Tool's own description
//! ([`BrokerTool::is_sidekicks`]): every Agent is offered the Tools through
//! which it reaches any Provider, and a Sidekick those besides the Tools that
//! work across Suru itself (ADR 0042). The transport lists and dispatches by
//! [`BrokerTool::offered_to`], and the note an Agent is told names what it
//! reads, so a Sidekick's Tool is declared here and nowhere else.

mod memories;
mod origins;
mod session_acts;
mod session_beginning;
mod session_listing;
mod session_reading;
mod settings;
mod workspaces;

pub(super) use settings::SIDEKICK_SETTINGS_RULE;

use std::borrow::Cow;

use futures_util::future::BoxFuture;
use serde::Serialize;
use serde_json::{Map, Value, json};
use tokio::sync::watch;

use super::{
    BrokerCaller, BrokerRole,
    wait::{self, WaitOutcome, WaitTimings},
};
use crate::{
    clock::ServerClock,
    model_catalog::ModelCatalogService,
    protocol::{
        AgentSelection, ModelAvailability, ModelCatalog, ModelDescriptor, ModelId,
        ModelOptionChoiceId, ModelOptionDescriptor, ModelOptionId, ModelOptionKind,
        ModelOptionValue, ProviderCatalogStatus, ProviderId, ProviderModelCatalog,
        ProviderUnavailability, SessionId, SettingsSnapshot, TurnStatus,
    },
    provider::{
        BrokeredDelivery, BrokeredSendRefusal, BrokeredSpawnRefusal, BrokeredStop,
        BrokeredSubagentRequest, ProviderOrchestrator, ProviderSubagentId,
    },
    server::operations::SessionOperations,
    sessions::{BrokeredReadError, BrokeredSpawnCap, BrokeredSubagentReading, SessionStore},
};

/// One Tool the Broker offers. A new Tool is a variant here, its description
/// beside the others — who is offered it among them — and an arm of
/// [`BrokerTools::call`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BrokerTool {
    ListProviders,
    SpawnSubagent,
    ReadSubagent,
    SendToSubagent,
    WaitSubagents,
    StopSubagent,
    /// A Sidekick's: the Sessions on its own Server, a Remote, or Everywhere,
    /// as compact rows.
    ListSessions,
    /// A Sidekick's: one Session on its own Server or a Remote, read as
    /// compact text.
    ReadSession,
    /// A Sidekick's: a Prompt sent to a Session on the user's behalf.
    SendPrompt,
    /// A Sidekick's: a Session interrupted, as the user interrupts one.
    InterruptSession,
    /// A Sidekick's: a Session set aside as done for now.
    SettleSession,
    /// A Sidekick's: a settled Session brought back.
    UnsettleSession,
    /// A Sidekick's: a Session begun on the user's behalf, a Subsession.
    BeginSession,
    /// A Sidekick's: a Session's Questionnaire answered on the user's behalf.
    AnswerQuestionnaire,
    /// A Sidekick's: the Workspaces on its own Server, a Remote, or
    /// Everywhere, as compact rows.
    ListWorkspaces,
    /// A Sidekick's: a Workspace's Description set, as the user sets one.
    SetWorkspaceDescription,
    /// A Sidekick's: the Remotes its own Server is paired with, and whether
    /// each answers.
    ListRemotes,
    /// A Sidekick's: its own Server's Settings, by key and the value in
    /// force.
    ListSettings,
    /// A Sidekick's: one of its own Server's Settings, and what it accepts.
    DescribeSetting,
    /// A Sidekick's: one of its own Server's Settings changed, as the user
    /// changes one.
    SetSetting,
    /// A Sidekick's: a Memory stored, for every Sidekick after it.
    StoreMemory,
    /// A Sidekick's: the Memories found by their words, tags and dates, as
    /// compact rows carrying snippets.
    SearchMemory,
    /// A Sidekick's: one Memory, whole.
    RecallMemory,
    /// A Sidekick's: a Memory changed, so what is kept stays true.
    UpdateMemory,
    /// A Sidekick's: a Memory forgotten.
    ForgetMemory,
}

impl BrokerTool {
    /// Every Tool, in the order `tools/list` lists those a caller is offered:
    /// every Agent's first, then a Sidekick's own.
    pub(super) const ALL: [Self; 25] = [
        Self::ListProviders,
        Self::SpawnSubagent,
        Self::ReadSubagent,
        Self::SendToSubagent,
        Self::WaitSubagents,
        Self::StopSubagent,
        Self::ListSessions,
        Self::ReadSession,
        Self::SendPrompt,
        Self::InterruptSession,
        Self::SettleSession,
        Self::UnsettleSession,
        Self::BeginSession,
        Self::AnswerQuestionnaire,
        Self::ListWorkspaces,
        Self::SetWorkspaceDescription,
        Self::ListRemotes,
        Self::ListSettings,
        Self::DescribeSetting,
        Self::SetSetting,
        Self::StoreMemory,
        Self::SearchMemory,
        Self::RecallMemory,
        Self::UpdateMemory,
        Self::ForgetMemory,
    ];

    pub(super) fn named(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tool| tool.name() == name)
    }

    /// Whether the Tool is a Sidekick's alone — one that works across Suru
    /// itself — rather than one every Agent is offered.
    pub(super) const fn is_sidekicks(self) -> bool {
        match self {
            Self::ListSessions
            | Self::ReadSession
            | Self::SendPrompt
            | Self::InterruptSession
            | Self::SettleSession
            | Self::UnsettleSession
            | Self::BeginSession
            | Self::AnswerQuestionnaire
            | Self::ListWorkspaces
            | Self::SetWorkspaceDescription
            | Self::ListRemotes
            | Self::ListSettings
            | Self::DescribeSetting
            | Self::SetSetting
            | Self::StoreMemory
            | Self::SearchMemory
            | Self::RecallMemory
            | Self::UpdateMemory
            | Self::ForgetMemory => true,
            Self::ListProviders
            | Self::SpawnSubagent
            | Self::ReadSubagent
            | Self::SendToSubagent
            | Self::WaitSubagents
            | Self::StopSubagent => false,
        }
    }

    /// Whether an Agent that is `role` to the Broker is offered the Tool: may
    /// see it listed, and may call it.
    pub(super) fn is_offered_to(self, role: BrokerRole) -> bool {
        !self.is_sidekicks() || role == BrokerRole::Sidekick
    }

    /// The Tools an Agent that is `role` to the Broker is offered, in the
    /// order they are listed.
    pub(super) fn offered_to(role: BrokerRole) -> impl Iterator<Item = Self> {
        Self::ALL
            .into_iter()
            .filter(move |tool| tool.is_offered_to(role))
    }

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::ListProviders => "list_providers",
            Self::SpawnSubagent => "spawn_subagent",
            Self::ReadSubagent => "read_subagent",
            Self::SendToSubagent => "send_to_subagent",
            Self::WaitSubagents => crate::protocol::WAIT_SUBAGENTS_TOOL,
            Self::StopSubagent => "stop_subagent",
            Self::ListSessions => "list_sessions",
            Self::ReadSession => "read_session",
            Self::SendPrompt => "send_prompt",
            Self::InterruptSession => "interrupt_session",
            Self::SettleSession => "settle_session",
            Self::UnsettleSession => "unsettle_session",
            Self::BeginSession => "begin_session",
            Self::AnswerQuestionnaire => "answer_questionnaire",
            Self::ListWorkspaces => "list_workspaces",
            Self::SetWorkspaceDescription => "set_workspace_description",
            Self::ListRemotes => "list_remotes",
            Self::ListSettings => "list_settings",
            Self::DescribeSetting => "describe_setting",
            Self::SetSetting => "set_setting",
            Self::StoreMemory => "store_memory",
            Self::SearchMemory => "search_memory",
            Self::RecallMemory => "recall_memory",
            Self::UpdateMemory => "update_memory",
            Self::ForgetMemory => "forget_memory",
        }
    }

    /// Whether the Tool's effect is a Subagent row: spawning, sending to, and
    /// stopping a Subagent each stand in the row they open or settle, so no
    /// Provider records such a call as anything more. Every other Tool changes
    /// no row of the caller's, and is a Tool Call like any other Tool's.
    pub(super) const fn affects_a_subagent_row(self) -> bool {
        match self {
            Self::SpawnSubagent | Self::SendToSubagent | Self::StopSubagent => true,
            Self::ListProviders
            | Self::ReadSubagent
            | Self::WaitSubagents
            | Self::ListSessions
            | Self::ReadSession
            | Self::SendPrompt
            | Self::InterruptSession
            | Self::SettleSession
            | Self::UnsettleSession
            | Self::BeginSession
            | Self::AnswerQuestionnaire
            | Self::ListWorkspaces
            | Self::SetWorkspaceDescription
            | Self::ListRemotes
            | Self::ListSettings
            | Self::DescribeSetting
            | Self::SetSetting
            | Self::StoreMemory
            | Self::SearchMemory
            | Self::RecallMemory
            | Self::UpdateMemory
            | Self::ForgetMemory => false,
        }
    }

    /// Whether a call of the Tool is recorded by the row it opens or settles
    /// in the caller's Transcript — a Subagent's, or the Subsession's row
    /// beginning a Session stands as — so that no Provider records it as a
    /// Tool Call besides.
    pub(super) const fn is_recorded_by_its_row(self) -> bool {
        self.affects_a_subagent_row() || matches!(self, Self::BeginSession)
    }

    pub(super) fn title(self) -> &'static str {
        match self {
            Self::ListProviders => "List Providers",
            Self::SpawnSubagent => "Spawn Subagent",
            Self::ReadSubagent => "Read Subagent",
            Self::SendToSubagent => "Send to Subagent",
            Self::WaitSubagents => "Wait on Subagents",
            Self::StopSubagent => "Stop Subagent",
            Self::ListSessions => "List Sessions",
            Self::ReadSession => "Read Session",
            Self::SendPrompt => "Send Prompt",
            Self::InterruptSession => "Interrupt Session",
            Self::SettleSession => "Settle Session",
            Self::UnsettleSession => "Unsettle Session",
            Self::BeginSession => "Begin Session",
            Self::AnswerQuestionnaire => "Answer Questionnaire",
            Self::ListWorkspaces => "List Workspaces",
            Self::SetWorkspaceDescription => "Set Workspace Description",
            Self::ListRemotes => "List Remotes",
            Self::ListSettings => "List Settings",
            Self::DescribeSetting => "Describe Setting",
            Self::SetSetting => "Set Setting",
            Self::StoreMemory => "Store Memory",
            Self::SearchMemory => "Search Memories",
            Self::RecallMemory => "Recall Memory",
            Self::UpdateMemory => "Update Memory",
            Self::ForgetMemory => "Forget Memory",
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
            Self::SendToSubagent => SEND_TO_SUBAGENT_DESCRIPTION,
            Self::WaitSubagents => WAIT_SUBAGENTS_DESCRIPTION,
            Self::StopSubagent => STOP_SUBAGENT_DESCRIPTION,
            Self::ListSessions => session_listing::DESCRIPTION,
            Self::ReadSession => session_reading::DESCRIPTION,
            Self::SendPrompt => session_acts::SEND_PROMPT_DESCRIPTION,
            Self::InterruptSession => session_acts::INTERRUPT_SESSION_DESCRIPTION,
            Self::SettleSession => session_acts::SETTLE_SESSION_DESCRIPTION,
            Self::UnsettleSession => session_acts::UNSETTLE_SESSION_DESCRIPTION,
            Self::BeginSession => session_beginning::DESCRIPTION,
            Self::AnswerQuestionnaire => session_acts::ANSWER_QUESTIONNAIRE_DESCRIPTION,
            Self::ListWorkspaces => workspaces::LIST_WORKSPACES_DESCRIPTION,
            Self::SetWorkspaceDescription => workspaces::SET_WORKSPACE_DESCRIPTION_DESCRIPTION,
            Self::ListRemotes => origins::LIST_REMOTES_DESCRIPTION,
            Self::ListSettings => settings::LIST_SETTINGS_DESCRIPTION,
            Self::DescribeSetting => settings::DESCRIBE_SETTING_DESCRIPTION,
            Self::SetSetting => settings::SET_SETTING_DESCRIPTION,
            Self::StoreMemory => memories::STORE_MEMORY_DESCRIPTION,
            Self::SearchMemory => memories::SEARCH_MEMORY_DESCRIPTION,
            Self::RecallMemory => memories::RECALL_MEMORY_DESCRIPTION,
            Self::UpdateMemory => memories::UPDATE_MEMORY_DESCRIPTION,
            Self::ForgetMemory => memories::FORGET_MEMORY_DESCRIPTION,
        }
    }

    /// The JSON Schema of the Tool's arguments.
    pub(super) fn input_schema(self) -> Map<String, Value> {
        let schema = match self {
            Self::ListProviders | Self::ListRemotes => json!({
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
            Self::SendToSubagent => json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "The session_id spawn_subagent answered with.",
                    },
                    "message": {
                        "type": "string",
                        "description": "Everything the Subagent needs to know of what it is to \
                            do next.",
                    },
                    "summary": {
                        "type": "string",
                        "description": "A few words on what the message asks, for the row a \
                            resume adds to your Transcript; the message's first line stands \
                            in where none is given.",
                    },
                },
                "required": SendArguments::REQUIRED,
                "additionalProperties": false,
            }),
            // Neither is required, and the timeout's bounds are left to the
            // description: a timeout outside them is kept at the nearest,
            // never refused, which a schema bound would have a harness do.
            Self::WaitSubagents => json!({
                "type": "object",
                "properties": {
                    "ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "The session_ids of the Subagents to wait on, as \
                            spawn_subagent answered with them. Leave it out to wait on every \
                            Subagent you spawned that is working.",
                    },
                    "timeout_seconds": {
                        "type": "number",
                        "description": "How long to wait at most, in seconds: 60 unless given, \
                            and never less than 10 nor more than 600.",
                    },
                },
                "additionalProperties": false,
            }),
            Self::ListSessions => session_listing::input_schema(),
            Self::ReadSession => session_reading::input_schema(),
            Self::SendPrompt => session_acts::send_prompt_schema(),
            Self::InterruptSession | Self::SettleSession | Self::UnsettleSession => {
                session_acts::session_schema()
            }
            Self::BeginSession => session_beginning::input_schema(),
            Self::AnswerQuestionnaire => session_acts::answer_questionnaire_schema(),
            Self::ListWorkspaces => workspaces::list_workspaces_schema(),
            Self::SetWorkspaceDescription => workspaces::set_workspace_description_schema(),
            Self::ListSettings => settings::list_settings_schema(),
            Self::DescribeSetting => settings::describe_setting_schema(),
            Self::SetSetting => settings::set_setting_schema(),
            Self::StoreMemory => memories::store_memory_schema(),
            Self::SearchMemory => memories::search_memory_schema(),
            Self::RecallMemory | Self::ForgetMemory => memories::one_memory_schema(),
            Self::UpdateMemory => memories::update_memory_schema(),
        };
        let Value::Object(schema) = schema else {
            unreachable!("every input schema is a JSON object");
        };
        schema
    }

    /// What a Transcript may keep of the arguments a call of the Tool was
    /// made with: the arguments as they were, except `answer_questionnaire`'s,
    /// whose Answers — any of which may be secret, for all the call says — are
    /// withheld. A Setting's value is kept as it was: no Setting holds a
    /// secret. So is what a Memory Tool was called with, a stored or changed
    /// Memory's whole body among it: the Sidekick's own words, kept in the
    /// Sidekick's own Transcript.
    pub(super) fn recorded_arguments(self, arguments: &Value) -> Cow<'_, Value> {
        match self {
            Self::AnswerQuestionnaire => {
                Cow::Owned(session_acts::recorded_answer_arguments(arguments))
            }
            Self::ListProviders
            | Self::SpawnSubagent
            | Self::ReadSubagent
            | Self::SendToSubagent
            | Self::WaitSubagents
            | Self::StopSubagent
            | Self::ListSessions
            | Self::ReadSession
            | Self::SendPrompt
            | Self::InterruptSession
            | Self::SettleSession
            | Self::UnsettleSession
            | Self::BeginSession
            | Self::ListWorkspaces
            | Self::SetWorkspaceDescription
            | Self::ListRemotes
            | Self::ListSettings
            | Self::DescribeSetting
            | Self::SetSetting => Cow::Borrowed(arguments),
            Self::StoreMemory
            | Self::SearchMemory
            | Self::RecallMemory
            | Self::UpdateMemory
            | Self::ForgetMemory => Cow::Borrowed(arguments),
        }
    }

    /// Whether the Tool only reads, changing nothing Suru holds.
    pub(super) fn is_read_only(self) -> bool {
        match self {
            Self::ListProviders
            | Self::ReadSubagent
            | Self::WaitSubagents
            | Self::ListSessions
            | Self::ReadSession
            | Self::ListWorkspaces
            | Self::ListRemotes
            | Self::ListSettings
            | Self::DescribeSetting
            | Self::SearchMemory
            | Self::RecallMemory => true,
            Self::SpawnSubagent
            | Self::SendToSubagent
            | Self::StopSubagent
            | Self::SendPrompt
            | Self::InterruptSession
            | Self::SettleSession
            | Self::UnsettleSession
            | Self::BeginSession
            | Self::AnswerQuestionnaire
            | Self::SetWorkspaceDescription
            | Self::SetSetting
            | Self::StoreMemory
            | Self::UpdateMemory
            | Self::ForgetMemory => false,
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
was wrong. So is a spawn that would pass the user's limits on how deep \
Subagents spawned this way nest or how many work at once beneath the top-level \
Session; it is never queued.";

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
— its final answer once settled — or null when it has written none. A \
\"failed\" stretch also carries \"error\": what it failed with, as its \
Transcript says, where anything says why. An id naming no Subagent spawned \
with spawn_subagent by you or by a Subagent beneath you is refused.";

const SEND_TO_SUBAGENT_DESCRIPTION: &str = "\
Send more work to a Subagent spawned with spawn_subagent — by you, or by a \
Subagent beneath you. Takes \"id\", the session_id spawn_subagent answered \
with; \"message\", everything the Subagent needs to know of what it is to \
do next; and \"summary\", a few words on what the message asks, for your own \
Transcript rather than the Subagent, which may be left out. When the \
Subagent's work has settled, it is resumed on its own conversation, with \
everything it learned: the message begins a new stretch of its work, which \
stands as a new row in your Transcript leading into the same Session — reading \
the summary, or the message's first line where you gave none — and you are \
sent its Subagent Report when that stretch settles, as after a spawn. When it \
is still working on what it was delegated, the message \
steers that work and reaches it there; nothing new begins and no row is \
added. Work it took up of its own accord since it settled is stopped instead, \
and it is resumed with the message; so is one whose work ends as the message \
arrives, once that work has settled. Answers once the message has been \
delivered, with JSON of the shape {\"session_id\": \"...\", \"delivered\": \
\"...\"}, where \"delivered\" is \"resumed\" or \"steered\". An id naming no \
Subagent spawned with spawn_subagent by you or by a Subagent beneath you is \
refused, as is an empty message. So is a resume that would pass the user's \
limit on how many Subagents spawned this way work at once beneath the \
top-level Session; it is never queued, and wait_subagents waits for one to \
settle. A message the Subagent is stopped before receiving is refused, saying \
so. A refused message reaches no one.";

const WAIT_SUBAGENTS_DESCRIPTION: &str = "\
Wait until a Subagent spawned with spawn_subagent settles, when you must have \
its result before your turn ends. When you have nothing left to do but wait, \
end your turn instead: a Subagent's settling reaches you as a new message that \
wakes you if your turn has ended. Takes \"ids\", the session_ids of the Subagents \
to wait on — ones you spawned, or ones a Subagent beneath you spawned — and \
\"timeout_seconds\", how long to wait at most: 60 unless you say, and never \
less than 10 nor more than 600, a timeout outside those kept at the nearest. \
Leave \"ids\" out to wait on every Subagent you spawned with spawn_subagent \
that is working. Answers as soon as any Subagent waited on has settled — at \
once, when one already has — with JSON of the shape {\"settled\": [subagent, \
...], \"timed_out\": false, \"timeout_seconds\": ...}: one entry for every \
Subagent waited on that has settled, each shaped as read_subagent answers, \
{\"session_id\": \"...\", \"status\": \"...\", \"duration_ms\": ..., \
\"message\": ...}, and the timeout the wait kept. When none settles in time it \
answers {\"settled\": [], \"timed_out\": true, \"timeout_seconds\": ...}; call \
it again to wait on. With nothing to wait on — no ids given and none of your \
Subagents working — it answers at once, {\"settled\": [], \"timed_out\": \
false, \"timeout_seconds\": ..., \"reason\": \"...\"}. A Subagent Report \
still reaches you when a Subagent settles, whether or not a wait answered \
with it. An id naming no Subagent spawned with spawn_subagent by you or by a \
Subagent beneath you is refused.";

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

/// One call of a Tool: who is calling — the Session its token names, or the
/// native Subagent's the call named more exactly — the arguments as the Agent
/// sent them, and where the call may report progress while it runs, when its
/// caller asked to hear of it.
pub(super) struct ToolCall {
    pub(super) caller: BrokerCaller,
    pub(super) arguments: Map<String, Value>,
    pub(super) progress: Option<ProgressReporter>,
}

/// How far a Tool call that runs long has come, reported while it runs so
/// the harness waiting on it keeps its idle window open (ADR 0034): the
/// progress so far, out of the total it may reach, and a line saying what it
/// is doing.
pub(super) struct ToolProgress {
    pub(super) progress: f64,
    pub(super) total: f64,
    pub(super) message: String,
}

/// Where a Tool call reports its progress: the transport it came in by,
/// which knows how.
pub(super) type ProgressReporter =
    Box<dyn Fn(ToolProgress) -> BoxFuture<'static, ()> + Send + Sync>;

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
    /// The acts on Sessions a Sidekick performs, each the very operation a
    /// Client's request performs.
    operations: SessionOperations,
    /// Read at each listing of Sessions, for the auto-settle Setting the
    /// user's own listing reads, and by the Settings Tools for the settings in
    /// force.
    settings: watch::Receiver<SettingsSnapshot>,
    /// The moment a listing of Sessions reads auto-settle against.
    clock: ServerClock,
    wait: WaitTimings,
}

impl BrokerTools {
    pub(crate) fn new(
        model_catalog: ModelCatalogService,
        providers: ProviderOrchestrator,
        sessions: SessionStore,
        operations: SessionOperations,
        settings: watch::Receiver<SettingsSnapshot>,
    ) -> Self {
        Self {
            model_catalog,
            providers,
            sessions,
            operations,
            settings,
            clock: ServerClock::default(),
            wait: WaitTimings::default(),
        }
    }

    /// Reads the moment a listing of Sessions settles them against from
    /// `clock` rather than the real one.
    pub(crate) fn with_clock(mut self, clock: ServerClock) -> Self {
        self.clock = clock;
        self
    }

    /// Who a call presenting `caller`'s token is made for, once `agent` — the
    /// Provider's own identity for the Agent making the call, where the call
    /// names one — is read against the Sessions the Tools act on; see
    /// [`BrokerCaller::attributed`].
    pub(super) fn attribute(
        &self,
        caller: BrokerCaller,
        agent: Option<&ProviderSubagentId>,
    ) -> BrokerCaller {
        caller.attributed(agent, &self.sessions)
    }

    /// Keeps `wait_subagents`' clock by `timings` rather than real seconds.
    pub(crate) fn with_wait_timings(mut self, timings: WaitTimings) -> Self {
        self.wait = timings;
        self
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
                    .map_err(spawn_refusal)?;
                Ok(json!({ "session_id": session_id }))
            }
            BrokerTool::ReadSubagent => self.read_subagent(call),
            BrokerTool::SendToSubagent => {
                let send = SendArguments::read(&call.arguments)?;
                let delivered = self
                    .providers
                    .send_to_brokered_subagent(
                        call.caller.session_id(),
                        send.id,
                        &send.message,
                        send.summary.as_deref(),
                    )
                    .await
                    .map_err(|refusal| send_refusal(refusal, send.id))?;
                Ok(serde_json::to_value(SendAnswer {
                    session_id: send.id,
                    delivered: Delivered::from(delivered),
                })
                .expect("a send's answer always serializes"))
            }
            BrokerTool::WaitSubagents => self.wait_subagents(call).await,
            BrokerTool::StopSubagent => {
                let subagent =
                    named_subagent(BrokerTool::StopSubagent, &call.arguments, &STOP_TAKES)?;
                let stop = self
                    .providers
                    .stop_brokered_subagent(call.caller.session_id(), subagent)
                    .await
                    .map_err(ToolRefusal)?;
                Ok(stop_answer(subagent, stop))
            }
            BrokerTool::ListSessions => {
                let arguments = session_listing::ListArguments::read(&call.arguments)?;
                let gathered = self
                    .operations
                    .sessions_in(arguments.origins())
                    .await
                    .map_err(|refusal| {
                        origins::origin_refusal(refusal, "Its Sessions were not listed.")
                    })?;
                let auto_settle = self.settings.borrow().settings.sidebar.auto_settle;
                let listing =
                    session_listing::listing(gathered, &arguments, auto_settle, self.clock.now());
                Ok(serde_json::to_value(listing).expect("a listing of Sessions always serializes"))
            }
            BrokerTool::ReadSession => self.read_session(call).await,
            BrokerTool::SendPrompt => self.send_prompt(call).await,
            BrokerTool::InterruptSession => self.interrupt_session(call).await,
            BrokerTool::SettleSession => self.settle_session(tool, call, true).await,
            BrokerTool::UnsettleSession => self.settle_session(tool, call, false).await,
            BrokerTool::BeginSession => self.begin_session(call).await,
            BrokerTool::AnswerQuestionnaire => self.answer_questionnaire(call).await,
            BrokerTool::ListWorkspaces => self.list_workspaces(&call).await,
            BrokerTool::SetWorkspaceDescription => self.set_workspace_description(&call).await,
            BrokerTool::ListRemotes => self.list_remotes(&call).await,
            BrokerTool::ListSettings => self.list_settings(&call),
            BrokerTool::DescribeSetting => self.describe_setting(&call),
            BrokerTool::SetSetting => self.set_setting(&call).await,
            BrokerTool::StoreMemory => self.store_memory(&call).await,
            BrokerTool::SearchMemory => self.search_memory(&call).await,
            BrokerTool::RecallMemory => self.recall_memory(&call).await,
            BrokerTool::UpdateMemory => self.update_memory(&call).await,
            BrokerTool::ForgetMemory => self.forget_memory(&call).await,
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
            .map_err(|error| unreachable_refusal(BrokerTool::ReadSubagent, error, subagent))?;
        Ok(serde_json::to_value(SubagentReadout::from(reading))
            .expect("a Subagent's reading always serializes"))
    }

    /// Answers `wait_subagents`: blocks until a Subagent waited on settles or
    /// the kept timeout passes, reporting progress meanwhile where the call
    /// asked to hear of it. A call naming no Subagent waits on the brokered
    /// Subagents the caller spawned that are working — its own delegations,
    /// whose Reports reach it — and one finding none answers at once.
    async fn wait_subagents(&self, call: ToolCall) -> Result<Value, ToolRefusal> {
        let tool = BrokerTool::WaitSubagents;
        let arguments = WaitArguments::read(&call.arguments)?;
        let caller = call.caller.session_id();
        let waited_on = match arguments.ids {
            Some(ids) => ids,
            None => self
                .sessions
                .working_brokered_children(caller)
                .map_err(|error| unreachable_refusal(tool, error, caller))?,
        };
        if waited_on.is_empty() {
            return Ok(json!({
                "settled": [],
                "timed_out": false,
                "timeout_seconds": arguments.timeout_seconds,
                "reason": "None of the Subagents you spawned with spawn_subagent is working, so \
                    there was nothing to wait on.",
            }));
        }
        let outcome = wait::until_one_settles(
            &self.sessions,
            self.wait,
            caller,
            &waited_on,
            arguments.timeout_seconds,
            call.progress.as_ref(),
        )
        .await
        .map_err(|unreachable| {
            unreachable_refusal(tool, unreachable.error, unreachable.subagent)
        })?;
        let (settled, timed_out) = match outcome {
            WaitOutcome::Settled(readings) => (
                readings.into_iter().map(SubagentReadout::from).collect(),
                false,
            ),
            WaitOutcome::TimedOut => (Vec::new(), true),
        };
        Ok(json!({
            "settled": settled,
            "timed_out": timed_out,
            "timeout_seconds": arguments.timeout_seconds,
        }))
    }
}

/// What a refused spawn tells the delegating Agent. A cap is named with the
/// Setting that pins it and what the Agent may do instead, so it waits or
/// reconsiders rather than believing work has begun.
fn spawn_refusal(refusal: BrokeredSpawnRefusal) -> ToolRefusal {
    match refusal {
        BrokeredSpawnRefusal::Refused(reason) => ToolRefusal(reason),
        BrokeredSpawnRefusal::Capped(cap) => cap_refusal(cap, "nothing was spawned"),
    }
}

/// What a delegating Agent is told of `cap`, which kept what it asked for
/// from happening — `not_done` says what did not — naming the Setting that
/// pins the cap and what the Agent may do instead.
fn cap_refusal(cap: BrokeredSpawnCap, not_done: &str) -> ToolRefusal {
    match cap {
        BrokeredSpawnCap::Depth { max_depth, depth } => ToolRefusal(format!(
            "Suru's Broker lets Subagents stand at most {max_depth} {sessions} deep, counting the \
             top-level Session as the first (`broker.maxDepth`), and one spawned here would stand \
             {depth} deep, so {not_done}. Do this work yourself, or ask the user to raise the \
             Setting.",
            sessions = if max_depth == 1 {
                "Session"
            } else {
                "Sessions"
            },
        )),
        BrokeredSpawnCap::Concurrency {
            max_concurrent_subagents,
            working,
        } => ToolRefusal(format!(
            "Suru's Broker lets at most {max_concurrent_subagents} brokered {subagents} work at \
             once beneath a top-level Session (`broker.maxConcurrentSubagents`), and {working} \
             {are} working now, so {not_done}. Call wait_subagents to wait for one to settle, or \
             ask the user to raise the Setting.",
            subagents = if max_concurrent_subagents == 1 {
                "Subagent"
            } else {
                "Subagents"
            },
            are = if working == 1 { "is" } else { "are" },
        )),
    }
}

/// What a refused `send_to_subagent` tells the Agent that sent to
/// `subagent`: nothing it sent reached the Subagent, and why.
fn send_refusal(refusal: BrokeredSendRefusal, subagent: SessionId) -> ToolRefusal {
    match refusal {
        BrokeredSendRefusal::Unreachable(error) => {
            unreachable_refusal(BrokerTool::SendToSubagent, error, subagent)
        }
        BrokeredSendRefusal::Capped(cap) => cap_refusal(cap, "the Subagent was not resumed"),
        BrokeredSendRefusal::EmptyMessage => ToolRefusal::new(EMPTY_MESSAGE),
        BrokeredSendRefusal::Refused(reason) => ToolRefusal(reason),
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
    takes_only(tool, arguments, takes)?;
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

/// Refuses a call of `tool` naming any argument it does not take, saying
/// which it does take, as `takes` lists them.
fn takes_only(
    tool: BrokerTool,
    arguments: &Map<String, Value>,
    takes: &[&str],
) -> Result<(), ToolRefusal> {
    match arguments
        .keys()
        .find(|argument| !takes.contains(&argument.as_str()))
    {
        None => Ok(()),
        Some(unknown) => Err(ToolRefusal::new(format!(
            "{} takes no argument `{unknown}`; it takes {}.",
            tool.name(),
            listed(takes.iter().copied())
        ))),
    }
}

/// Why `tool` could not reach `subagent` for the calling Agent, in words that
/// Agent reads.
fn unreachable_refusal(
    tool: BrokerTool,
    error: BrokeredReadError,
    subagent: SessionId,
) -> ToolRefusal {
    let reaches_only = match tool {
        BrokerTool::ReadSubagent => "read_subagent reads only those".to_owned(),
        BrokerTool::SendToSubagent => "send_to_subagent sends only to those".to_owned(),
        BrokerTool::WaitSubagents => "wait_subagents waits only on those".to_owned(),
        other => format!("{} takes only those", other.name()),
    };
    ToolRefusal::new(match error {
        BrokeredReadError::CallerNotFound => {
            "The Session calling the Broker no longer exists on this Suru server.".to_owned()
        }
        BrokeredReadError::NoSuchSession => format!(
            "Suru holds no Session `{subagent}`; pass the session_id spawn_subagent answered with."
        ),
        BrokeredReadError::NotBrokeredBeneathCaller => format!(
            "`{subagent}` is not a Subagent spawned with spawn_subagent by you or by a Subagent \
             beneath you; {reaches_only}."
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
    /// What a failed stretch failed with; left out of every other reading,
    /// and of a failure nothing explained.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl From<BrokeredSubagentReading> for SubagentReadout {
    fn from(reading: BrokeredSubagentReading) -> Self {
        Self {
            session_id: reading.session_id,
            status: SubagentStatus::from(reading.status),
            duration_ms: reading.duration_ms,
            message: reading.message,
            error: reading.error,
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

/// What `send_to_subagent` is refused with for a message that says nothing.
const EMPTY_MESSAGE: &str =
    "send_to_subagent's `message` is empty; say what the Subagent is to do.";

/// What `send_to_subagent` answers.
#[derive(Debug, Serialize)]
struct SendAnswer {
    session_id: SessionId,
    delivered: Delivered,
}

/// How a message `send_to_subagent` sent was delivered, spelled as its
/// description spells it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Delivered {
    /// It began a new stretch of the Subagent's work.
    Resumed,
    /// It reached the work the Subagent was doing.
    Steered,
}

impl From<BrokeredDelivery> for Delivered {
    fn from(delivery: BrokeredDelivery) -> Self {
        match delivery {
            BrokeredDelivery::Resumed => Self::Resumed,
            BrokeredDelivery::Steered => Self::Steered,
        }
    }
}

/// What `stop_subagent` takes: the one Subagent to stop.
const STOP_TAKES: [&str; 1] = ["id"];

/// What `send_to_subagent` was called with: the Subagent to send to, what it
/// is to do next, and the caller's few words on that, where it gave any.
#[derive(Debug, Eq, PartialEq)]
struct SendArguments {
    id: SessionId,
    message: String,
    summary: Option<String>,
}

impl SendArguments {
    /// Everything a call may name.
    const TAKES: [&'static str; 3] = ["id", "message", "summary"];
    /// What a call must name: a summary may be left out.
    const REQUIRED: [&'static str; 2] = ["id", "message"];

    fn read(arguments: &Map<String, Value>) -> Result<Self, ToolRefusal> {
        let id = named_subagent(BrokerTool::SendToSubagent, arguments, &Self::TAKES)?;
        let message = match arguments.get("message") {
            Some(Value::String(message)) => message.clone(),
            None | Some(Value::Null) => {
                return Err(ToolRefusal::new(
                    "send_to_subagent needs `message`, what the Subagent is to do next.",
                ));
            }
            Some(_) => {
                return Err(ToolRefusal::new(
                    "send_to_subagent's `message` must be a string.",
                ));
            }
        };
        if message.trim().is_empty() {
            return Err(ToolRefusal::new(EMPTY_MESSAGE));
        }
        // A summary that says nothing is none, and the row reads the message's
        // first line as it does when none was given.
        let summary = match arguments.get("summary") {
            None | Some(Value::Null) => None,
            Some(Value::String(summary)) => {
                let summary = summary.trim();
                (!summary.is_empty()).then(|| summary.to_owned())
            }
            Some(_) => {
                return Err(ToolRefusal::new(
                    "send_to_subagent's `summary` must be a string.",
                ));
            }
        };
        Ok(Self {
            id,
            message,
            summary,
        })
    }
}

/// What `wait_subagents` was called with: the Subagents to wait on, where it
/// named any, and the timeout it keeps.
#[derive(Debug, Eq, PartialEq)]
struct WaitArguments {
    /// `None` when the call named none, to wait on every Subagent the caller
    /// spawned that is working.
    ids: Option<Vec<SessionId>>,
    timeout_seconds: u64,
}

impl WaitArguments {
    const TAKES: [&'static str; 2] = ["ids", "timeout_seconds"];

    fn read(arguments: &Map<String, Value>) -> Result<Self, ToolRefusal> {
        takes_only(BrokerTool::WaitSubagents, arguments, &Self::TAKES)?;
        let ids = match arguments.get("ids") {
            None | Some(Value::Null) => None,
            Some(Value::Array(ids)) if ids.is_empty() => None,
            Some(Value::Array(ids)) => {
                let mut named = Vec::with_capacity(ids.len());
                for id in ids {
                    let id = serde_json::from_value::<SessionId>(id.clone()).map_err(|_| {
                        ToolRefusal::new(format!(
                            "wait_subagents' `ids` must be the session_ids spawn_subagent \
                             answered with; {id} is not a session_id."
                        ))
                    })?;
                    if !named.contains(&id) {
                        named.push(id);
                    }
                }
                Some(named)
            }
            Some(_) => {
                return Err(ToolRefusal::new(
                    "wait_subagents' `ids` must be a list of the session_ids spawn_subagent \
                     answered with.",
                ));
            }
        };
        let timeout_seconds = match arguments.get("timeout_seconds") {
            None | Some(Value::Null) => wait::DEFAULT_TIMEOUT_SECONDS,
            Some(Value::Number(seconds)) => {
                wait::kept_timeout(seconds.as_f64().unwrap_or(f64::MAX))
            }
            Some(_) => {
                return Err(ToolRefusal::new(format!(
                    "wait_subagents' `timeout_seconds` must be a number of seconds, from {} to {}.",
                    wait::MIN_TIMEOUT_SECONDS,
                    wait::MAX_TIMEOUT_SECONDS
                )));
            }
        };
        Ok(Self {
            ids,
            timeout_seconds,
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
        takes_only(BrokerTool::SpawnSubagent, arguments, &Self::TAKES)?;
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
                format!("Model `{model}` has no Model Options; leave out `{option_id}`.")
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
    fn a_sidekick_is_offered_every_tool_and_any_other_agent_all_but_the_sidekicks_own() {
        assert_eq!(
            BrokerTool::offered_to(BrokerRole::Sidekick).collect::<Vec<_>>(),
            BrokerTool::ALL
        );
        assert_eq!(
            BrokerTool::offered_to(BrokerRole::Agent)
                .map(BrokerTool::name)
                .collect::<Vec<_>>(),
            [
                "list_providers",
                "spawn_subagent",
                "read_subagent",
                "send_to_subagent",
                "wait_subagents",
                "stop_subagent",
            ]
        );
        assert!(!BrokerTool::ListSessions.is_offered_to(BrokerRole::Agent));
        assert!(!BrokerTool::ReadSession.is_offered_to(BrokerRole::Agent));
        assert!(!BrokerTool::AnswerQuestionnaire.is_offered_to(BrokerRole::Agent));
        assert!(!BrokerTool::ListWorkspaces.is_offered_to(BrokerRole::Agent));
        assert!(!BrokerTool::SetWorkspaceDescription.is_offered_to(BrokerRole::Agent));
        assert!(!BrokerTool::ListRemotes.is_offered_to(BrokerRole::Agent));
        assert!(!BrokerTool::ListSettings.is_offered_to(BrokerRole::Agent));
        assert!(!BrokerTool::DescribeSetting.is_offered_to(BrokerRole::Agent));
        assert!(!BrokerTool::SetSetting.is_offered_to(BrokerRole::Agent));
        for memory_tool in [
            BrokerTool::StoreMemory,
            BrokerTool::SearchMemory,
            BrokerTool::RecallMemory,
            BrokerTool::UpdateMemory,
            BrokerTool::ForgetMemory,
        ] {
            assert!(!memory_tool.is_offered_to(BrokerRole::Agent));
        }
    }

    /// A Memory's title, body and tags are the Sidekick's own words, so its
    /// Transcript keeps a call storing or changing one as it was made.
    #[test]
    fn a_memory_tools_call_stands_in_the_transcript_as_the_sidekick_made_it() {
        let stored = json!({
            "title": "How the user reviews",
            "body": "Never squash without asking.",
            "tags": ["review"],
        });
        for tool in [BrokerTool::StoreMemory, BrokerTool::UpdateMemory] {
            assert_eq!(*tool.recorded_arguments(&stored), stored);
        }
        assert!(BrokerTool::SearchMemory.is_read_only() && BrokerTool::RecallMemory.is_read_only());
        assert!(
            !BrokerTool::StoreMemory.is_read_only()
                && !BrokerTool::UpdateMemory.is_read_only()
                && !BrokerTool::ForgetMemory.is_read_only()
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
        let read =
            |arguments| named_subagent(BrokerTool::StopSubagent, &options(arguments), &STOP_TAKES);
        assert_eq!(read(json!({ "id": id })), Ok(id));
        for (value, says) in [
            (json!({}), "stop_subagent needs `id`"),
            (json!({ "id": null }), "stop_subagent needs `id`"),
            (json!({ "id": 7 }), "`id` must be the session_id"),
            (
                json!({ "id": "the researcher" }),
                "`id` must be the session_id",
            ),
            (
                json!({ "id": id, "force": true }),
                "stop_subagent takes no argument `force`; it takes `id`.",
            ),
        ] {
            let refusal = read(value)
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
        for argument in STOP_TAKES {
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
        assert_eq!(schema["required"], json!(STOP_TAKES));
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
    fn a_subagent_report_sends_its_agent_to_the_tool_that_reads_the_whole_message() {
        use crate::provider::{SubagentReport, SubagentReportOutcome};
        let subagent = SessionId::new();
        let long = "a".repeat(SubagentReport::EXCERPT_CHARS + 1);
        let report = SubagentReport::new(
            subagent,
            "Researcher",
            SubagentReportOutcome::Completed,
            Some(1_000),
            Some(&long),
            None,
        )
        .to_string();
        let read = BrokerTool::ReadSubagent.name();
        let send = BrokerTool::SendToSubagent.name();
        assert!(
            report.contains(&format!(
                "Its session_id is {subagent}, which {read} and {send} take."
            )),
            "the Report names the id the Broker's read and send take: {report}"
        );
        assert!(
            report.ends_with(&format!("{read} gives the whole Message.]")),
            "and, cut short, where the rest is: {report}"
        );
    }

    #[test]
    fn a_sidekick_report_sends_its_sidekick_to_the_tools_that_read_and_answer_the_session() {
        use crate::protocol::{Outlook, SessionReference};
        use crate::provider::{
            SidekickIntervention, SidekickReport, SidekickReportSubject, SidekickTurnOutcome,
        };
        let session = SessionReference::new(Outlook::Local, SessionId::new());
        let subject = SidekickReportSubject {
            session: session.clone(),
            title: "Fix the flaky test".to_owned(),
            subagent: None,
        };
        let read = BrokerTool::ReadSession.name();
        let answer = BrokerTool::AnswerQuestionnaire.name();
        let long = "a".repeat(SidekickReport::EXCERPT_CHARS + 1);
        let settled = SidekickReport::turn_settled(
            subject.clone(),
            SidekickTurnOutcome::Completed,
            Some(1_000),
            None,
            Some(("1.2".parse().expect("an entry number"), &long)),
        )
        .to_string();
        assert!(
            settled.contains(&format!(
                "Its session_id is {}, which {read} takes.",
                session.session_id
            )),
            "the Report names the id the Sidekick's read takes: {settled}"
        );
        assert!(
            settled.ends_with(&format!(
                "{read} with item \"1.2\" gives the whole Message.]"
            )),
            "and, cut short, the item that reads the rest: {settled}"
        );
        let asked = SidekickReport::intervention_owed(subject, SidekickIntervention::Questionnaire)
            .to_string();
        assert!(
            asked.contains(&format!(
                "{read} gives its Questions, and {answer} answers it."
            )),
            "a Questionnaire is read and answered by the Sidekick's own Tools: {asked}"
        );
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

    #[test]
    fn send_arguments_are_read_as_their_schema_gives_them() {
        let id = SessionId::new();
        assert_eq!(
            SendArguments::read(&options(json!({ "id": id, "message": "Go on." }))),
            Ok(SendArguments {
                id,
                message: "Go on.".to_owned(),
                summary: None,
            })
        );
        assert_eq!(
            SendArguments::read(&options(json!({
                "id": id,
                "message": "Go on.",
                "summary": " Carry on with the survey ",
            }))),
            Ok(SendArguments {
                id,
                message: "Go on.".to_owned(),
                summary: Some("Carry on with the survey".to_owned()),
            }),
            "a summary is read trimmed"
        );
        for value in [
            json!({ "id": id, "message": "Go on.", "summary": null }),
            json!({ "id": id, "message": "Go on.", "summary": " \n" }),
        ] {
            assert_eq!(
                SendArguments::read(&options(value.clone())),
                Ok(SendArguments {
                    id,
                    message: "Go on.".to_owned(),
                    summary: None,
                }),
                "a summary that says nothing is none: {value}"
            );
        }
        for (value, says) in [
            (
                json!({ "message": "Go on." }),
                "send_to_subagent needs `id`",
            ),
            (json!({ "id": id }), "send_to_subagent needs `message`"),
            (
                json!({ "id": id, "message": 7 }),
                "`message` must be a string",
            ),
            (json!({ "id": id, "message": " \n" }), "`message` is empty"),
            (
                json!({ "id": "Researcher", "message": "Go on." }),
                "`id` must be the session_id",
            ),
            (
                json!({ "id": id, "message": "Go on.", "summary": ["Go", "on"] }),
                "`summary` must be a string",
            ),
            (
                json!({ "id": id, "message": "Go on.", "urgent": true }),
                "send_to_subagent takes no argument `urgent`; it takes `id`, `message`, \
                 `summary`.",
            ),
        ] {
            let refusal = SendArguments::read(&options(value))
                .expect_err("the arguments are refused")
                .to_string();
            assert!(refusal.contains(says), "{says:?} is said in {refusal:?}");
        }
    }

    #[test]
    fn wait_arguments_name_the_subagents_to_wait_on_and_keep_a_bounded_timeout() {
        let (first, second) = (SessionId::new(), SessionId::new());
        let read = |value| WaitArguments::read(&options(value));
        for (value, ids, timeout_seconds) in [
            (json!({}), None, 60),
            (json!({ "ids": null, "timeout_seconds": null }), None, 60),
            (json!({ "ids": [] }), None, 60),
            (
                json!({ "ids": [first, second, first], "timeout_seconds": 5 }),
                Some(vec![first, second]),
                10,
            ),
            (json!({ "timeout_seconds": 1e9 }), None, 600),
            (json!({ "timeout_seconds": 120.2 }), None, 120),
        ] {
            assert_eq!(
                read(value.clone()),
                Ok(WaitArguments {
                    ids: ids.clone(),
                    timeout_seconds,
                }),
                "{value}"
            );
        }
        for (value, says) in [
            (
                json!({ "ids": first }),
                "`ids` must be a list of the session_ids",
            ),
            (json!({ "ids": [7] }), "7 is not a session_id"),
            (
                json!({ "timeout_seconds": "a minute" }),
                "`timeout_seconds` must be a number of seconds, from 10 to 600",
            ),
            (
                json!({ "until": "settled" }),
                "wait_subagents takes no argument `until`; it takes `ids`, `timeout_seconds`.",
            ),
        ] {
            let refusal = read(value)
                .expect_err("the arguments are refused")
                .to_string();
            assert!(refusal.contains(says), "{says:?} is said in {refusal:?}");
        }
    }

    #[test]
    fn send_and_wait_describe_what_they_take_and_the_shape_they_answer_with() {
        for (tool, description, takes, answers_with) in [
            (
                BrokerTool::SendToSubagent,
                SEND_TO_SUBAGENT_DESCRIPTION,
                &SendArguments::TAKES[..],
                &["session_id", "delivered"][..],
            ),
            (
                BrokerTool::WaitSubagents,
                WAIT_SUBAGENTS_DESCRIPTION,
                &WaitArguments::TAKES[..],
                &[
                    "settled",
                    "timed_out",
                    "timeout_seconds",
                    "reason",
                    "session_id",
                    "status",
                    "duration_ms",
                    "message",
                ][..],
            ),
        ] {
            let schema = tool.input_schema();
            for argument in takes {
                assert!(
                    schema["properties"]
                        .as_object()
                        .is_some_and(|properties| properties.contains_key(*argument)),
                    "{} takes {argument}",
                    tool.name()
                );
                assert!(
                    description.contains(&format!("\"{argument}\"")),
                    "{} says what {argument} is",
                    tool.name()
                );
            }
            for field in answers_with {
                assert!(
                    description.contains(&format!("\"{field}\"")),
                    "{} gives the shape of {field}",
                    tool.name()
                );
            }
            assert_eq!(schema["additionalProperties"], json!(false));
        }
        for delivered in [Delivered::Resumed, Delivered::Steered] {
            let spelled = serde_json::to_value(delivered)
                .expect("a delivery serializes")
                .to_string();
            assert!(
                SEND_TO_SUBAGENT_DESCRIPTION.contains(&spelled),
                "send_to_subagent's description names {spelled}"
            );
        }
        assert_eq!(
            BrokerTool::SendToSubagent.input_schema()["required"],
            json!(SendArguments::REQUIRED),
            "a send must name the Subagent and the message; its summary may be left out"
        );
        assert!(
            SendArguments::REQUIRED
                .iter()
                .all(|argument| SendArguments::TAKES.contains(argument)),
            "and everything a send must name, it takes"
        );
        assert_eq!(
            BrokerTool::WaitSubagents.input_schema().get("required"),
            None,
            "a wait may name nothing it takes"
        );
        assert!(!BrokerTool::SendToSubagent.is_read_only());
        assert!(
            BrokerTool::WaitSubagents.is_read_only(),
            "a wait changes nothing Suru holds"
        );
    }

    #[test]
    fn a_subagent_out_of_reach_is_refused_saying_which_subagents_each_tool_reaches() {
        let subagent = SessionId::new();
        for (tool, says) in [
            (BrokerTool::ReadSubagent, "read_subagent reads only those."),
            (
                BrokerTool::SendToSubagent,
                "send_to_subagent sends only to those.",
            ),
            (
                BrokerTool::WaitSubagents,
                "wait_subagents waits only on those.",
            ),
        ] {
            let refusal =
                unreachable_refusal(tool, BrokeredReadError::NotBrokeredBeneathCaller, subagent)
                    .to_string();
            assert!(
                refusal.starts_with(&format!(
                    "`{subagent}` is not a Subagent spawned with spawn_subagent by you or by a \
                     Subagent beneath you; "
                )) && refusal.ends_with(says),
                "{refusal}"
            );
        }
    }

    /// A cap is refused in words naming the Setting that pins it — one the
    /// schema declares, so the Agent may send the user straight to it — and
    /// read naturally at a cap of one.
    #[test]
    fn a_spawn_past_a_cap_is_refused_naming_the_setting_that_pins_it() {
        let depth = spawn_refusal(BrokeredSpawnRefusal::Capped(BrokeredSpawnCap::Depth {
            max_depth: 1,
            depth: 2,
        }))
        .to_string();
        assert_eq!(
            depth,
            "Suru's Broker lets Subagents stand at most 1 Session deep, counting the top-level \
             Session as the first (`broker.maxDepth`), and one spawned here would stand 2 deep, \
             so nothing was spawned. Do this work yourself, or ask the user to raise the Setting."
        );
        let concurrency = spawn_refusal(BrokeredSpawnRefusal::Capped(
            BrokeredSpawnCap::Concurrency {
                max_concurrent_subagents: 1,
                working: 1,
            },
        ))
        .to_string();
        assert_eq!(
            concurrency,
            "Suru's Broker lets at most 1 brokered Subagent work at once beneath a top-level \
             Session (`broker.maxConcurrentSubagents`), and 1 is working now, so nothing was \
             spawned. Call wait_subagents to wait for one to settle, or ask the user to raise the \
             Setting."
        );
        assert!(
            concurrency.contains(BrokerTool::WaitSubagents.name()),
            "the Agent is sent to the Tool that waits for room"
        );
        let resume = send_refusal(
            BrokeredSendRefusal::Capped(BrokeredSpawnCap::Concurrency {
                max_concurrent_subagents: 2,
                working: 3,
            }),
            SessionId::new(),
        )
        .to_string();
        assert!(
            resume.contains(
                "and 3 are working now, so the Subagent was not resumed. Call wait_subagents"
            ),
            "a resume past the cap says it did not resume: {resume}"
        );
        for (refusal, key) in [
            (&depth, "broker.maxDepth"),
            (&concurrency, "broker.maxConcurrentSubagents"),
        ] {
            assert!(refusal.contains(&format!("`{key}`")), "{refusal}");
            assert!(
                crate::settings::SCHEMA
                    .iter()
                    .any(|descriptor| descriptor.key == key),
                "the refusal names {key}, which the schema declares"
            );
        }
        assert_eq!(
            spawn_refusal(BrokeredSpawnRefusal::Refused("Not today.".to_owned())).to_string(),
            "Not today.",
            "any other refusal is passed on in the words it came in"
        );
    }
}
