//! Durable server state behind a domain-oriented repository seam.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
};

use diesel::{
    OptionalExtension, QueryableByName, SqliteConnection,
    connection::SimpleConnection,
    prelude::*,
    sql_types::{BigInt, Text},
};
use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};

use crate::{
    protocol::{
        AgentSelection, PromptId, ProviderId, SessionId, SessionSnapshot, SessionSummary,
        TranscriptItem, UnreadableSessionSummary,
    },
    provider::ProviderResumeState,
    runtime::protect_current_user_file,
};

mod rows;
mod writer;

pub(crate) use writer::{StorageSink, StorageWriter};

use rows::{
    ActivityRow, LandingAgentSelectionRow, MessageRow, PromptRow, ProviderResumeStateRow,
    SessionRow, StoredRows, TurnRow,
};

const DATABASE_FILE: &str = "suru.db";
const CURRENT_SCHEMA_VERSION: &str = "20260904000000";
const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

diesel::table! {
    sessions (id) {
        id -> Text,
        title -> Text,
        emoji -> Nullable<Text>,
        settled_at -> Nullable<BigInt>,
        viewed_at -> Nullable<BigInt>,
        created_at -> BigInt,
        updated_at -> BigInt,
        workspace -> Text,
        agent_selection -> Nullable<Text>,
        agent_selection_availability -> Text,
        status -> Text,
        revision -> BigInt,
        parent_session_id -> Nullable<Text>,
    }
}

diesel::table! {
    landing_agent_selection (singleton) {
        singleton -> Integer,
        selection -> Text,
    }
}

