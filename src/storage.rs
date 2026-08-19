//! Durable Session state behind a domain-oriented repository seam.

use std::{
    collections::HashMap,
    fmt,
    path::{Path, PathBuf},
    sync::{Arc, mpsc as std_mpsc},
    thread::{self, JoinHandle},
    time::Duration,
};

use diesel::{
    OptionalExtension, QueryableByName, SqliteConnection,
    connection::SimpleConnection,
    deserialize::Queryable,
    prelude::*,
    sql_types::{BigInt, Text},
};
use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use uuid::Uuid;

use crate::{
    protocol::{
        Activity, ActivityId, ActivityStatus, AgentId, AgentIdentity, AgentSelection, FileChange,
        Message, MessageId, MessageRole, MessageStatus, ModelId, ModelOptionChoiceId,
        ModelOptionId, ModelOptionSelection, ModelOptionValue, Prompt, PromptDelivery, PromptId,
        PromptOrder, PromptStatus, ProviderId, Session, SessionId, SessionRevision,
        SessionSnapshot, SessionStatus, SessionSummary, SessionTimestamp, SessionUpdate,
        TranscriptItem, Turn, TurnId, TurnStatus, Workspace,
    },
    runtime::protect_current_user_file,
    session_projection::apply_update,
};

const DATABASE_FILE: &str = "chidori.db";
const CURRENT_SCHEMA_VERSION: &str = "20260820010000";
const IDLE_FLUSH_DELAY: Duration = Duration::from_millis(100);
const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

diesel::table! {
    sessions (id) {
        id -> Text,
        title -> Text,
        created_at -> BigInt,
        updated_at -> BigInt,
        workspace -> Text,
        agent_selection -> Nullable<Text>,
        agent_selection_availability -> Text,
        status -> Text,
        revision -> BigInt,
    }
}

diesel::table! {
    prompts (id) {
        id -> Text,
        session_id -> Text,
        row_order -> BigInt,
        admission_order -> BigInt,
        payload -> Text,
    }
}

diesel::table! {
    turns (id) {
        id -> Text,
        session_id -> Text,
        prompt_id -> Text,
        row_order -> BigInt,
        payload -> Text,
    }
}

diesel::table! {
    messages (id) {
        id -> Text,
        session_id -> Text,
        turn_id -> Text,
        row_order -> BigInt,
        transcript_order -> BigInt,
        payload -> Text,
    }
}

diesel::table! {
    activities (id) {
        id -> Text,
        session_id -> Text,
        turn_id -> Text,
        row_order -> BigInt,
        transcript_order -> BigInt,
        payload -> Text,
    }
}

#[derive(Clone)]
pub(crate) struct SessionRepository {
    database_path: Arc<PathBuf>,
}

#[derive(Clone)]
pub(crate) struct PersistedSession {
    pub(crate) summary: SessionSummary,
    pub(crate) snapshot: SessionSnapshot,
}

#[derive(Debug)]
pub(crate) enum SessionRepositoryError {
    InvalidDatabasePath(PathBuf),
    Open {
        path: PathBuf,
        message: String,
    },
    NewerSchema {
        database_version: String,
        binary_version: &'static str,
    },
    Migration(String),
    Read(String),
    InvalidSession {
        session_id: String,
        message: String,
    },
    Write {
        session_id: SessionId,
        message: String,
    },
    BlockingTask {
        operation: &'static str,
        message: String,
    },
    WriterTask(String),
}

impl fmt::Display for SessionRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDatabasePath(path) => {
                write!(formatter, "database path is not valid UTF-8: {path:?}")
            }
            Self::Open { path, message } => {
                write!(formatter, "open Session database {path:?}: {message}")
            }
            Self::NewerSchema {
                database_version,
                binary_version,
            } => write!(
                formatter,
                "Session database schema {database_version} is newer than this Chidori binary (latest supported schema {binary_version})"
            ),
            Self::Migration(message) => {
                write!(formatter, "apply Session database migrations: {message}")
            }
            Self::Read(message) => write!(formatter, "read persisted Sessions: {message}"),
            Self::InvalidSession {
                session_id,
                message,
            } => write!(
                formatter,
                "decode persisted Session {session_id}: {message}"
            ),
            Self::Write {
                session_id,
                message,
            } => write!(formatter, "save Session {session_id}: {message}"),
            Self::BlockingTask { operation, message } => write!(
                formatter,
                "Session repository {operation} task failed: {message}"
            ),
            Self::WriterTask(message) => {
                write!(formatter, "Session writer task failed: {message}")
            }
        }
    }
}

