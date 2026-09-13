//! The persistence DTO layer: the row shape of each stored table, and its conversions to and from
//! the domain types in [`crate::protocol`].
//!
//! Nothing here touches a connection. Each row type owns both directions of its own mapping —
//! `from_*` on the way down, `into_*` on the way back — along with the encoding helpers those
//! conversions share, leaving the storage root to connections, migrations, and queries.

use std::{collections::HashMap, fmt, path::PathBuf};

use diesel::prelude::*;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use uuid::Uuid;

use crate::{
    model_catalog::RememberedProviderCatalog,
    protocol::{
        Activity, ActivityId, ActivityStatus, AgentId, AgentIdentity, AgentSelection, Cost,
        CostBasis, FileChange, Message, MessageId, MessageRole, MessageStatus, ModelDescriptor,
        ModelId, ModelOptionChoiceId, ModelOptionId, ModelOptionSelection, ModelOptionValue,
        Prompt, PromptDelivery, PromptId, PromptOrder, PromptStatus, ProviderId, Session,
        SessionId, SessionRevision, SessionStandingInputs, SessionSummary, SessionTimestamp,
        SkillInvocation, TranscriptItem, Turn, TurnId, TurnStatus, UnreadableSessionSummary, Usage,
        Workspace,
    },
    provider::ProviderResumeState,
};

use super::{
    PersistedSession, StorageError, StoredResumeState, activities, landing_agent_selection,
    messages, model_catalog, prompts, provider_resume_states, sessions, turns,
};

/// One Provider's remembered Model Catalog: the Models and warning it last
/// served, as one JSON payload, beside when it served them.
#[derive(AsChangeset, Insertable, Queryable, Selectable)]
#[diesel(table_name = model_catalog)]
pub(super) struct ModelCatalogRow {
    provider: String,
    payload: String,
    discovered_at: i64,
}

#[derive(Deserialize, Serialize)]
struct ModelCatalogPayload {
    models: Vec<ModelDescriptor>,
    warning: Option<String>,
}

impl ModelCatalogRow {
    pub(super) fn from_remembered(
        remembered: RememberedProviderCatalog,
    ) -> Result<Self, StorageError> {
        Ok(Self {
            provider: remembered.provider.to_string(),
            payload: serde_json::to_string(&ModelCatalogPayload {
                models: remembered.models,
                warning: remembered.warning,
            })
            .map_err(|error| StorageError::WriteModelCatalog(error.to_string()))?,
            discovered_at: i64::try_from(remembered.discovered_at.0).unwrap_or(i64::MAX),
        })
    }

    /// Nothing for a payload this binary cannot read: a remembered catalog is
    /// best-effort, and the Provider is asked again on the next connect.
    pub(super) fn into_remembered(self) -> Option<RememberedProviderCatalog> {
        let payload: ModelCatalogPayload = serde_json::from_str(&self.payload).ok()?;
        Some(RememberedProviderCatalog {
            provider: ProviderId::new(self.provider),
            models: payload.models,
            warning: payload.warning,
            discovered_at: SessionTimestamp(u64::try_from(self.discovered_at).unwrap_or(0)),
        })
    }
}

#[derive(AsChangeset, Insertable, Queryable, Selectable)]
#[diesel(table_name = landing_agent_selection)]
pub(super) struct LandingAgentSelectionRow {
    singleton: i32,
    selection: String,
}

impl LandingAgentSelectionRow {
    pub(super) fn from_selection(selection: AgentSelection) -> Result<Self, StorageError> {
        Ok(Self {
            singleton: 1,
            selection: serde_json::to_string(&selection)
                .map_err(|error| StorageError::WriteLandingAgentSelection(error.to_string()))?,
        })
    }

    pub(super) fn into_selection(self) -> Option<AgentSelection> {
        serde_json::from_str(&self.selection).ok()
    }
}

#[derive(AsChangeset, Insertable, Queryable, Selectable)]
#[diesel(table_name = sessions, treat_none_as_null = true)]
pub(super) struct SessionRow {
    pub(super) id: String,
    title: String,
    emoji: Option<String>,
    settled_at: Option<i64>,
    viewed_at: Option<i64>,
    created_at: i64,
    updated_at: i64,
    workspace: String,
    agent_selection: Option<String>,
    agent_selection_availability: String,
    status: String,
    revision: i64,
    parent_session_id: Option<String>,
    context_fill: Option<String>,
}

