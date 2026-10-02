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
        Activity, ActivityId, ActivityStatus, AgentId, AgentIdentity, AgentSelection,
        AttachmentBinding, Author, Cost, CostBasis, FileChange, Message, MessageId, MessageRole,
        MessageStatus, ModelDescriptor, ModelId, ModelOptionChoiceId, ModelOptionId,
        ModelOptionSelection, ModelOptionValue, Outlook, Prompt, PromptDelivery, PromptId,
        PromptOrder, PromptStatus, PromptWithdrawal, ProviderId, Session, SessionId,
        SessionRevision, SessionStandingInputs, SessionSummary, SessionTimestamp, SkillId,
        SkillInvocation, TextSpan, TranscriptItem, Turn, TurnId, TurnStatus,
        UnreadableSessionSummary, Usage, Workspace, WorkspaceDescription, WorkspaceId,
    },
    provider::{ProviderResumeState, ProviderSubagentId},
};

use super::{
    PersistedSession, StorageError, StoredResumeState, StoredSidekickAct, StoredSubagentIdentity,
    StoredWorkspace, UnreadableStoredSession, activities, landing_agent_selection, messages,
    model_catalog, prompts, provider_resume_states, provider_subagent_identities, sessions,
    sidekick_acts, turns, workspaces,
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

/// A Workspace's durable, Workspace-owned state: its Icon and its
/// Description. Nothing broader lives here, since ADR 0027 keeps no
/// persisted registry of Repositories.
/// `created_at` stamps the row's first write, whichever of the two made it;
/// each later write of either moves `updated_at` alone, through the explicit
/// conflict updates in `StorageRepository` rather than through this row's own
/// `Insertable` derive.
#[derive(Insertable, Queryable, Selectable)]
#[diesel(table_name = workspaces)]
pub(super) struct WorkspaceRow {
    id: String,
    icon: Option<String>,
    created_at: i64,
    updated_at: i64,
    description: Option<String>,
    description_set: bool,
    /// Written apart from the Icon and the Description, once either has made
    /// the row (see `StorageRepository::write_workspace_path`).
    path: Option<String>,
}

impl WorkspaceRow {
    pub(super) fn from_icon(workspace_id: WorkspaceId, icon: String, at: SessionTimestamp) -> Self {
        let stamp = i64::try_from(at.0).unwrap_or(i64::MAX);
        Self {
            id: workspace_id.0,
            icon: Some(icon),
            created_at: stamp,
            updated_at: stamp,
            description: None,
            description_set: false,
            path: None,
        }
    }

    pub(super) fn from_description(
        workspace_id: WorkspaceId,
        description: Option<WorkspaceDescription>,
        at: SessionTimestamp,
    ) -> Self {
        let stamp = i64::try_from(at.0).unwrap_or(i64::MAX);
        let (description, description_set) = description.map_or((None, false), |description| {
            (Some(description.text), description.set)
        });
        Self {
            id: workspace_id.0,
            icon: None,
            created_at: stamp,
            updated_at: stamp,
            description,
            description_set,
            path: None,
        }
    }

    /// The row as the Session store holds it. A Description is only ever
    /// set with text, so a row whose text is absent carries none, whatever
    /// its flag says.
    pub(super) fn into_stored(self) -> (WorkspaceId, StoredWorkspace) {
        let description = self.description.map(|text| WorkspaceDescription {
            text,
            set: self.description_set,
        });
        (
            WorkspaceId(self.id),
            StoredWorkspace {
                icon: self.icon,
                description,
                path: self.path.map(PathBuf::from),
            },
        )
    }
}

/// One Sidekick's latest act on one Session, as the `sidekick_acts` table
/// keeps it. The Session acted on is named with its Origin, which is empty
/// for a Session of this Server and otherwise the name of the Remote it
/// lives on.
#[derive(Insertable, Queryable, Selectable)]
#[diesel(table_name = sidekick_acts)]
pub(super) struct SidekickActRow {
    sidekick_session_id: String,
    origin: String,
    session_id: String,
    acted_at: i64,
    began: bool,
    resolved: bool,
    confirmed: bool,
    pairing: String,
    beginning: Option<String>,
    evidence: String,
}

impl SidekickActRow {
    /// The Origin a Session of this Server is kept under.
    pub(super) const THIS_SERVER: &'static str = "";

    pub(super) fn from_stored(act: &StoredSidekickAct) -> Self {
        Self {
            sidekick_session_id: act.sidekick.to_string(),
            origin: act
                .origin
                .remote_name()
                .unwrap_or(Self::THIS_SERVER)
                .to_owned(),
            session_id: act.session_id.to_string(),
            acted_at: i64::try_from(act.acted_at.0).unwrap_or(i64::MAX),
            began: act.began,
            resolved: act.resolved,
            confirmed: act.confirmed,
            pairing: act.pairing.clone(),
            beginning: act.beginning.clone(),
            evidence: act.evidence.clone(),
        }
    }

    /// The act as the Session store holds it, or nothing for a row whose
    /// identities no longer decode.
    pub(super) fn into_stored(self) -> Option<StoredSidekickAct> {
        Some(StoredSidekickAct {
            sidekick: SessionId::from_uuid(Uuid::parse_str(&self.sidekick_session_id).ok()?),
            origin: match self.origin.as_str() {
                Self::THIS_SERVER => Outlook::Local,
                remote => Outlook::Remote(remote.to_owned()),
            },
            session_id: SessionId::from_uuid(Uuid::parse_str(&self.session_id).ok()?),
            acted_at: SessionTimestamp(u64::try_from(self.acted_at).unwrap_or(0)),
            began: self.began,
            resolved: self.resolved,
            confirmed: self.confirmed,
            pairing: self.pairing,
            beginning: self.beginning,
            evidence: self.evidence,
        })
    }
}

#[derive(AsChangeset, Insertable, Queryable, Selectable)]
#[diesel(table_name = sessions, treat_none_as_null = true)]
pub(super) struct SessionRow {
    pub(super) id: String,
    title: String,
    icon: Option<String>,
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
    brokered: bool,
    begun_by: Option<String>,
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

/// The Provider's own identity for the Subagent a child Session is, keyed by
/// that Session. Plain columns rather than a payload: the identity is the
/// Provider's opaque string, which Suru only ever compares.
#[derive(AsChangeset, Insertable, Queryable, Selectable)]
#[diesel(table_name = provider_subagent_identities)]
pub(super) struct ProviderSubagentIdentityRow {
    session_id: String,
    provider: String,
    subagent_id: String,
}

impl ProviderSubagentIdentityRow {
    fn from_identity(session_id: &str, identity: StoredSubagentIdentity) -> Self {
        Self {
            session_id: session_id.to_owned(),
            provider: identity.provider.to_string(),
            subagent_id: identity.subagent_id.as_str().to_owned(),
        }
    }

    pub(super) fn into_identity(self) -> StoredSubagentIdentity {
        StoredSubagentIdentity {
            provider: ProviderId::new(self.provider),
            subagent_id: ProviderSubagentId::new(self.subagent_id),
        }
    }
}

pub(super) struct StoredRows {
    pub(super) session_id: SessionId,
    pub(super) session: SessionRow,
    pub(super) prompts: Vec<PromptRow>,
    pub(super) turns: Vec<TurnRow>,
    pub(super) messages: Vec<MessageRow>,
    pub(super) activities: Vec<ActivityRow>,
    pub(super) subagent_identity: Option<ProviderSubagentIdentityRow>,
    /// Every Attachment the Session's Prompts and Messages bind, once each.
    pub(super) attachment_ids: Vec<String>,
    /// The acts of Sidekicks on the Session that the change these rows
    /// record follows, so each lands in the same transaction as its change.
    pub(super) sidekick_acts: Vec<SidekickActRow>,
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
            subagent_identity,
            brokered,
        } = persisted;
        let session_id = snapshot.session.id;
        let id = session_id.to_string();
        let transcript_order = snapshot
            .transcript
            .iter()
            .enumerate()
            .map(|(order, item)| (transcript_identity(*item), order))
            .collect::<HashMap<_, _>>();
        let session = SessionRow::from_parts(summary, snapshot.revision, brokered)?;
        let attachment_ids = snapshot
            .prompts
            .iter()
            .flat_map(|prompt| &prompt.attachments)
            .chain(
                snapshot
                    .messages
                    .iter()
                    .flat_map(|message| &message.attachments),
            )
            .map(|binding| binding.attachment_id.as_str().to_owned())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
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
        let subagent_identity = subagent_identity
            .map(|identity| ProviderSubagentIdentityRow::from_identity(&id, identity));
        Ok(Self {
            session_id,
            session,
            prompts,
            turns,
            messages,
            activities,
            subagent_identity,
            attachment_ids,
            sidekick_acts: Vec::new(),
        })
    }
}