impl std::error::Error for SessionRepositoryError {}

impl SessionRepository {
    pub(crate) async fn open(data_root: &Path) -> Result<Self, SessionRepositoryError> {
        let repository = Self {
            database_path: Arc::new(data_root.join(DATABASE_FILE)),
        };
        let database_path = repository.database_path.as_ref().clone();
        on_blocking_task("startup", move || initialize_database(&database_path)).await?;
        Ok(repository)
    }

    pub(crate) async fn load_sessions(
        &self,
    ) -> Result<Vec<PersistedSession>, SessionRepositoryError> {
        let database_path = self.database_path.as_ref().clone();
        on_blocking_task("loading", move || load_sessions(&database_path)).await
    }

    fn save_sessions(
        &self,
        persisted: Vec<PersistedSession>,
    ) -> Result<(), SessionRepositoryError> {
        if persisted.is_empty() {
            return Ok(());
        }
        let rows = persisted
            .into_iter()
            .map(StoredRows::from_session)
            .collect::<Result<Vec<_>, _>>()?;
        let mut connection = connect(&self.database_path)?;
        for rows in rows {
            save_rows(&mut connection, rows)?;
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct SessionPersistenceSink {
    commands: std_mpsc::Sender<WriterCommand>,
}

pub(crate) struct SessionPersistenceWriter {
    commands: std_mpsc::Sender<WriterCommand>,
    task: JoinHandle<Result<(), SessionRepositoryError>>,
}

enum WriterCommand {
    Create(PersistedSession),
    Update {
        summary: SessionSummary,
        update: SessionUpdate,
        durability: Option<std_mpsc::SyncSender<Result<(), String>>>,
    },
    Shutdown,
}

struct WriterState {
    persisted: PersistedSession,
    dirty: bool,
}

impl SessionPersistenceWriter {
    pub(crate) fn spawn(
        repository: SessionRepository,
        restored: &[PersistedSession],
    ) -> (Self, SessionPersistenceSink) {
        let (commands, receiver) = std_mpsc::channel();
        let mut sessions = restored
            .iter()
            .cloned()
            .map(|persisted| {
                (
                    persisted.snapshot.session.id,
                    WriterState {
                        persisted,
                        dirty: false,
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        let task = thread::spawn(move || {
            loop {
                match receiver.recv_timeout(IDLE_FLUSH_DELAY) {
                    Ok(WriterCommand::Create(persisted)) => {
                        sessions.insert(
                            persisted.snapshot.session.id,
                            WriterState {
                                persisted,
                                dirty: true,
                            },
                        );
                    }
                    Ok(WriterCommand::Update {
                        summary,
                        update,
                        durability,
                    }) => {
                        let session_id = update.session_id;
                        let state = sessions.get_mut(&session_id).ok_or_else(|| {
                            SessionRepositoryError::WriterTask(format!(
                                "received update for unknown Session {session_id}"
                            ))
                        })?;
                        apply_update(&mut state.persisted.snapshot, &update).map_err(|error| {
                            SessionRepositoryError::WriterTask(format!(
                                "project update for Session {session_id}: {error:#}"
                            ))
                        })?;
                        state.persisted.summary = summary;
                        state.dirty = true;
                        if is_turn_boundary(&update) {
                            let result =
                                flush_sessions(&repository, &mut sessions, Some(session_id));
                            if let Some(durability) = durability {
                                let _ = durability
                                    .send(result.as_ref().map(|_| ()).map_err(ToString::to_string));
                            }
                            result?;
                        }
                    }
                    Ok(WriterCommand::Shutdown) | Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                        flush_sessions(&repository, &mut sessions, None)?;
                        break;
                    }
                    Err(std_mpsc::RecvTimeoutError::Timeout) => {
                        flush_sessions(&repository, &mut sessions, None)?;
                    }
                }
            }
            Ok(())
        });
        (
            Self {
                commands: commands.clone(),
                task,
            },
            SessionPersistenceSink { commands },
        )
    }

    pub(crate) async fn shutdown(self) -> Result<(), SessionRepositoryError> {
        let _ = self.commands.send(WriterCommand::Shutdown);
        tokio::task::spawn_blocking(move || self.task.join())
            .await
            .map_err(|error| SessionRepositoryError::WriterTask(error.to_string()))?
            .map_err(|_| SessionRepositoryError::WriterTask("writer thread panicked".to_owned()))?
    }
}

impl SessionPersistenceSink {
    pub(crate) fn created(&self, summary: SessionSummary, snapshot: SessionSnapshot) {
        let _ = self.commands.send(WriterCommand::Create(PersistedSession {
            summary,
            snapshot,
        }));
    }

    pub(crate) fn updated(
        &self,
        summary: SessionSummary,
        update: &SessionUpdate,
    ) -> Result<(), SessionRepositoryError> {
        // The sender is the only streaming-path work. Projection, coalescing, and SQLite I/O all
        // happen in the background writer.
        let (durability, receipt) = if is_turn_boundary(update) {
            let (sender, receiver) = std_mpsc::sync_channel(0);
            (Some(sender), Some(receiver))
        } else {
            (None, None)
        };
        self.commands
            .send(WriterCommand::Update {
                summary,
                update: update.clone(),
                durability,
            })
            .map_err(|_| {
                SessionRepositoryError::WriterTask("writer is no longer running".to_owned())
            })?;
        if let Some(receipt) = receipt {
            receipt
                .recv()
                .map_err(|_| {
                    SessionRepositoryError::WriterTask(
                        "writer stopped before confirming a Turn boundary".to_owned(),
                    )
                })?
                .map_err(SessionRepositoryError::WriterTask)?;
        }
        Ok(())
    }
}

fn flush_sessions(
    repository: &SessionRepository,
    sessions: &mut HashMap<SessionId, WriterState>,
    only: Option<SessionId>,
) -> Result<(), SessionRepositoryError> {
    let dirty = sessions
        .iter()
        .filter(|(session_id, state)| {
            state.dirty && only.as_ref().is_none_or(|only| only == *session_id)
        })
        .map(|(_, state)| state.persisted.clone())
        .collect::<Vec<_>>();
    repository.save_sessions(dirty)?;
    for (session_id, state) in sessions {
        if state.dirty && only.as_ref().is_none_or(|only| only == session_id) {
            state.dirty = false;
        }
    }
    Ok(())
}

fn is_turn_boundary(update: &SessionUpdate) -> bool {
    update.changes.iter().any(|change| match change {
        crate::protocol::SessionChange::TurnAdded { turn } => turn.status != TurnStatus::Active,
        crate::protocol::SessionChange::TurnStatusChanged { status, .. } => {
            *status != TurnStatus::Active
        }
        crate::protocol::SessionChange::SessionStatusChanged { status } => {
            *status == SessionStatus::Idle
        }
        _ => false,
    })
}

#[derive(AsChangeset, Insertable, Queryable, Selectable)]
#[diesel(table_name = sessions, treat_none_as_null = true)]
struct SessionRow {
    id: String,
    title: String,
    created_at: i64,
    updated_at: i64,
    workspace: String,
    agent_selection: Option<String>,
    agent_selection_availability: String,
    status: String,
    revision: i64,
}

#[derive(Insertable, Queryable, Selectable)]
#[diesel(table_name = prompts)]
struct PromptRow {
    id: String,
    session_id: String,
    row_order: i64,
    admission_order: i64,
    payload: String,
}

#[derive(Insertable, Queryable, Selectable)]
#[diesel(table_name = turns)]
struct TurnRow {
    id: String,
    session_id: String,
    prompt_id: String,
    row_order: i64,
    payload: String,
}

#[derive(Insertable, Queryable, Selectable)]
#[diesel(table_name = messages)]
struct MessageRow {
    id: String,
    session_id: String,
    turn_id: String,
    row_order: i64,
    transcript_order: i64,
    payload: String,
}

#[derive(Insertable, Queryable, Selectable)]
#[diesel(table_name = activities)]
struct ActivityRow {
    id: String,
    session_id: String,
    turn_id: String,
    row_order: i64,
    transcript_order: i64,
    payload: String,
}

struct StoredRows {
    session_id: SessionId,
    session: SessionRow,
    prompts: Vec<PromptRow>,
    turns: Vec<TurnRow>,
    messages: Vec<MessageRow>,
    activities: Vec<ActivityRow>,
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
    fn from_session(persisted: PersistedSession) -> Result<Self, SessionRepositoryError> {
        let PersistedSession { summary, snapshot } = persisted;
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
    ) -> Result<Self, SessionRepositoryError> {
        let session_id = summary.session.id;
        Ok(Self {
            id: session_id.to_string(),
            title: summary.title,
            created_at: u64_to_i64(session_id, "created_at", summary.created_at.0)?,
            updated_at: u64_to_i64(session_id, "updated_at", summary.updated_at.0)?,
            workspace: encode(
                session_id,
                "Workspace",
                &StoredWorkspace::from(summary.session.workspace),
            )?,
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
        })
    }

    fn into_summary_and_revision(
        self,
    ) -> Result<(SessionSummary, SessionRevision), SessionRepositoryError> {
        let session_id = self.id.clone();
        let id = parse_id(&session_id, "Session ID", SessionId::from_uuid)?;
        let workspace: StoredWorkspace = decode(&session_id, "Workspace", &self.workspace)?;
        let agent_selection = self
            .agent_selection
            .as_deref()
            .map(|value| decode::<StoredAgentSelection>(&session_id, "Agent Selection", value))
            .transpose()?
            .map(AgentSelection::from);
        let summary = SessionSummary {
            session: Session {
                id,
                workspace: workspace.into(),
                agent_selection,
                agent_selection_availability: decode(
                    &session_id,
                    "Agent Selection availability",
                    &self.agent_selection_availability,
                )?,
                status: decode(&session_id, "Session status", &self.status)?,
            },
            title: self.title,
            created_at: SessionTimestamp(i64_to_u64(&session_id, "created_at", self.created_at)?),
            updated_at: SessionTimestamp(i64_to_u64(&session_id, "updated_at", self.updated_at)?),
        };
        let revision = SessionRevision(i64_to_u64(&session_id, "revision", self.revision)?);
        Ok((summary, revision))
    }
}

impl PromptRow {
    fn from_prompt(
        position: RowPosition<'_>,
        prompt: Prompt,
    ) -> Result<Self, SessionRepositoryError> {
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
                    delivery: prompt.delivery,
                    status: prompt.status,
                },
            )?,
        })
    }

    fn into_prompt(self) -> Result<Prompt, SessionRepositoryError> {
        let session_id = self.session_id;
        let payload: StoredPromptPayload = decode(&session_id, "Prompt payload", &self.payload)?;
        Ok(Prompt {
            id: parse_id(&self.id, "Prompt ID", PromptId::from_uuid)?,
            text: payload.text,
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
    fn from_turn(position: RowPosition<'_>, turn: Turn) -> Result<Self, SessionRepositoryError> {
        Ok(Self {
            id: turn.id.to_string(),
            session_id: position.stored_session_id.to_owned(),
            prompt_id: turn.prompt_id.to_string(),
            row_order: usize_to_i64(position.session_id, "Turn row order", position.row_order)?,
            payload: encode(
                position.session_id,
                "Turn payload",
                &StoredTurnPayload {
                    agent: turn.agent.map(StoredAgentIdentity::from),
                    status: turn.status,
                },
            )?,
        })
    }

    fn into_turn(self) -> Result<Turn, SessionRepositoryError> {
        let session_id = self.session_id;
        let payload: StoredTurnPayload = decode(&session_id, "Turn payload", &self.payload)?;
        Ok(Turn {
            id: parse_id(&self.id, "Turn ID", TurnId::from_uuid)?,
            prompt_id: parse_id(&self.prompt_id, "Turn Prompt ID", PromptId::from_uuid)?,
            agent: payload.agent.map(AgentIdentity::from),
            status: payload.status,
        })
    }
}

impl MessageRow {
    fn from_message(
        position: TranscriptPosition<'_>,
        message: Message,
    ) -> Result<Self, SessionRepositoryError> {
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
                },
            )?,
        })
    }

    fn into_message(self) -> Result<(Message, i64), SessionRepositoryError> {
        let session_id = self.session_id;
        let payload: StoredMessagePayload = decode(&session_id, "Message payload", &self.payload)?;
        Ok((
            Message {
                id: parse_id(&self.id, "Message ID", MessageId::from_uuid)?,
                turn_id: parse_id(&self.turn_id, "Message Turn ID", TurnId::from_uuid)?,
                role: payload.role,
                status: payload.status,
                content: payload.content,
            },
            self.transcript_order,
        ))
    }
}

impl ActivityRow {
    fn from_activity(
        position: TranscriptPosition<'_>,
        activity: Activity,
    ) -> Result<Self, SessionRepositoryError> {
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

    fn into_activity(self) -> Result<(Activity, i64), SessionRepositoryError> {
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

#[derive(Deserialize, Serialize)]
struct StoredWorkspace {
    path: PathBuf,
}

impl From<Workspace> for StoredWorkspace {
    fn from(workspace: Workspace) -> Self {
        Self {
            path: workspace.path,
        }
    }
}

impl From<StoredWorkspace> for Workspace {
    fn from(workspace: StoredWorkspace) -> Self {
        Self {
            path: workspace.path,
        }
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
    delivery: PromptDelivery,
    status: PromptStatus,
}

#[derive(Deserialize, Serialize)]
struct StoredTurnPayload {
    agent: Option<StoredAgentIdentity>,
    status: TurnStatus,
}

#[derive(Deserialize, Serialize)]
struct StoredMessagePayload {
    role: MessageRole,
    status: MessageStatus,
    content: String,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredActivityPayload {
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
        exit_status: Option<i32>,
    },
    FileChange {
        status: ActivityStatus,
        changes: Vec<StoredFileChange>,
    },
}

impl StoredActivityPayload {
    fn into_activity(self, id: ActivityId, turn_id: TurnId) -> Activity {
        match self {
            Self::Status { text } => Activity::Status { id, turn_id, text },
            Self::Error { text } => Activity::Error { id, turn_id, text },
            Self::Command {
                status,
                command,
                cwd,
                output,
                exit_status,
            } => Activity::Command {
                id,
                turn_id,
                status,
                command,
                cwd,
                output,
                exit_status,
            },
            Self::FileChange { status, changes } => Activity::FileChange {
                id,
                turn_id,
                status,
                changes: changes.into_iter().map(Into::into).collect(),
            },
        }
    }
}

impl From<Activity> for StoredActivityPayload {
    fn from(activity: Activity) -> Self {
        match activity {
            Activity::Status { text, .. } => Self::Status { text },
            Activity::Error { text, .. } => Self::Error { text },
            Activity::Command {
                status,
                command,
                cwd,
                output,
                exit_status,
                ..
            } => Self::Command {
                status,
                command,
                cwd,
                output,
                exit_status,
            },
            Activity::FileChange {
                status, changes, ..
            } => Self::FileChange {
                status,
                changes: changes.into_iter().map(Into::into).collect(),
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

fn load_sessions(database_path: &Path) -> Result<Vec<PersistedSession>, SessionRepositoryError> {
    let mut connection = connect(database_path)?;
    sessions::table
        .select(SessionRow::as_select())
        .load::<SessionRow>(&mut connection)
        .map_err(|error| SessionRepositoryError::Read(error.to_string()))?
        .into_iter()
        .map(|row| load_session(&mut connection, row))
        .collect()
}

fn load_session(
    connection: &mut SqliteConnection,
    row: SessionRow,
) -> Result<PersistedSession, SessionRepositoryError> {
    let stored_session_id = row.id.clone();
    let (summary, revision) = row.into_summary_and_revision()?;
    let prompt_rows = prompts::table
        .filter(prompts::session_id.eq(&stored_session_id))
        .order(prompts::row_order.asc())
        .select(PromptRow::as_select())
        .load(connection)
        .map_err(|error| SessionRepositoryError::Read(error.to_string()))?;
    let turn_rows = turns::table
        .filter(turns::session_id.eq(&stored_session_id))
        .order(turns::row_order.asc())
        .select(TurnRow::as_select())
        .load(connection)
        .map_err(|error| SessionRepositoryError::Read(error.to_string()))?;
    let message_rows = messages::table
        .filter(messages::session_id.eq(&stored_session_id))
        .order(messages::row_order.asc())
        .select(MessageRow::as_select())
        .load(connection)
        .map_err(|error| SessionRepositoryError::Read(error.to_string()))?;
    let activity_rows = activities::table
        .filter(activities::session_id.eq(&stored_session_id))
        .order(activities::row_order.asc())
        .select(ActivityRow::as_select())
        .load(connection)
        .map_err(|error| SessionRepositoryError::Read(error.to_string()))?;

    let prompts = prompt_rows
        .into_iter()
        .map(PromptRow::into_prompt)
        .collect::<Result<Vec<_>, _>>()?;
    let turns = turn_rows
        .into_iter()
        .map(TurnRow::into_turn)
        .collect::<Result<Vec<_>, _>>()?;
    let decoded_messages = message_rows
        .into_iter()
        .map(MessageRow::into_message)
        .collect::<Result<Vec<_>, _>>()?;
    let decoded_activities = activity_rows
        .into_iter()
        .map(ActivityRow::into_activity)
        .collect::<Result<Vec<_>, _>>()?;
    let messages = decoded_messages
        .iter()
        .map(|(message, _)| message.clone())
        .collect();
    let activities = decoded_activities
        .iter()
        .map(|(activity, _)| activity.clone())
        .collect();
    let mut transcript = decoded_messages
        .into_iter()
        .map(|(message, order)| {
            (
                order,
                TranscriptItem::Message {
                    message_id: message.id,
                },
            )
        })
        .chain(decoded_activities.into_iter().map(|(activity, order)| {
            (
                order,
                TranscriptItem::Activity {
                    activity_id: activity.id(),
                },
            )
        }))
        .collect::<Vec<_>>();
    transcript.sort_unstable_by_key(|(order, _)| *order);
    let snapshot = SessionSnapshot {
        session: summary.session.clone(),
        revision,
        prompts,
        turns,
        messages,
        activities,
        transcript: transcript.into_iter().map(|(_, item)| item).collect(),
    };
    Ok(PersistedSession { summary, snapshot })
}

fn save_rows(
    connection: &mut SqliteConnection,
    rows: StoredRows,
) -> Result<(), SessionRepositoryError> {
    let session_id = rows.session_id;
    connection
        .transaction::<_, diesel::result::Error, _>(|connection| {
            diesel::insert_into(sessions::table)
                .values(&rows.session)
                .on_conflict(sessions::id)
                .do_update()
                .set(&rows.session)
                .execute(connection)?;
            diesel::delete(prompts::table.filter(prompts::session_id.eq(rows.session.id.as_str())))
                .execute(connection)?;
            diesel::delete(turns::table.filter(turns::session_id.eq(rows.session.id.as_str())))
                .execute(connection)?;
            diesel::delete(
                messages::table.filter(messages::session_id.eq(rows.session.id.as_str())),
            )
            .execute(connection)?;
            diesel::delete(
                activities::table.filter(activities::session_id.eq(rows.session.id.as_str())),
            )
            .execute(connection)?;
            if !rows.prompts.is_empty() {
                diesel::insert_into(prompts::table)
                    .values(&rows.prompts)
                    .execute(connection)?;
            }
            if !rows.turns.is_empty() {
                diesel::insert_into(turns::table)
                    .values(&rows.turns)
                    .execute(connection)?;
            }
            if !rows.messages.is_empty() {
                diesel::insert_into(messages::table)
                    .values(&rows.messages)
                    .execute(connection)?;
            }
            if !rows.activities.is_empty() {
                diesel::insert_into(activities::table)
                    .values(&rows.activities)
                    .execute(connection)?;
            }
            Ok(())
        })
        .map_err(|error| SessionRepositoryError::Write {
            session_id,
            message: error.to_string(),
        })
}

fn initialize_database(database_path: &Path) -> Result<(), SessionRepositoryError> {
    let mut connection = connect(database_path)?;
    refuse_newer_schema(&mut connection)?;
    connection
        .run_pending_migrations(MIGRATIONS)
        .map_err(|error| SessionRepositoryError::Migration(error.to_string()))?;
    protect_current_user_file(database_path).map_err(|error| SessionRepositoryError::Open {
        path: database_path.to_owned(),
        message: error.to_string(),
    })
}

fn connect(database_path: &Path) -> Result<SqliteConnection, SessionRepositoryError> {
    let database_url = database_path
        .to_str()
        .ok_or_else(|| SessionRepositoryError::InvalidDatabasePath(database_path.to_owned()))?;
    let mut connection = SqliteConnection::establish(database_url).map_err(|error| {
        SessionRepositoryError::Open {
            path: database_path.to_owned(),
            message: error.to_string(),
        }
    })?;
    connection
        .batch_execute(
            "PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;",
        )
        .map_err(|error| SessionRepositoryError::Open {
            path: database_path.to_owned(),
            message: error.to_string(),
        })?;
    Ok(connection)
}

#[derive(QueryableByName)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    value: i64,
}

#[derive(QueryableByName)]
struct VersionRow {
    #[diesel(sql_type = Text)]
    version: String,
}

fn refuse_newer_schema(connection: &mut SqliteConnection) -> Result<(), SessionRepositoryError> {
    let migration_table = diesel::sql_query(
        "SELECT COUNT(*) AS value FROM sqlite_master WHERE type = 'table' AND name = '__diesel_schema_migrations'",
    )
    .get_result::<CountRow>(connection)
    .map_err(|error| SessionRepositoryError::Read(error.to_string()))?;
    if migration_table.value == 0 {
        return Ok(());
    }
    let latest = diesel::sql_query(
        "SELECT version FROM __diesel_schema_migrations ORDER BY version DESC LIMIT 1",
    )
    .get_result::<VersionRow>(connection)
    .optional()
    .map_err(|error| SessionRepositoryError::Read(error.to_string()))?;
    if let Some(latest) = latest
        && latest.version.as_str() > CURRENT_SCHEMA_VERSION
    {
        return Err(SessionRepositoryError::NewerSchema {
            database_version: latest.version,
            binary_version: CURRENT_SCHEMA_VERSION,
        });
    }
    Ok(())
}

async fn on_blocking_task<T>(
    operation: &'static str,
    task: impl FnOnce() -> Result<T, SessionRepositoryError> + Send + 'static,
) -> Result<T, SessionRepositoryError>
where
    T: Send + 'static,
{
    tokio::task::spawn_blocking(task).await.map_err(|error| {
        SessionRepositoryError::BlockingTask {
            operation,
            message: error.to_string(),
        }
    })?
}

fn usize_to_i64(
    session_id: SessionId,
    field: &'static str,
    value: usize,
) -> Result<i64, SessionRepositoryError> {
    i64::try_from(value).map_err(|error| SessionRepositoryError::Write {
        session_id,
        message: format!("encode {field}: {error}"),
    })
}

fn u64_to_i64(
    session_id: SessionId,
    field: &'static str,
    value: u64,
) -> Result<i64, SessionRepositoryError> {
    i64::try_from(value).map_err(|error| SessionRepositoryError::Write {
        session_id,
        message: format!("encode {field}: {error}"),
    })
}

fn i64_to_u64(
    session_id: &str,
    field: &'static str,
    value: i64,
) -> Result<u64, SessionRepositoryError> {
    u64::try_from(value).map_err(|error| invalid_session(session_id, field, error))
}

fn encode<T: Serialize>(
    session_id: SessionId,
    field: &'static str,
    value: &T,
) -> Result<String, SessionRepositoryError> {
    serde_json::to_string(value).map_err(|error| SessionRepositoryError::Write {
        session_id,
        message: format!("encode {field}: {error}"),
    })
}

fn decode<T: DeserializeOwned>(
    session_id: &str,
    field: &'static str,
    value: &str,
) -> Result<T, SessionRepositoryError> {
    serde_json::from_str(value).map_err(|error| invalid_session(session_id, field, error))
}

fn parse_id<T>(
    value: &str,
    field: &'static str,
    constructor: impl FnOnce(Uuid) -> T,
) -> Result<T, SessionRepositoryError> {
    Uuid::parse_str(value)
        .map(constructor)
        .map_err(|error| invalid_session(value, field, error))
}

fn invalid_session(
    session_id: &str,
    field: &'static str,
    error: impl fmt::Display,
) -> SessionRepositoryError {
    SessionRepositoryError::InvalidSession {
        session_id: session_id.to_owned(),
        message: format!("invalid {field}: {error}"),
    }
}