#[derive(Insertable, Queryable, Selectable)]
#[diesel(table_name = prompts)]
pub(super) struct PromptRow {
    id: String,
    session_id: String,
    row_order: i64,
    admission_order: i64,
    payload: String,
}

#[derive(Insertable, Queryable, Selectable)]
#[diesel(table_name = turns)]
pub(super) struct TurnRow {
    id: String,
    pub(super) session_id: String,
    prompt_id: Option<String>,
    row_order: i64,
    payload: String,
}

#[derive(Insertable, Queryable, Selectable)]
#[diesel(table_name = messages)]
pub(super) struct MessageRow {
    id: String,
    session_id: String,
    turn_id: String,
    row_order: i64,
    transcript_order: i64,
    payload: String,
}

#[derive(Insertable, Queryable, Selectable)]
#[diesel(table_name = activities)]
pub(super) struct ActivityRow {
    id: String,
    session_id: String,
    turn_id: String,
    row_order: i64,
    transcript_order: i64,
    payload: String,
}

#[derive(Insertable, Queryable, Selectable)]
#[diesel(table_name = provider_resume_states)]
pub(super) struct ProviderResumeStateRow {
    session_id: String,
    provider: String,
    pub(super) payload: String,
}

impl ProviderResumeStateRow {
    pub(super) fn from_resume_state(state: &StoredResumeState) -> Result<Self, StorageError> {
        Ok(Self {
            session_id: state.session_id.to_string(),
            provider: state.provider.to_string(),
            payload: encode(
                state.session_id,
                "Resume State",
                state.resume_state.payload(),
            )?,
        })
    }

    pub(super) fn into_resume_state(
        self,
    ) -> Result<(ProviderId, ProviderResumeState), StorageError> {
        let payload = decode::<serde_json::Value>(&self.session_id, "Resume State", &self.payload)?;
        Ok((
            ProviderId::new(self.provider),
            ProviderResumeState::new(payload),
        ))
    }
}

pub(super) struct StoredRows {
    pub(super) session_id: SessionId,
    pub(super) session: SessionRow,
    pub(super) prompts: Vec<PromptRow>,
    pub(super) turns: Vec<TurnRow>,
    pub(super) messages: Vec<MessageRow>,
    pub(super) activities: Vec<ActivityRow>,
}

#[derive(Clone, Copy)]
struct RowPosition<'a> {
    session_id: SessionId,
    stored_session_id: &'a str,
    row_order: usize,
}

impl<'a> RowPosition<'a> {
    fn new(session_id: SessionId, stored_session_id: &'a str, row_order: usize) -> Self {
        Self {
            session_id,
            stored_session_id,
            row_order,
        }
    }
}

#[derive(Clone, Copy)]
struct TranscriptPosition<'a> {
    row: RowPosition<'a>,
    transcript_order: usize,
}