impl SessionRow {
    fn from_parts(
        summary: SessionSummary,
        revision: SessionRevision,
        brokered: bool,
    ) -> Result<Self, StorageError> {
        let session_id = summary.session.id;
        Ok(Self {
            id: session_id.to_string(),
            title: summary.title,
            icon: summary.icon,
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
            workspace: Self::session_metadata_payload(&summary.session)?,
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
            brokered,
            begun_by: summary
                .session
                .begun_by
                .map(StoredAuthor::from)
                .as_ref()
                .map(|author| encode(session_id, "beginning author", author))
                .transpose()?,
        })
    }

    pub(super) fn session_metadata_payload(session: &Session) -> Result<String, StorageError> {
        encode(
            session.id,
            "Session metadata",
            &StoredSessionMetadata {
                workspace: session.workspace.clone(),
                execution_directory: session.execution_directory.clone(),
                checkout: session.checkout.clone(),
                approval_posture: session.approval_posture,
            },
        )
    }

    pub(super) fn into_summary_and_revision(
        self,
    ) -> Result<(SessionSummary, SessionRevision), StorageError> {
        let session_id = self.id.clone();
        let id = parse_id(&session_id, "Session ID", SessionId::from_uuid)?;
        let metadata: StoredSessionMetadata =
            decode(&session_id, "Session metadata", &self.workspace)?;
        let agent_selection = self
            .agent_selection
            .as_deref()
            .map(|value| decode::<StoredAgentSelection>(&session_id, "Agent Selection", value))
            .transpose()?
            .map(AgentSelection::from);
        let approval_posture = metadata.approval_posture;
        let summary = SessionSummary {
            checkout_state: None,
            session: Session {
                checkout: metadata.checkout.clone(),
                context_fill: self
                    .context_fill
                    .as_deref()
                    .map(|fill| decode(&session_id, "Context Fill", fill))
                    .transpose()?,
                id,
                execution_directory: metadata.execution_directory,
                workspace: metadata.workspace,
                agent_selection,
                agent_selection_availability: decode(
                    &session_id,
                    "Agent Selection availability",
                    &self.agent_selection_availability,
                )?,
                approval_posture,
                status: decode(&session_id, "Session status", &self.status)?,
                working_since: None,
                monitoring_since: None,
                parent: self
                    .parent_session_id
                    .as_deref()
                    .map(|parent| parse_id(parent, "parent Session ID", SessionId::from_uuid))
                    .transpose()?,
                begun_by: self
                    .begun_by
                    .as_deref()
                    .map(|author| decode::<StoredAuthor>(&session_id, "beginning author", author))
                    .transpose()?
                    .map(Author::from),
            },
            title: self.title,
            icon: self.icon,
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
            own_cost: None,
            remote_subsessions: Vec::new(),
            created_at: SessionTimestamp(i64_to_u64(&session_id, "created_at", self.created_at)?),
            updated_at: SessionTimestamp(i64_to_u64(&session_id, "updated_at", self.updated_at)?),
        };
        let revision = SessionRevision(i64_to_u64(&session_id, "revision", self.revision)?);
        Ok((summary, revision))
    }

