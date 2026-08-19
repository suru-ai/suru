//! Durable Session metadata behind a domain-oriented repository seam.

use std::{
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
};

use diesel::{
    OptionalExtension, QueryableByName, SqliteConnection,
    connection::SimpleConnection,
    deserialize::Queryable,
    prelude::*,
    sql_types::{BigInt, Text},
};
use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};
use serde::{Serialize, de::DeserializeOwned};
use tokio::{sync::mpsc, task::JoinHandle};
use uuid::Uuid;

use crate::{
    protocol::{Session, SessionId, SessionSummary, SessionTimestamp},
    runtime::protect_current_user_file,
};

const DATABASE_FILE: &str = "chidori.db";
const CURRENT_SCHEMA_VERSION: &str = "20260820000000";
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
    }
}

#[derive(Clone)]
pub(crate) struct SessionRepository {
    database_path: Arc<PathBuf>,
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
            Self::Read(message) => write!(formatter, "read Session metadata: {message}"),
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
            Self::BlockingTask { operation, message } => {
                write!(
                    formatter,
                    "Session repository {operation} task failed: {message}"
                )
            }
            Self::WriterTask(message) => {
                write!(formatter, "Session metadata writer task failed: {message}")
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

    pub(crate) async fn list_sessions(
        &self,
    ) -> Result<Vec<SessionSummary>, SessionRepositoryError> {
        let database_path = self.database_path.as_ref().clone();
        on_blocking_task("listing", move || {
            let mut connection = connect(&database_path)?;
            sessions::table
                .select(SessionRow::as_select())
                .load::<SessionRow>(&mut connection)
                .map_err(|error| SessionRepositoryError::Read(error.to_string()))?
                .into_iter()
                .map(SessionRow::into_summary)
                .collect()
        })
        .await
    }

    async fn save_session(&self, summary: SessionSummary) -> Result<(), SessionRepositoryError> {
        let database_path = self.database_path.as_ref().clone();
        on_blocking_task("write", move || {
            let session_id = summary.session.id;
            let row = SessionRow::from_summary(summary)?;
            let mut connection = connect(&database_path)?;
            diesel::insert_into(sessions::table)
                .values(&row)
                .on_conflict(sessions::id)
                .do_update()
                .set(&row)
                .execute(&mut connection)
                .map_err(|error| SessionRepositoryError::Write {
                    session_id,
                    message: error.to_string(),
                })?;
            Ok(())
        })
        .await
    }
}

#[derive(Clone)]
pub(crate) struct SessionMetadataSink {
    commands: mpsc::UnboundedSender<WriterCommand>,
}

pub(crate) struct SessionMetadataWriter {
    commands: mpsc::UnboundedSender<WriterCommand>,
    task: JoinHandle<Result<(), SessionRepositoryError>>,
}

enum WriterCommand {
    Save(SessionSummary),
    Shutdown,
}

impl SessionMetadataWriter {
    pub(crate) fn spawn(repository: SessionRepository) -> (Self, SessionMetadataSink) {
        let (commands, mut receiver) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            while let Some(command) = receiver.recv().await {
                match command {
                    WriterCommand::Save(summary) => repository.save_session(summary).await?,
                    WriterCommand::Shutdown => break,
                }
            }
            Ok(())
        });
        (
            Self {
                commands: commands.clone(),
                task,
            },
            SessionMetadataSink { commands },
        )
    }

    pub(crate) async fn shutdown(self) -> Result<(), SessionRepositoryError> {
        let _ = self.commands.send(WriterCommand::Shutdown);
        self.task
            .await
            .map_err(|error| SessionRepositoryError::WriterTask(error.to_string()))?
    }
}

impl SessionMetadataSink {
    pub(crate) fn save(&self, summary: SessionSummary) {
        // Shutdown closes the receiver only after HTTP and Provider mutation sources have stopped.
        let _ = self.commands.send(WriterCommand::Save(summary));
    }
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
}

impl SessionRow {
    fn from_summary(summary: SessionSummary) -> Result<Self, SessionRepositoryError> {
        let session_id = summary.session.id;
        Ok(Self {
            id: session_id.to_string(),
            title: summary.title,
            created_at: timestamp_to_i64(session_id, "created_at", summary.created_at)?,
            updated_at: timestamp_to_i64(session_id, "updated_at", summary.updated_at)?,
            workspace: encode(session_id, "Workspace", &summary.session.workspace)?,
            agent_selection: summary
                .session
                .agent_selection
                .as_ref()
                .map(|selection| encode(session_id, "Agent Selection", selection))
                .transpose()?,
            agent_selection_availability: encode(
                session_id,
                "Agent Selection availability",
                &summary.session.agent_selection_availability,
            )?,
            status: encode(session_id, "Session status", &summary.session.status)?,
        })
    }

    fn into_summary(self) -> Result<SessionSummary, SessionRepositoryError> {
        let session_id = self.id.clone();
        let id = Uuid::parse_str(&self.id)
            .map(SessionId::from_uuid)
            .map_err(|error| invalid_session(&session_id, "Session ID", error))?;
        Ok(SessionSummary {
            session: Session {
                id,
                workspace: decode(&session_id, "Workspace", &self.workspace)?,
                agent_selection: self
                    .agent_selection
                    .as_deref()
                    .map(|value| decode(&session_id, "Agent Selection", value))
                    .transpose()?,
                agent_selection_availability: decode(
                    &session_id,
                    "Agent Selection availability",
                    &self.agent_selection_availability,
                )?,
                status: decode(&session_id, "Session status", &self.status)?,
            },
            title: self.title,
            created_at: timestamp_from_i64(&session_id, "created_at", self.created_at)?,
            updated_at: timestamp_from_i64(&session_id, "updated_at", self.updated_at)?,
        })
    }
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

fn timestamp_to_i64(
    session_id: SessionId,
    field: &'static str,
    timestamp: SessionTimestamp,
) -> Result<i64, SessionRepositoryError> {
    i64::try_from(timestamp.0).map_err(|error| SessionRepositoryError::Write {
        session_id,
        message: format!("encode {field}: {error}"),
    })
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

fn timestamp_from_i64(
    session_id: &str,
    field: &'static str,
    timestamp: i64,
) -> Result<SessionTimestamp, SessionRepositoryError> {
    u64::try_from(timestamp)
        .map(SessionTimestamp)
        .map_err(|error| invalid_session(session_id, field, error))
}

fn decode<T: DeserializeOwned>(
    session_id: &str,
    field: &'static str,
    value: &str,
) -> Result<T, SessionRepositoryError> {
    serde_json::from_str(value).map_err(|error| invalid_session(session_id, field, error))
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