impl StoredRows {
    pub(super) fn from_session(persisted: PersistedSession) -> Result<Self, StorageError> {
        let PersistedSession {
            summary,
            snapshot,
            resume_states: _,
        } = persisted;
        let session_id = snapshot.session.id;
        let id = session_id.to_string();
        let transcript_order = snapshot
            .transcript
            .iter()
            .enumerate()
            .map(|(order, item)| (transcript_identity(*item), order))
            .collect::<HashMap<_, _>>();
        let session = SessionRow::from_parts(summary, snapshot.revision)?;
        let prompts = snapshot
            .prompts
            .into_iter()
            .enumerate()
            .map(|(order, prompt)| {
                PromptRow::from_prompt(RowPosition::new(session_id, &id, order), prompt)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let turns = snapshot
            .turns
            .into_iter()
            .enumerate()
            .map(|(order, turn)| TurnRow::from_turn(RowPosition::new(session_id, &id, order), turn))
            .collect::<Result<Vec<_>, _>>()?;
        let messages = snapshot
            .messages
            .into_iter()
            .enumerate()
            .map(|(order, message)| {
                let presentation = transcript_order
                    .get(&TranscriptIdentity::Message(message.id))
                    .copied()
                    .ok_or_else(|| invalid_session(&id, "Transcript", "Message is missing"))?;
                MessageRow::from_message(
                    TranscriptPosition {
                        row: RowPosition::new(session_id, &id, order),
                        transcript_order: presentation,
                    },
                    message,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let activities = snapshot
            .activities
            .into_iter()
            .enumerate()
            .map(|(order, activity)| {
                let presentation = transcript_order
                    .get(&TranscriptIdentity::Activity(activity.id()))
                    .copied()
                    .ok_or_else(|| invalid_session(&id, "Transcript", "Activity is missing"))?;
                ActivityRow::from_activity(
                    TranscriptPosition {
                        row: RowPosition::new(session_id, &id, order),
                        transcript_order: presentation,
                    },
                    activity,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            session_id,
            session,
            prompts,
            turns,
            messages,
            activities,
        })
    }
}

impl SessionRow {
    fn from_parts(
        summary: SessionSummary,
        revision: SessionRevision,
    ) -> Result<Self, StorageError> {
        let session_id = summary.session.id;
        Ok(Self {
            id: session_id.to_string(),
            title: summary.title,
            emoji: summary.emoji,
            settled_at: summary
                .settled_at
                .map(|settled_at| u64_to_i64(session_id, "settled_at", settled_at.0))
                .transpose()?,
            viewed_at: summary
                .standing_inputs
                .viewed_at
                .map(|viewed_at| u64_to_i64(session_id, "viewed_at", viewed_at.0))
                .transpose()?,
            created_at: u64_to_i64(session_id, "created_at", summary.created_at.0)?,
            updated_at: u64_to_i64(session_id, "updated_at", summary.updated_at.0)?,
            workspace: Self::location_payload(&summary.session)?,
            agent_selection: summary
                .session
                .agent_selection
                .map(StoredAgentSelection::from)
                .as_ref()
                .map(|selection| encode(session_id, "Agent Selection", selection))
                .transpose()?,
            agent_selection_availability: encode(
                session_id,
                "Agent Selection availability",
                &summary.session.agent_selection_availability,
            )?,
            status: encode(session_id, "Session status", &summary.session.status)?,
            revision: u64_to_i64(session_id, "revision", revision.0)?,
            parent_session_id: summary.session.parent.map(|parent| parent.to_string()),
            context_fill: summary
                .session
                .context_fill
                .as_ref()
                .map(|fill| encode(session_id, "Context Fill", fill))
                .transpose()?,
        })
    }

    pub(super) fn location_payload(session: &Session) -> Result<String, StorageError> {
        encode(
            session.id,
            "Session location",
            &StoredSessionLocation {
                path: session.workspace.path.clone(),
                workspace: Some(session.workspace.clone()),
                execution_directory: Some(session.execution_directory.clone()),
                checkout: session.checkout.clone(),
            },
        )
    }

    pub(super) fn into_summary_and_revision(
        self,
    ) -> Result<(SessionSummary, SessionRevision), StorageError> {
        let session_id = self.id.clone();
        let id = parse_id(&session_id, "Session ID", SessionId::from_uuid)?;
        let workspace: StoredSessionLocation = decode(&session_id, "Workspace", &self.workspace)?;
        let agent_selection = self
            .agent_selection
            .as_deref()
            .map(|value| decode::<StoredAgentSelection>(&session_id, "Agent Selection", value))
            .transpose()?
            .map(AgentSelection::from);
        let summary = SessionSummary {
            checkout_state: None,
            session: Session {
                checkout: workspace.checkout.clone(),
                context_fill: self
                    .context_fill
                    .as_deref()
                    .map(|fill| decode(&session_id, "Context Fill", fill))
                    .transpose()?,
                id,
                execution_directory: workspace.execution_directory(),
                workspace: workspace.into(),
                agent_selection,
                agent_selection_availability: decode(
                    &session_id,
                    "Agent Selection availability",
                    &self.agent_selection_availability,
                )?,
                status: decode(&session_id, "Session status", &self.status)?,
                working_since: None,
                parent: self
                    .parent_session_id
                    .as_deref()
                    .map(|parent| parse_id(parent, "parent Session ID", SessionId::from_uuid))
                    .transpose()?,
            },
            title: self.title,
            emoji: self.emoji,
            settled_at: self
                .settled_at
                .map(|settled_at| i64_to_u64(&session_id, "settled_at", settled_at))
                .transpose()?
                .map(SessionTimestamp),
            standing_inputs: SessionStandingInputs {
                pending_questionnaires: Vec::new(),
                submitting_questionnaires: Vec::new(),
                pending_questionnaires_revision: crate::protocol::SessionRevision(0),
                pending_approvals: Vec::new(),
                submitting_approvals: Vec::new(),
                pending_approvals_revision: crate::protocol::SessionRevision(0),
                subagent_interventions: Vec::new(),
                latest_turn: None,
                viewed_at: self
                    .viewed_at
                    .map(|viewed_at| i64_to_u64(&session_id, "viewed_at", viewed_at))
                    .transpose()?
                    .map(SessionTimestamp),
            },
            total_usage: None,
            created_at: SessionTimestamp(i64_to_u64(&session_id, "created_at", self.created_at)?),
            updated_at: SessionTimestamp(i64_to_u64(&session_id, "updated_at", self.updated_at)?),
        };
        let revision = SessionRevision(i64_to_u64(&session_id, "revision", self.revision)?);
        Ok((summary, revision))
    }

    pub(super) fn is_child(&self) -> bool {
        self.parent_session_id.is_some()
    }

    pub(super) fn parent_id(&self) -> Result<Option<SessionId>, StorageError> {
        self.parent_session_id
            .as_deref()
            .map(|id| parse_id(id, "parent Session ID", SessionId::from_uuid))
            .transpose()
    }

    pub(super) fn unreadable_summary(&self) -> Result<UnreadableSessionSummary, StorageError> {
        let session_id = self.id.clone();
        Ok(UnreadableSessionSummary {
            id: parse_id(&session_id, "Session ID", SessionId::from_uuid)?,
            title: self.title.clone(),
            created_at: SessionTimestamp(i64_to_u64(&session_id, "created_at", self.created_at)?),
            updated_at: SessionTimestamp(i64_to_u64(&session_id, "updated_at", self.updated_at)?),
            workspace: serde_json::from_str::<StoredSessionLocation>(&self.workspace)
                .ok()
                .map(Workspace::from),
        })
    }
}

impl PromptRow {
    fn from_prompt(position: RowPosition<'_>, prompt: Prompt) -> Result<Self, StorageError> {
        Ok(Self {
            id: prompt.id.to_string(),
            session_id: position.stored_session_id.to_owned(),
            row_order: usize_to_i64(position.session_id, "Prompt row order", position.row_order)?,
            admission_order: u64_to_i64(
                position.session_id,
                "Prompt admission order",
                prompt.admission_order.0,
            )?,
            payload: encode(
                position.session_id,
                "Prompt payload",
                &StoredPromptPayload {
                    text: prompt.text,
                    skill_invocations: prompt.skill_invocations,
                    delivery: prompt.delivery,
                    status: prompt.status,
                },
            )?,
        })
    }

    pub(super) fn into_prompt(self) -> Result<Prompt, StorageError> {
        let session_id = self.session_id;
        let payload: StoredPromptPayload = decode(&session_id, "Prompt payload", &self.payload)?;
        Ok(Prompt {
            id: parse_id(&self.id, "Prompt ID", PromptId::from_uuid)?,
            text: payload.text,
            skill_invocations: payload.skill_invocations,
            delivery: payload.delivery,
            admission_order: PromptOrder(i64_to_u64(
                &session_id,
                "Prompt admission order",
                self.admission_order,
            )?),
            status: payload.status,
        })
    }
}

impl TurnRow {
    fn from_turn(position: RowPosition<'_>, turn: Turn) -> Result<Self, StorageError> {
        Ok(Self {
            id: turn.id.to_string(),
            session_id: position.stored_session_id.to_owned(),
            prompt_id: turn.prompt_id.map(|prompt_id| prompt_id.to_string()),
            row_order: usize_to_i64(position.session_id, "Turn row order", position.row_order)?,
            payload: encode(
                position.session_id,
                "Turn payload",
                &StoredTurnPayload {
                    agent: turn.agent.map(StoredAgentIdentity::from),
                    status: turn.status,
                    started_at: turn.started_at,
                    settled_at: turn.settled_at,
                    last_output_at: turn.last_output_at,
                    usage: turn.usage,
                    cost: turn.cost,
                    cost_basis: turn.cost_basis,
                    cost_details: turn.cost_details,
                },
            )?,
        })
    }

    pub(super) fn into_turn(self) -> Result<Turn, StorageError> {
        let session_id = self.session_id;
        let payload: StoredTurnPayload = decode(&session_id, "Turn payload", &self.payload)?;
        let turn = Turn {
            id: parse_id(&self.id, "Turn ID", TurnId::from_uuid)?,
            prompt_id: self
                .prompt_id
                .as_deref()
                .map(|prompt_id| parse_id(prompt_id, "Turn Prompt ID", PromptId::from_uuid))
                .transpose()?,
            agent: payload.agent.map(AgentIdentity::from),
            status: payload.status,
            started_at: payload.started_at,
            settled_at: payload.settled_at,
            last_output_at: payload.last_output_at,
            usage: payload.usage,
            cost: payload.cost,
            cost_basis: payload.cost_basis,
            cost_details: payload.cost_details,
        };
        if !turn.has_valid_cost_attribution() {
            return Err(StorageError::InvalidSession {
                session_id,
                message: "Turn payload has a Cost without exactly one Cost Basis".to_owned(),
            });
        }
        Ok(turn)
    }
}

impl MessageRow {
    fn from_message(
        position: TranscriptPosition<'_>,
        message: Message,
    ) -> Result<Self, StorageError> {
        Ok(Self {
            id: message.id.to_string(),
            session_id: position.row.stored_session_id.to_owned(),
            turn_id: message.turn_id.to_string(),
            row_order: usize_to_i64(
                position.row.session_id,
                "Message row order",
                position.row.row_order,
            )?,
            transcript_order: usize_to_i64(
                position.row.session_id,
                "Transcript order",
                position.transcript_order,
            )?,
            payload: encode(
                position.row.session_id,
                "Message payload",
                &StoredMessagePayload {
                    role: message.role,
                    status: message.status,
                    content: message.content,
                    skill_invocations: message.skill_invocations,
                    truncated: message.truncated,
                },
            )?,
        })
    }

    pub(super) fn into_message(self) -> Result<(Message, i64), StorageError> {
        let session_id = self.session_id;
        let payload: StoredMessagePayload = decode(&session_id, "Message payload", &self.payload)?;
        Ok((
            Message {
                id: parse_id(&self.id, "Message ID", MessageId::from_uuid)?,
                turn_id: parse_id(&self.turn_id, "Message Turn ID", TurnId::from_uuid)?,
                role: payload.role,
                status: payload.status,
                content: payload.content,
                skill_invocations: payload.skill_invocations,
                truncated: payload.truncated,
            },
            self.transcript_order,
        ))
    }
}

impl ActivityRow {
    fn from_activity(
        position: TranscriptPosition<'_>,
        activity: Activity,
    ) -> Result<Self, StorageError> {
        let id = activity.id();
        let turn_id = activity.turn_id();
        Ok(Self {
            id: id.to_string(),
            session_id: position.row.stored_session_id.to_owned(),
            turn_id: turn_id.to_string(),
            row_order: usize_to_i64(
                position.row.session_id,
                "Activity row order",
                position.row.row_order,
            )?,
            transcript_order: usize_to_i64(
                position.row.session_id,
                "Transcript order",
                position.transcript_order,
            )?,
            payload: encode(
                position.row.session_id,
                "Activity payload",
                &StoredActivityPayload::from(activity),
            )?,
        })
    }

    pub(super) fn into_activity(self) -> Result<(Activity, i64), StorageError> {
        let session_id = self.session_id;
        let id = parse_id(&self.id, "Activity ID", ActivityId::from_uuid)?;
        let turn_id = parse_id(&self.turn_id, "Activity Turn ID", TurnId::from_uuid)?;
        let payload: StoredActivityPayload =
            decode(&session_id, "Activity payload", &self.payload)?;
        Ok((payload.into_activity(id, turn_id), self.transcript_order))
    }
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum TranscriptIdentity {
    Message(MessageId),
    Activity(ActivityId),
}

fn transcript_identity(item: TranscriptItem) -> TranscriptIdentity {
    match item {
        TranscriptItem::Message { message_id } => TranscriptIdentity::Message(message_id),
        TranscriptItem::Activity { activity_id } => TranscriptIdentity::Activity(activity_id),
    }
}

/// Session location metadata stays readable without opening its Transcript.
/// Path-only records predate the grouping/execution split and retain that exact path.
#[derive(Deserialize, Serialize)]
struct StoredSessionLocation {
    path: PathBuf,
    #[serde(default)]
    workspace: Option<Workspace>,
    #[serde(default)]
    checkout: Option<crate::protocol::CheckoutAssociation>,
    #[serde(default)]
    execution_directory: Option<crate::protocol::ExecutionDirectory>,
}

impl StoredSessionLocation {
    fn execution_directory(&self) -> crate::protocol::ExecutionDirectory {
        self.execution_directory
            .clone()
            .unwrap_or_else(|| crate::protocol::ExecutionDirectory {
                path: self.path.clone(),
            })
    }
}

impl From<StoredSessionLocation> for Workspace {
    fn from(workspace: StoredSessionLocation) -> Self {
        workspace
            .workspace
            .unwrap_or_else(|| Workspace::directory(workspace.path))
    }
}

#[derive(Deserialize, Serialize)]
struct StoredAgentIdentity {
    agent: AgentId,
    selection: StoredAgentSelection,
}

impl From<AgentIdentity> for StoredAgentIdentity {
    fn from(identity: AgentIdentity) -> Self {
        Self {
            agent: identity.agent,
            selection: identity.selection.into(),
        }
    }
}

impl From<StoredAgentIdentity> for AgentIdentity {
    fn from(identity: StoredAgentIdentity) -> Self {
        Self {
            agent: identity.agent,
            selection: identity.selection.into(),
        }
    }
}

#[derive(Deserialize, Serialize)]
struct StoredAgentSelection {
    provider: ProviderId,
    model: ModelId,
    options: Vec<StoredModelOptionSelection>,
}

impl From<AgentSelection> for StoredAgentSelection {
    fn from(selection: AgentSelection) -> Self {
        Self {
            provider: selection.provider,
            model: selection.model,
            options: selection.options.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<StoredAgentSelection> for AgentSelection {
    fn from(selection: StoredAgentSelection) -> Self {
        Self {
            provider: selection.provider,
            model: selection.model,
            options: selection.options.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Deserialize, Serialize)]
struct StoredModelOptionSelection {
    id: ModelOptionId,
    value: StoredModelOptionValue,
}

impl From<ModelOptionSelection> for StoredModelOptionSelection {
    fn from(selection: ModelOptionSelection) -> Self {
        Self {
            id: selection.id,
            value: selection.value.into(),
        }
    }
}

impl From<StoredModelOptionSelection> for ModelOptionSelection {
    fn from(selection: StoredModelOptionSelection) -> Self {
        Self {
            id: selection.id,
            value: selection.value.into(),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StoredModelOptionValue {
    Select { choice: ModelOptionChoiceId },
    Toggle { enabled: bool },
}

impl From<ModelOptionValue> for StoredModelOptionValue {
    fn from(value: ModelOptionValue) -> Self {
        match value {
            ModelOptionValue::Select { choice } => Self::Select { choice },
            ModelOptionValue::Toggle { enabled } => Self::Toggle { enabled },
        }
    }
}

impl From<StoredModelOptionValue> for ModelOptionValue {
    fn from(value: StoredModelOptionValue) -> Self {
        match value {
            StoredModelOptionValue::Select { choice } => Self::Select { choice },
            StoredModelOptionValue::Toggle { enabled } => Self::Toggle { enabled },
        }
    }
}

#[derive(Deserialize, Serialize)]
struct StoredPromptPayload {
    text: String,
    #[serde(default)]
    skill_invocations: Vec<SkillInvocation>,
    delivery: PromptDelivery,
    status: PromptStatus,
}

#[derive(Deserialize, Serialize)]
struct StoredTurnPayload {
    agent: Option<StoredAgentIdentity>,
    status: TurnStatus,
    /// Absent in every Turn stored before Suru recorded Turn timing, which is
    /// why these decode as `None` rather than needing a schema migration.
    #[serde(default)]
    started_at: Option<SessionTimestamp>,
    #[serde(default)]
    settled_at: Option<SessionTimestamp>,
    #[serde(default)]
    last_output_at: Option<SessionTimestamp>,
    /// Absent in every Turn stored before usage tracking; defaulted fields
    /// keep those payloads readable without a schema migration.
    #[serde(default)]
    usage: Option<Usage>,
    #[serde(default)]
    cost: Option<Cost>,
    #[serde(default)]
    cost_basis: Option<CostBasis>,
    #[serde(default)]
    cost_details: Option<crate::protocol::CostDetails>,
}

#[derive(Deserialize, Serialize)]
struct StoredMessagePayload {
    role: MessageRole,
    status: MessageStatus,
    content: String,
    #[serde(default)]
    skill_invocations: Vec<SkillInvocation>,
    truncated: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredActivityPayload {
    Approval {
        approval: crate::protocol::Approval,
        tool_activity_id: Option<ActivityId>,
        #[serde(default)]
        detail_truncated: bool,
        outcome: crate::protocol::ApprovalOutcome,
        decision: Option<crate::protocol::Decision>,
    },
    Questionnaire {
        questionnaire: crate::protocol::Questionnaire,
        outcome: crate::protocol::QuestionnaireOutcome,
        answer: Option<crate::protocol::Answer>,
    },
    Status {
        text: String,
    },
    Error {
        text: String,
    },
    Command {
        status: ActivityStatus,
        command: String,
        cwd: Option<PathBuf>,
        output: String,
        output_truncated: bool,
        exit_status: Option<i32>,
    },
    FileChange {
        status: ActivityStatus,
        changes: Vec<StoredFileChange>,
    },
    Reasoning {
        status: ActivityStatus,
        title: Option<String>,
        content: String,
        content_truncated: bool,
        duration_ms: Option<u64>,
    },
    Subagent {
        status: ActivityStatus,
        name: String,
        description: String,
        #[serde(default)]
        model: Option<ModelId>,
        session_id: SessionId,
        duration_ms: Option<u64>,
    },
}

impl StoredActivityPayload {
    fn into_activity(self, id: ActivityId, turn_id: TurnId) -> Activity {
        match self {
            Self::Approval {
                approval,
                tool_activity_id,
                detail_truncated,
                outcome,
                decision,
            } => Activity::Approval {
                id,
                turn_id,
                approval,
                tool_activity_id,
                detail_truncated,
                outcome,
                decision,
            },
            Self::Questionnaire {
                questionnaire,
                outcome,
                answer,
            } => Activity::Questionnaire {
                id,
                turn_id,
                questionnaire,
                outcome,
                answer,
            },
            Self::Status { text } => Activity::Status { id, turn_id, text },
            Self::Error { text } => Activity::Error { id, turn_id, text },
            Self::Command {
                status,
                command,
                cwd,
                output,
                output_truncated,
                exit_status,
            } => Activity::Command {
                id,
                turn_id,
                status,
                command,
                cwd,
                output,
                output_truncated,
                exit_status,
            },
            Self::FileChange { status, changes } => Activity::FileChange {
                id,
                turn_id,
                status,
                changes: changes.into_iter().map(Into::into).collect(),
            },
            Self::Reasoning {
                status,
                title,
                content,
                content_truncated,
                duration_ms,
            } => Activity::Reasoning {
                id,
                turn_id,
                status,
                title,
                content,
                content_truncated,
                duration_ms,
            },
            Self::Subagent {
                status,
                name,
                description,
                model,
                session_id,
                duration_ms,
            } => Activity::Subagent {
                id,
                turn_id,
                status,
                name,
                description,
                model,
                session_id,
                duration_ms,
            },
        }
    }
}

impl From<Activity> for StoredActivityPayload {
    fn from(activity: Activity) -> Self {
        match activity {
            Activity::Approval {
                approval,
                tool_activity_id,
                detail_truncated,
                outcome,
                decision,
                ..
            } => Self::Approval {
                approval,
                tool_activity_id,
                detail_truncated,
                outcome,
                decision,
            },
            Activity::Questionnaire {
                questionnaire,
                outcome,
                answer,
                ..
            } => Self::Questionnaire {
                questionnaire,
                outcome,
                answer,
            },
            Activity::Status { text, .. } => Self::Status { text },
            Activity::Error { text, .. } => Self::Error { text },
            Activity::Command {
                status,
                command,
                cwd,
                output,
                output_truncated,
                exit_status,
                ..
            } => Self::Command {
                status,
                command,
                cwd,
                output,
                output_truncated,
                exit_status,
            },
            Activity::FileChange {
                status, changes, ..
            } => Self::FileChange {
                status,
                changes: changes.into_iter().map(Into::into).collect(),
            },
            Activity::Reasoning {
                status,
                title,
                content,
                content_truncated,
                duration_ms,
                ..
            } => Self::Reasoning {
                status,
                title,
                content,
                content_truncated,
                duration_ms,
            },
            Activity::Subagent {
                status,
                name,
                description,
                model,
                session_id,
                duration_ms,
                ..
            } => Self::Subagent {
                status,
                name,
                description,
                model,
                session_id,
                duration_ms,
            },
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredFileChange {
    Add {
        path: PathBuf,
    },
    Delete {
        path: PathBuf,
    },
    Update {
        path: PathBuf,
        moved_to: Option<PathBuf>,
    },
}

impl From<FileChange> for StoredFileChange {
    fn from(change: FileChange) -> Self {
        match change {
            FileChange::Add { path } => Self::Add { path },
            FileChange::Delete { path } => Self::Delete { path },
            FileChange::Update { path, moved_to } => Self::Update { path, moved_to },
        }
    }
}

impl From<StoredFileChange> for FileChange {
    fn from(change: StoredFileChange) -> Self {
        match change {
            StoredFileChange::Add { path } => Self::Add { path },
            StoredFileChange::Delete { path } => Self::Delete { path },
            StoredFileChange::Update { path, moved_to } => Self::Update { path, moved_to },
        }
    }
}

fn usize_to_i64(
    session_id: SessionId,
    field: &'static str,
    value: usize,
) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|error| StorageError::Write {
        session_id,
        message: format!("encode {field}: {error}"),
    })
}

fn u64_to_i64(session_id: SessionId, field: &'static str, value: u64) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|error| StorageError::Write {
        session_id,
        message: format!("encode {field}: {error}"),
    })
}

fn i64_to_u64(session_id: &str, field: &'static str, value: i64) -> Result<u64, StorageError> {
    u64::try_from(value).map_err(|error| invalid_session(session_id, field, error))
}

fn encode<T: Serialize>(
    session_id: SessionId,
    field: &'static str,
    value: &T,
) -> Result<String, StorageError> {
    serde_json::to_string(value).map_err(|error| StorageError::Write {
        session_id,
        message: format!("encode {field}: {error}"),
    })
}

fn decode<T: DeserializeOwned>(
    session_id: &str,
    field: &'static str,
    value: &str,
) -> Result<T, StorageError> {
    serde_json::from_str(value).map_err(|error| invalid_session(session_id, field, error))
}

fn parse_id<T>(
    value: &str,
    field: &'static str,
    constructor: impl FnOnce(Uuid) -> T,
) -> Result<T, StorageError> {
    Uuid::parse_str(value)
        .map(constructor)
        .map_err(|error| invalid_session(value, field, error))
}

fn invalid_session(
    session_id: &str,
    field: &'static str,
    error: impl fmt::Display,
) -> StorageError {
    StorageError::InvalidSession {
        session_id: session_id.to_owned(),
        message: format!("invalid {field}: {error}"),
    }
}