    pub(super) fn is_child(&self) -> bool {
        self.parent_session_id.is_some()
    }

    /// Whether the row is a brokered Subagent's Session, read apart from the
    /// summary because it is a fact about where the Session's Provider work
    /// runs rather than anything a reader is shown.
    pub(super) fn brokered(&self) -> bool {
        self.brokered
    }

    pub(super) fn parent_id(&self) -> Result<Option<SessionId>, StorageError> {
        self.parent_session_id
            .as_deref()
            .map(|id| parse_id(id, "parent Session ID", SessionId::from_uuid))
            .transpose()
    }

    pub(super) fn unreadable_session(&self) -> Result<UnreadableStoredSession, StorageError> {
        let session_id = self.id.clone();
        let metadata = serde_json::from_str::<StoredSessionMetadata>(&self.workspace).ok();
        let checkout = metadata
            .as_ref()
            .and_then(|metadata| metadata.checkout.clone());
        let execution_directory = metadata
            .as_ref()
            .map(|metadata| metadata.execution_directory.clone());
        Ok(UnreadableStoredSession {
            summary: UnreadableSessionSummary {
                id: parse_id(&session_id, "Session ID", SessionId::from_uuid)?,
                title: self.title.clone(),
                created_at: SessionTimestamp(i64_to_u64(
                    &session_id,
                    "created_at",
                    self.created_at,
                )?),
                updated_at: SessionTimestamp(i64_to_u64(
                    &session_id,
                    "updated_at",
                    self.updated_at,
                )?),
                workspace: metadata.map(|metadata| metadata.workspace),
            },
            checkout,
            execution_directory,
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
                    skill_invocations: stored_skill_invocations(prompt.skill_invocations),
                    attachments: prompt.attachments,
                    delivery: prompt.delivery,
                    status: prompt.status,
                    withdrawal: prompt.withdrawal,
                    author: prompt.author.map(StoredAuthor::from),
                    taken: prompt.taken,
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
            skill_invocations: skill_invocations(payload.skill_invocations),
            attachments: payload.attachments,
            delivery: payload.delivery,
            admission_order: PromptOrder(i64_to_u64(
                &session_id,
                "Prompt admission order",
                self.admission_order,
            )?),
            status: payload.status,
            withdrawal: payload.withdrawal,
            author: payload.author.map(Author::from),
            taken: payload.taken,
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
                    compaction_requested: turn.compaction_requested,
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
            compaction_requested: payload.compaction_requested,
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
        if !turn.has_valid_opening() {
            return Err(StorageError::InvalidSession {
                session_id,
                message: "Turn payload is begun by both a Prompt and a Compaction request"
                    .to_owned(),
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
                    skill_invocations: stored_skill_invocations(message.skill_invocations),
                    attachments: message.attachments,
                    truncated: message.truncated,
                    author: message.author.map(StoredAuthor::from),
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
                skill_invocations: skill_invocations(payload.skill_invocations),
                attachments: payload.attachments,
                truncated: payload.truncated,
                author: payload.author.map(Author::from),
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

    /// The stored identity of the Session whose Transcript holds the row.
    pub(super) fn session_id(&self) -> &str {
        &self.session_id
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

/// Session metadata needed by listings and restoration stays readable without
/// opening the Transcript.
#[derive(Deserialize, Serialize)]
struct StoredSessionMetadata {
    workspace: Workspace,
    checkout: Option<crate::protocol::CheckoutAssociation>,
    execution_directory: crate::protocol::ExecutionDirectory,
    approval_posture: Option<crate::protocol::SessionApprovalPosture>,
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
    skill_invocations: Vec<StoredSkillInvocation>,
    /// Absent when the Prompt binds none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    attachments: Vec<AttachmentBinding>,
    delivery: PromptDelivery,
    status: PromptStatus,
    /// Absent unless the Session withdrew the Prompt of its own accord.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    withdrawal: Option<PromptWithdrawal>,
    /// Absent for a Prompt the user sent themselves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    author: Option<StoredAuthor>,
    /// Absent for a Prompt no Turn took.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    taken: Option<crate::protocol::PromptTaking>,
}

/// Who sent a Prompt or gave an Answer on the user's behalf, as stored with
/// the Prompt, the Message it became, and the Questionnaire answered — kept
/// apart from the protocol's so renaming a protocol field cannot strand what
/// is already written.
#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredAuthor {
    Sidekick {
        session_id: SessionId,
        title: String,
    },
    PeerSidekick {
        peer: String,
        #[serde(default)]
        fingerprint: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        act: Option<crate::protocol::ActId>,
    },
}

impl From<Author> for StoredAuthor {
    fn from(author: Author) -> Self {
        match author {
            Author::Sidekick { session_id, title } => Self::Sidekick { session_id, title },
            Author::PeerSidekick {
                peer,
                fingerprint,
                act,
            } => Self::PeerSidekick {
                peer,
                fingerprint,
                act,
            },
        }
    }
}

impl From<StoredAuthor> for Author {
    fn from(author: StoredAuthor) -> Self {
        match author {
            StoredAuthor::Sidekick { session_id, title } => Self::Sidekick { session_id, title },
            StoredAuthor::PeerSidekick {
                peer,
                fingerprint,
                act,
            } => Self::PeerSidekick {
                peer,
                fingerprint,
                act,
            },
        }
    }
}

/// A Skill Invocation as stored, kept apart from the protocol's so renaming
/// a protocol field cannot strand the Prompts and Messages already written.
#[derive(Deserialize, Serialize)]
struct StoredSkillInvocation {
    skill_id: SkillId,
    name: String,
    scope: Option<String>,
    span: StoredTextSpan,
}

#[derive(Deserialize, Serialize)]
struct StoredTextSpan {
    start: u32,
    end: u32,
}

impl From<SkillInvocation> for StoredSkillInvocation {
    fn from(invocation: SkillInvocation) -> Self {
        Self {
            skill_id: invocation.skill_id,
            name: invocation.name,
            scope: invocation.scope,
            span: StoredTextSpan {
                start: invocation.span.start,
                end: invocation.span.end,
            },
        }
    }
}

impl From<StoredSkillInvocation> for SkillInvocation {
    fn from(invocation: StoredSkillInvocation) -> Self {
        Self {
            skill_id: invocation.skill_id,
            name: invocation.name,
            scope: invocation.scope,
            span: TextSpan {
                start: invocation.span.start,
                end: invocation.span.end,
            },
        }
    }
}

fn stored_skill_invocations(invocations: Vec<SkillInvocation>) -> Vec<StoredSkillInvocation> {
    invocations.into_iter().map(Into::into).collect()
}

fn skill_invocations(invocations: Vec<StoredSkillInvocation>) -> Vec<SkillInvocation> {
    invocations.into_iter().map(Into::into).collect()
}

#[derive(Deserialize, Serialize)]
struct StoredTurnPayload {
    /// Whether a Compaction request began the Turn, which a Continuation's
    /// absent Prompt alone would not say. Absent when no request began it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    compaction_requested: bool,
    agent: Option<StoredAgentIdentity>,
    status: TurnStatus,
    started_at: Option<SessionTimestamp>,
    settled_at: Option<SessionTimestamp>,
    last_output_at: Option<SessionTimestamp>,
    usage: Option<Usage>,
    cost: Option<Cost>,
    cost_basis: Option<CostBasis>,
    cost_details: Option<crate::protocol::CostDetails>,
}

#[derive(Deserialize, Serialize)]
struct StoredMessagePayload {
    role: MessageRole,
    status: MessageStatus,
    content: String,
    skill_invocations: Vec<StoredSkillInvocation>,
    /// Absent when the Message binds none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    attachments: Vec<AttachmentBinding>,
    truncated: bool,
    /// Absent for every Message but a user Message delivered from a Prompt
    /// sent on the user's behalf.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    author: Option<StoredAuthor>,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredActivityPayload {
    Approval {
        approval: crate::protocol::Approval,
        tool_activity_id: Option<ActivityId>,
        detail_truncated: bool,
        outcome: crate::protocol::ApprovalOutcome,
        decision: Option<crate::protocol::Decision>,
        follow_up_error: Option<String>,
        /// Absent for one asked before Suru recorded when.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        asked_at: Option<SessionTimestamp>,
    },
    Questionnaire {
        questionnaire: crate::protocol::Questionnaire,
        outcome: crate::protocol::QuestionnaireOutcome,
        answer: Option<crate::protocol::Answer>,
        /// Absent for a Questionnaire the user answered or declined
        /// themselves, and for one neither.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        author: Option<StoredAuthor>,
        /// Absent for one asked, or settled, before Suru recorded when.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        asked_at: Option<SessionTimestamp>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        settled_at: Option<SessionTimestamp>,
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
    ToolCall {
        status: ActivityStatus,
        name: String,
        server: Option<String>,
        input: String,
        input_truncated: bool,
        output: String,
        output_truncated: bool,
        omitted_parts: u32,
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
        model: Option<ModelId>,
        session_id: SessionId,
        brokered: bool,
        duration_ms: Option<u64>,
        /// Absent for a row stood before Suru recorded when.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delegated_at: Option<SessionTimestamp>,
    },
    WatchOutcome {
        status: crate::protocol::WatchOutcomeStatus,
        description: String,
        summary: Option<String>,
    },
    Compaction {
        status: ActivityStatus,
        trigger: crate::protocol::CompactionTrigger,
        instructions: Option<String>,
        before_tokens: Option<u64>,
        after_tokens: Option<u64>,
        error: Option<String>,
        summary: Option<String>,
        summary_truncated: bool,
    },
    Subsession {
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<String>,
        title: String,
        prompt: String,
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
                follow_up_error,
                asked_at,
            } => Activity::Approval {
                id,
                turn_id,
                approval,
                tool_activity_id,
                detail_truncated,
                outcome,
                decision,
                follow_up_error,
                asked_at,
            },
            Self::Questionnaire {
                questionnaire,
                outcome,
                answer,
                author,
                asked_at,
                settled_at,
            } => Activity::Questionnaire {
                id,
                turn_id,
                questionnaire,
                outcome,
                answer,
                author: author.map(Author::from),
                asked_at,
                settled_at,
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
            Self::ToolCall {
                status,
                name,
                server,
                input,
                input_truncated,
                output,
                output_truncated,
                omitted_parts,
            } => Activity::ToolCall {
                id,
                turn_id,
                status,
                name,
                server,
                input,
                input_truncated,
                output,
                output_truncated,
                omitted_parts,
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
                brokered,
                duration_ms,
                delegated_at,
            } => Activity::Subagent {
                id,
                turn_id,
                status,
                name,
                description,
                model,
                session_id,
                brokered,
                duration_ms,
                delegated_at,
            },
            Self::WatchOutcome {
                status,
                description,
                summary,
            } => Activity::WatchOutcome {
                id,
                turn_id,
                status,
                description,
                summary,
            },
            Self::Compaction {
                status,
                trigger,
                instructions,
                before_tokens,
                after_tokens,
                error,
                summary,
                summary_truncated,
            } => Activity::Compaction {
                id,
                turn_id,
                status,
                trigger,
                instructions,
                before_tokens,
                after_tokens,
                error,
                summary,
                summary_truncated,
            },
            Self::Subsession {
                session_id,
                origin,
                title,
                prompt,
            } => Activity::Subsession {
                id,
                turn_id,
                session_id,
                origin,
                title,
                prompt,
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
                follow_up_error,
                asked_at,
                ..
            } => Self::Approval {
                approval,
                tool_activity_id,
                detail_truncated,
                outcome,
                decision,
                follow_up_error,
                asked_at,
            },
            Activity::Questionnaire {
                questionnaire,
                outcome,
                answer,
                author,
                asked_at,
                settled_at,
                ..
            } => Self::Questionnaire {
                questionnaire,
                outcome,
                answer,
                author: author.map(StoredAuthor::from),
                asked_at,
                settled_at,
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
            Activity::ToolCall {
                status,
                name,
                server,
                input,
                input_truncated,
                output,
                output_truncated,
                omitted_parts,
                ..
            } => Self::ToolCall {
                status,
                name,
                server,
                input,
                input_truncated,
                output,
                output_truncated,
                omitted_parts,
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
                brokered,
                duration_ms,
                delegated_at,
                ..
            } => Self::Subagent {
                status,
                name,
                description,
                model,
                session_id,
                brokered,
                duration_ms,
                delegated_at,
            },
            Activity::WatchOutcome {
                status,
                description,
                summary,
                ..
            } => Self::WatchOutcome {
                status,
                description,
                summary,
            },
            Activity::Compaction {
                status,
                trigger,
                instructions,
                before_tokens,
                after_tokens,
                error,
                summary,
                summary_truncated,
                ..
            } => Self::Compaction {
                status,
                trigger,
                instructions,
                before_tokens,
                after_tokens,
                error,
                summary,
                summary_truncated,
            },
            Activity::Subsession {
                session_id,
                origin,
                title,
                prompt,
                ..
            } => Self::Subsession {
                session_id,
                origin,
                title,
                prompt,
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

#[cfg(test)]
mod tests {
    use super::*;

    // The stored shape is storage's own, so it holds when the protocol's
    // Skill Invocation is renamed.
    #[test]
    fn a_prompt_stored_with_a_skill_invocation_span_stays_readable() {
        let prompt = PromptRow {
            id: "0199a0cc-7f41-7e0b-a6b8-1f2e3d4c5b6a".to_owned(),
            session_id: "c59a2193-9a99-48a9-91e7-ff1f6b9ee239".to_owned(),
            row_order: 0,
            admission_order: 0,
            payload: r#"{"text":"$implement #204","skill_invocations":[{"skill_id":"claude-implement","name":"implement","scope":"User","span":{"start":0,"end":10}}],"delivery":"steer","status":"delivered"}"#.to_owned(),
        }
        .into_prompt()
        .expect("decode a Prompt stored with a Skill Invocation span");

        assert_eq!(
            prompt.skill_invocations,
            vec![SkillInvocation {
                skill_id: SkillId::new("claude-implement"),
                name: "implement".to_owned(),
                scope: Some("User".to_owned()),
                span: TextSpan { start: 0, end: 10 },
            }]
        );
        assert_eq!(prompt.withdrawal, None, "it says nothing of a withdrawal");
        assert_eq!(
            prompt.author, None,
            "a Prompt stored before authors were is the user's own"
        );
    }

    #[test]
    fn a_prompt_the_session_withdrew_is_stored_with_why() {
        let session_id = SessionId::new();
        let stored_session_id = session_id.to_string();
        let withdrawn = Prompt {
            id: PromptId::new(),
            text: "Now the lexer".to_owned(),
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
            delivery: PromptDelivery::Steer,
            admission_order: PromptOrder(3),
            status: PromptStatus::Cancelled,
            withdrawal: Some(PromptWithdrawal::CompactionUnfinished {
                turn_id: TurnId::new(),
            }),
            author: None,
            taken: None,
        };
        let row = PromptRow::from_prompt(
            RowPosition::new(session_id, &stored_session_id, 2),
            withdrawn.clone(),
        )
        .expect("store a withdrawn Prompt");
        assert_eq!(row.into_prompt().expect("read it back"), withdrawn);

        let cancelled = Prompt {
            withdrawal: None,
            ..withdrawn
        };
        let row = PromptRow::from_prompt(
            RowPosition::new(session_id, &stored_session_id, 2),
            cancelled.clone(),
        )
        .expect("store a cancelled Prompt");
        assert!(
            !row.payload.contains("withdrawal"),
            "a Prompt withdrawn for no reason of the Session's own stores none: {}",
            row.payload
        );
        assert_eq!(row.into_prompt().expect("read it back"), cancelled);
    }

    #[test]
    fn a_sidekicks_prompt_and_the_message_it_became_are_read_back_naming_the_sidekick() {
        for author in [
            Author::Sidekick {
                session_id: SessionId::new(),
                title: "Tidy the listing".to_owned(),
            },
            Author::PeerSidekick {
                peer: "laptop".to_owned(),
                fingerprint: "ab12cd34ef56".to_owned(),
                act: None,
            },
        ] {
            a_prompt_and_its_message_are_read_back_naming(author);
        }
    }

    /// Read back naming its author, and the Turn that took it, when.
    fn a_prompt_and_its_message_are_read_back_naming(author: Author) {
        let session_id = SessionId::new();
        let stored_session_id = session_id.to_string();
        let prompt = Prompt {
            id: PromptId::new(),
            text: "Pick this back up".to_owned(),
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
            delivery: PromptDelivery::Queue,
            admission_order: PromptOrder(2),
            status: PromptStatus::Delivered,
            taken: Some(crate::protocol::PromptTaking {
                turn_id: TurnId::new(),
                taken_at: Some(SessionTimestamp(7)),
            }),
            author: Some(author.clone()),
            withdrawal: None,
        };
        let message = Message {
            id: MessageId::new(),
            turn_id: TurnId::new(),
            role: MessageRole::User,
            status: MessageStatus::Completed,
            content: prompt.text.clone(),
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
            truncated: false,
            author: Some(author),
        };

        let read_prompt = PromptRow::from_prompt(
            RowPosition::new(session_id, &stored_session_id, 0),
            prompt.clone(),
        )
        .expect("store the Prompt")
        .into_prompt()
        .expect("read the Prompt back");
        let (read_message, _) = MessageRow::from_message(
            TranscriptPosition {
                row: RowPosition::new(session_id, &stored_session_id, 0),
                transcript_order: 0,
            },
            message.clone(),
        )
        .expect("store the Message")
        .into_message()
        .expect("read the Message back");

        assert_eq!(read_prompt, prompt);
        assert_eq!(read_message, message);
    }

    #[test]
    fn an_answer_a_sidekick_gave_is_read_back_naming_the_sidekick_and_one_stored_before_names_none()
    {
        let session_id = SessionId::new();
        let stored_session_id = session_id.to_string();
        let questionnaire = crate::protocol::Questionnaire {
            id: crate::protocol::QuestionnaireId::new(),
            questions: Vec::new(),
        };
        let answered = Activity::Questionnaire {
            id: ActivityId::new(),
            turn_id: TurnId::new(),
            questionnaire: questionnaire.clone(),
            outcome: crate::protocol::QuestionnaireOutcome::Answered,
            answer: Some(crate::protocol::Answer {
                questions: Vec::new(),
            }),
            author: Some(Author::Sidekick {
                session_id: SessionId::new(),
                title: "Tidy the listing".to_owned(),
            }),
            asked_at: Some(SessionTimestamp(3)),
            settled_at: Some(SessionTimestamp(5)),
        };
        let position = || TranscriptPosition {
            row: RowPosition::new(session_id, &stored_session_id, 0),
            transcript_order: 0,
        };
        let (read, _) = ActivityRow::from_activity(position(), answered.clone())
            .expect("store the Questionnaire")
            .into_activity()
            .expect("read the Questionnaire back");
        assert_eq!(read, answered);

        let mut row =
            ActivityRow::from_activity(position(), answered).expect("store the Questionnaire");
        let mut payload: serde_json::Value =
            serde_json::from_str(&row.payload).expect("the payload is JSON");
        payload
            .as_object_mut()
            .expect("the payload is an object")
            .remove("author");
        row.payload = payload.to_string();
        let (read, _) = row
            .into_activity()
            .expect("read the older Questionnaire back");
        assert!(
            matches!(read, Activity::Questionnaire { author: None, .. }),
            "a Questionnaire stored before authors were was answered by the user: {read:?}"
        );
    }
}