diesel::table! {
    provider_resume_states (session_id, provider) {
        session_id -> Text,
        provider -> Text,
        payload -> Text,
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
        prompt_id -> Nullable<Text>,
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
pub(crate) struct StorageRepository {
    database_path: Arc<PathBuf>,
}

#[derive(Clone)]
pub(crate) struct PersistedSession {
    pub(crate) summary: SessionSummary,
    pub(crate) snapshot: SessionSnapshot,
    pub(crate) resume_states: HashMap<ProviderId, ProviderResumeState>,
}

pub(crate) struct StoredResumeState {
    pub(crate) session_id: SessionId,
    pub(crate) provider: ProviderId,
    pub(crate) resume_state: ProviderResumeState,
}

#[derive(Default)]
pub(crate) struct RestoredSessions {
    pub(crate) readable: Vec<PersistedSession>,
    pub(crate) unreadable: Vec<UnreadableSessionSummary>,
    pub(crate) deferred: Option<DeferredSessions>,
}

pub(crate) struct DeferredSessions {
    pub(crate) repository: StorageRepository,
    pub(crate) summaries: HashMap<SessionId, UnreadableSessionSummary>,
    pub(crate) parents: HashMap<SessionId, Option<SessionId>>,
    pub(crate) child_ids: HashSet<SessionId>,
}

#[derive(Debug)]
pub(crate) enum StorageError {
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
    WriteLandingAgentSelection(String),
    BlockingTask {
        operation: &'static str,
        message: String,
    },
    WriterTask(String),
}

impl fmt::Display for StorageError {
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
                "Session database schema {database_version} is newer than this Suru binary (latest supported schema {binary_version})"
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
            Self::WriteLandingAgentSelection(message) => {
                write!(formatter, "save landing Agent Selection: {message}")
            }
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

impl std::error::Error for StorageError {}

impl StorageRepository {
    pub(crate) async fn open(data_root: &Path) -> Result<Self, StorageError> {
        let repository = Self {
            database_path: Arc::new(data_root.join(DATABASE_FILE)),
        };
        let database_path = repository.database_path.as_ref().clone();
        on_blocking_task("startup", move || initialize_database(&database_path)).await?;
        Ok(repository)
    }

    pub(crate) async fn load_sessions(&self) -> Result<RestoredSessions, StorageError> {
        let database_path = self.database_path.as_ref().clone();
        on_blocking_task("loading", move || load_sessions(&database_path)).await
    }

    pub(crate) async fn session(
        &self,
        session_id: SessionId,
    ) -> Result<Option<PersistedSession>, StorageError> {
        let path = self.database_path.clone();
        on_blocking_task("hydrate Session", move || {
            let mut connection = connect(&path)?;
            let row = sessions::table
                .filter(sessions::id.eq(session_id.to_string()))
                .select(SessionRow::as_select())
                .first(&mut connection)
                .optional()
                .map_err(|error| StorageError::Read(error.to_string()))?;
            row.map(|row| load_session(&mut connection, row))
                .transpose()
        })
        .await
    }

    pub(crate) async fn prompt_session(
        &self,
        prompt_id: PromptId,
    ) -> Result<Option<SessionId>, StorageError> {
        let path = self.database_path.clone();
        on_blocking_task("find Prompt owner", move || {
            let mut connection = connect(&path)?;
            let id = prompts::table
                .filter(prompts::id.eq(prompt_id.to_string()))
                .select(prompts::session_id)
                .first::<String>(&mut connection)
                .optional()
                .map_err(|error| StorageError::Read(error.to_string()))?;
            id.map(|id| {
                uuid::Uuid::parse_str(&id)
                    .map(SessionId::from_uuid)
                    .map_err(|error| StorageError::InvalidSession {
                        session_id: id,
                        message: error.to_string(),
                    })
            })
            .transpose()
        })
        .await
    }

    pub(crate) async fn landing_agent_selection(
        &self,
    ) -> Result<Option<AgentSelection>, StorageError> {
        let database_path = self.database_path.as_ref().clone();
        on_blocking_task("reading landing Agent Selection", move || {
            let mut connection = connect(&database_path)?;
            let row = landing_agent_selection::table
                .select(LandingAgentSelectionRow::as_select())
                .first::<LandingAgentSelectionRow>(&mut connection)
                .optional()
                .map_err(|error| StorageError::Read(error.to_string()))?;
            // A preference is best-effort stored state. If a future or damaged payload cannot
            // decode, preserve startup and fall back to the Provider's current default.
            Ok(row.and_then(LandingAgentSelectionRow::into_selection))
        })
        .await
    }

    fn save_sessions(&self, persisted: Vec<PersistedSession>) -> Result<(), StorageError> {
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

    fn delete_session(&self, session_id: SessionId) -> Result<(), StorageError> {
        let mut connection = connect(&self.database_path)?;
        diesel::delete(sessions::table.filter(sessions::id.eq(session_id.to_string())))
            .execute(&mut connection)
            .map_err(|error| StorageError::Write {
                session_id,
                message: error.to_string(),
            })?;
        Ok(())
    }

    fn save_landing_agent_selection(&self, selection: AgentSelection) -> Result<(), StorageError> {
        let row = LandingAgentSelectionRow::from_selection(selection)?;
        let mut connection = connect(&self.database_path)?;
        diesel::insert_into(landing_agent_selection::table)
            .values(&row)
            .on_conflict(landing_agent_selection::singleton)
            .do_update()
            .set(&row)
            .execute(&mut connection)
            .map_err(|error| StorageError::WriteLandingAgentSelection(error.to_string()))?;
        Ok(())
    }

    fn save_resume_state(&self, state: &StoredResumeState) -> Result<(), StorageError> {
        let session_id = state.session_id;
        let row = ProviderResumeStateRow::from_resume_state(state)?;
        let mut connection = connect(&self.database_path)?;
        diesel::insert_into(provider_resume_states::table)
            .values(&row)
            .on_conflict((
                provider_resume_states::session_id,
                provider_resume_states::provider,
            ))
            .do_update()
            .set(provider_resume_states::payload.eq(&row.payload))
            .execute(&mut connection)
            .map_err(|error| StorageError::Write {
                session_id,
                message: error.to_string(),
            })?;
        Ok(())
    }
}

fn load_sessions(database_path: &Path) -> Result<RestoredSessions, StorageError> {
    let mut connection = connect(database_path)?;
    let rows = sessions::table
        .select(SessionRow::as_select())
        .load::<SessionRow>(&mut connection)
        .map_err(|error| StorageError::Read(error.to_string()))?;
    // Turns contain timing, outcome, and metering, never Transcript content.
    // One ordered query avoids the per-Session query fanout during readiness.
    let turn_rows = turns::table
        .order((turns::session_id.asc(), turns::row_order.asc()))
        .select(TurnRow::as_select())
        .load::<TurnRow>(&mut connection)
        .map_err(|error| StorageError::Read(error.to_string()))?;
    let decode_started = std::time::Instant::now();
    let mut by_session: HashMap<String, Vec<TurnRow>> = HashMap::new();
    for turn in turn_rows {
        by_session
            .entry(turn.session_id.clone())
            .or_default()
            .push(turn);
    }
    let mut restored = RestoredSessions::default();
    let mut deferred = DeferredSessions {
        repository: StorageRepository {
            database_path: Arc::new(database_path.to_owned()),
        },
        summaries: HashMap::new(),
        parents: HashMap::new(),
        child_ids: HashSet::new(),
    };
    for row in rows {
        let unreadable = row.unreadable_summary()?;
        if row.is_child() {
            deferred.child_ids.insert(unreadable.id);
        }
        // Keep malformed parent metadata inside the per-Session decode
        // boundary below; an invalid link must never fail server startup.
        let parent = row.parent_id().ok().flatten();
        deferred.parents.insert(unreadable.id, parent);
        let turns = by_session.remove(&row.id).unwrap_or_default();
        let result = (|| {
            let (mut summary, revision) = row.into_summary_and_revision()?;
            let turns = turns
                .into_iter()
                .map(TurnRow::into_turn)
                .collect::<Result<Vec<_>, _>>()?;
            summary.standing_inputs.latest_turn =
                crate::protocol::SessionStandingInputs::from_turns(&turns).latest_turn;
            let snapshot = SessionSnapshot {
                session: summary.session.clone(),
                revision,
                turns,
                prompts: Vec::new(),
                messages: Vec::new(),
                activities: Vec::new(),
                transcript: Vec::new(),
                subagent_questionnaires: Vec::new(),
                subagent_usage: None,
            };
            summary.total_usage = snapshot.total_usage();
            Ok::<_, StorageError>(PersistedSession {
                summary,
                snapshot,
                resume_states: HashMap::new(),
            })
        })();
        match result {
            Ok(session) => {
                deferred.summaries.insert(unreadable.id, unreadable);
                restored.readable.push(session);
            }
            Err(StorageError::InvalidSession { .. }) => restored.unreadable.push(unreadable),
            Err(error) => return Err(error),
        }
    }
    restored.deferred = Some(deferred);
    tracing::debug!(
        storage_decode_us = decode_started.elapsed().as_micros() as u64,
        "Session metadata decoded"
    );
    Ok(restored)
}

fn load_session(
    connection: &mut SqliteConnection,
    row: SessionRow,
) -> Result<PersistedSession, StorageError> {
    let stored_session_id = row.id.clone();
    let (mut summary, revision) = row.into_summary_and_revision()?;
    let prompt_rows = prompts::table
        .filter(prompts::session_id.eq(&stored_session_id))
        .order(prompts::row_order.asc())
        .select(PromptRow::as_select())
        .load(connection)
        .map_err(|error| StorageError::Read(error.to_string()))?;
    let turn_rows = turns::table
        .filter(turns::session_id.eq(&stored_session_id))
        .order(turns::row_order.asc())
        .select(TurnRow::as_select())
        .load(connection)
        .map_err(|error| StorageError::Read(error.to_string()))?;
    let message_rows = messages::table
        .filter(messages::session_id.eq(&stored_session_id))
        .order(messages::row_order.asc())
        .select(MessageRow::as_select())
        .load(connection)
        .map_err(|error| StorageError::Read(error.to_string()))?;
    let activity_rows = activities::table
        .filter(activities::session_id.eq(&stored_session_id))
        .order(activities::row_order.asc())
        .select(ActivityRow::as_select())
        .load(connection)
        .map_err(|error| StorageError::Read(error.to_string()))?;
    let resume_state_rows = provider_resume_states::table
        .filter(provider_resume_states::session_id.eq(&stored_session_id))
        .select(ProviderResumeStateRow::as_select())
        .load::<ProviderResumeStateRow>(connection)
        .map_err(|error| StorageError::Read(error.to_string()))?;

    let decode_started = std::time::Instant::now();
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
    let resume_states = resume_state_rows
        .into_iter()
        .map(ProviderResumeStateRow::into_resume_state)
        .collect::<Result<HashMap<_, _>, StorageError>>()?;
    let mut transcript = decoded_messages
        .iter()
        .map(|(message, order)| {
            (
                *order,
                TranscriptItem::Message {
                    message_id: message.id,
                },
            )
        })
        .chain(decoded_activities.iter().map(|(activity, order)| {
            (
                *order,
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
        messages: decoded_messages
            .into_iter()
            .map(|(message, _)| message)
            .collect(),
        activities: decoded_activities
            .into_iter()
            .map(|(activity, _)| activity)
            .collect(),
        transcript: transcript.into_iter().map(|(_, item)| item).collect(),
        // A child's Usage lives in the child's own stored Turns, so the
        // roll-up is re-derived across the subtree once every Session is
        // loaded rather than stored twice.
        subagent_questionnaires: Vec::new(),
        subagent_usage: None,
    };
    // Working is reconstructed across the complete Session tree after every
    // stored Session has been loaded; one row cannot see that subtree here.
    summary.standing_inputs.latest_turn =
        crate::protocol::SessionStandingInputs::from_turns(&snapshot.turns).latest_turn;
    summary.total_usage = snapshot.total_usage();
    tracing::debug!(
        storage_decode_us = decode_started.elapsed().as_micros() as u64,
        "Session history decoded"
    );
    Ok(PersistedSession {
        summary,
        snapshot,
        resume_states,
    })
}

fn save_rows(connection: &mut SqliteConnection, rows: StoredRows) -> Result<(), StorageError> {
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
        .map_err(|error| StorageError::Write {
            session_id,
            message: error.to_string(),
        })
}

fn initialize_database(database_path: &Path) -> Result<(), StorageError> {
    let mut connection = connect(database_path)?;
    refuse_newer_schema(&mut connection)?;
    connection
        .run_pending_migrations(MIGRATIONS)
        .map_err(|error| StorageError::Migration(error.to_string()))?;
    protect_current_user_file(database_path).map_err(|error| StorageError::Open {
        path: database_path.to_owned(),
        message: error.to_string(),
    })
}

fn connect(database_path: &Path) -> Result<SqliteConnection, StorageError> {
    let database_url = database_path
        .to_str()
        .ok_or_else(|| StorageError::InvalidDatabasePath(database_path.to_owned()))?;
    let mut connection =
        SqliteConnection::establish(database_url).map_err(|error| StorageError::Open {
            path: database_path.to_owned(),
            message: error.to_string(),
        })?;
    connection.set_instrumentation(|event: diesel::connection::InstrumentationEvent<'_>| {
        if matches!(
            event,
            diesel::connection::InstrumentationEvent::StartQuery { .. }
        ) {
            tracing::debug!(storage_query = 1_u64, "Session repository query");
        }
    });
    connection
        .batch_execute(
            "PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;",
        )
        .map_err(|error| StorageError::Open {
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

fn refuse_newer_schema(connection: &mut SqliteConnection) -> Result<(), StorageError> {
    let migration_table = diesel::sql_query(
        "SELECT COUNT(*) AS value FROM sqlite_master WHERE type = 'table' AND name = '__diesel_schema_migrations'",
    )
    .get_result::<CountRow>(connection)
    .map_err(|error| StorageError::Read(error.to_string()))?;
    if migration_table.value == 0 {
        return Ok(());
    }
    let latest = diesel::sql_query(
        "SELECT version FROM __diesel_schema_migrations ORDER BY version DESC LIMIT 1",
    )
    .get_result::<VersionRow>(connection)
    .optional()
    .map_err(|error| StorageError::Read(error.to_string()))?;
    if let Some(latest) = latest
        && latest.version.as_str() > CURRENT_SCHEMA_VERSION
    {
        return Err(StorageError::NewerSchema {
            database_version: latest.version,
            binary_version: CURRENT_SCHEMA_VERSION,
        });
    }
    Ok(())
}

async fn on_blocking_task<T>(
    operation: &'static str,
    task: impl FnOnce() -> Result<T, StorageError> + Send + 'static,
) -> Result<T, StorageError>
where
    T: Send + 'static,
{
    tokio::task::spawn_blocking(task)
        .await
        .map_err(|error| StorageError::BlockingTask {
            operation,
            message: error.to_string(),
        })?
}
