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
    model_catalog::RememberedProviderCatalog,
    protocol::{
        AgentSelection, CheckoutAssociation, ExecutionDirectory, PromptId, ProviderId, SessionId,
        SessionSnapshot, SessionSummary, SessionTimestamp, TranscriptItem,
        UnreadableSessionSummary, WorkspaceId,
    },
    provider::{ProviderResumeState, ProviderSubagentId},
    runtime::protect_current_user_file,
};

mod attachment_table;
mod rows;
mod writer;

pub(crate) use writer::{StorageSink, StorageWriter};

use rows::{
    ActivityRow, LandingAgentSelectionRow, MessageRow, ModelCatalogRow, PromptRow,
    ProviderResumeStateRow, ProviderSubagentIdentityRow, SessionRow, StoredRows, TurnRow,
    WorkspaceRow,
};

const DATABASE_FILE: &str = "suru.db";
const CURRENT_SCHEMA_VERSION: &str = "20260928000000";
const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

diesel::table! {
    sessions (id) {
        id -> Text,
        title -> Text,
        icon -> Nullable<Text>,
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
        context_fill -> Nullable<Text>,
        brokered -> Bool,
    }
}

diesel::table! {
    attachments (id) {
        id -> Text,
        mime_type -> Text,
        byte_length -> BigInt,
        width -> Nullable<BigInt>,
        height -> Nullable<BigInt>,
        referenced_at -> BigInt,
        bytes -> Binary,
    }
}

diesel::table! {
    session_attachments (session_id, attachment_id) {
        session_id -> Text,
        attachment_id -> Text,
    }
}

diesel::allow_tables_to_appear_in_same_query!(attachments, session_attachments);

diesel::table! {
    landing_agent_selection (singleton) {
        singleton -> Integer,
        selection -> Text,
    }
}

diesel::table! {
    model_catalog (provider) {
        provider -> Text,
        payload -> Text,
        discovered_at -> BigInt,
    }
}

diesel::table! {
    workspaces (id) {
        id -> Text,
        icon -> Nullable<Text>,
        created_at -> BigInt,
        updated_at -> BigInt,
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
    provider_subagent_identities (session_id) {
        session_id -> Text,
        provider -> Text,
        subagent_id -> Text,
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
    /// How long after it was last uploaded or bound an Attachment outlives
    /// the deletion of the last Session referencing it.
    attachment_grace: std::time::Duration,
    /// Where the time an Attachment's grace is measured by is read.
    clock: crate::clock::ServerClock,
}

#[derive(Clone)]
pub(crate) struct PersistedSession {
    pub(crate) summary: SessionSummary,
    pub(crate) snapshot: SessionSnapshot,
    pub(crate) resume_states: HashMap<ProviderId, ProviderResumeState>,
    /// Set exactly on a Subagent's child Session: the Provider's own identity
    /// for the Subagent it is, stored with it from its spawn.
    pub(crate) subagent_identity: Option<StoredSubagentIdentity>,
    /// Whether the Session is a brokered Subagent's, which gives it a
    /// Provider actor of its own (ADR 0035). Fixed at the spawn and stored
    /// with the Session, so the next process routes its Provider work the way
    /// this one did.
    pub(crate) brokered: bool,
}

impl PersistedSession {
    /// A Session just created, before its Provider has kept anything for it:
    /// no Resume State, and — until the caller says otherwise — nothing its
    /// spawn fixed about it, as for a top-level Session.
    pub(crate) fn created(summary: SessionSummary, snapshot: SessionSnapshot) -> Self {
        Self {
            summary,
            snapshot,
            resume_states: HashMap::new(),
            subagent_identity: None,
            brokered: false,
        }
    }
}

/// The Provider's own identity for the Subagent a child Session is — Claude's
/// task id, Codex's child thread id — beside the Provider that minted it, which
/// is the only one that can name it again. A resume after a restart names the
/// Subagent by it, and this is what finds the Session that resume continues
/// (ADR 0031).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredSubagentIdentity {
    pub(crate) provider: ProviderId,
    pub(crate) subagent_id: ProviderSubagentId,
}

pub(crate) struct StoredResumeState {
    pub(crate) session_id: SessionId,
    pub(crate) provider: ProviderId,
    pub(crate) resume_state: ProviderResumeState,
}

#[derive(Default)]
pub(crate) struct RestoredSessions {
    pub(crate) readable: Vec<PersistedSession>,
    pub(crate) unreadable: Vec<UnreadableStoredSession>,
    pub(crate) deferred: Option<DeferredSessions>,
}

/// Catalog facts that remain trustworthy when a Session's full stored shape
/// cannot be decoded. Location facts let automatic Worktree removal preserve
/// a possible reference until the unreadable Session is explicitly deleted.
pub(crate) struct UnreadableStoredSession {
    pub(crate) summary: UnreadableSessionSummary,
    pub(crate) checkout: Option<CheckoutAssociation>,
    pub(crate) execution_directory: Option<ExecutionDirectory>,
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
    WriteModelCatalog(String),
    WriteWorkspaceIcon(String),
    WriteAttachment(String),
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
            Self::WriteModelCatalog(message) => {
                write!(formatter, "save remembered Model Catalog: {message}")
            }
            Self::WriteWorkspaceIcon(message) => {
                write!(formatter, "save a Workspace Icon: {message}")
            }
            Self::WriteAttachment(message) => write!(formatter, "save an Attachment: {message}"),
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
            attachment_grace: crate::attachments::ATTACHMENT_GRACE,
            clock: crate::clock::ServerClock::default(),
        };
        let database_path = repository.database_path.as_ref().clone();
        on_blocking_task("startup", move || initialize_database(&database_path)).await?;
        Ok(repository)
    }

    /// Leaves an Attachment in place for `grace` after it was last uploaded
    /// or bound, even once the last Session referencing it is deleted.
    pub(crate) fn with_attachment_grace(mut self, grace: std::time::Duration) -> Self {
        self.attachment_grace = grace;
        self
    }

    /// Measures an Attachment's grace by `clock`.
    pub(crate) fn with_clock(mut self, clock: crate::clock::ServerClock) -> Self {
        self.clock = clock;
        self
    }

    pub(crate) async fn load_sessions(&self) -> Result<RestoredSessions, StorageError> {
        let repository = self.clone();
        on_blocking_task("loading", move || load_sessions(repository)).await
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

    /// The Model Catalog each Provider served the last time discovery
    /// succeeded, as remembered across restarts. A row that no longer decodes
    /// is left out rather than failing startup: the Provider is asked again as
    /// soon as a client connects.
    pub(crate) async fn model_catalog(
        &self,
    ) -> Result<Vec<RememberedProviderCatalog>, StorageError> {
        let database_path = self.database_path.as_ref().clone();
        on_blocking_task("reading remembered Model Catalog", move || {
            let mut connection = connect(&database_path)?;
            let rows = model_catalog::table
                .select(ModelCatalogRow::as_select())
                .load::<ModelCatalogRow>(&mut connection)
                .map_err(|error| StorageError::Read(error.to_string()))?;
            Ok(rows
                .into_iter()
                .filter_map(ModelCatalogRow::into_remembered)
                .collect())
        })
        .await
    }

    /// Every Workspace's Icon this server has ever derived, as the `workspaces`
    /// table holds it — its whole reading, since the table carries nothing
    /// else (ADR 0027). A row that no longer decodes is left out rather than
    /// failing startup: the next Session created in that Workspace derives
    /// one again, exactly as if none had ever landed.
    pub(crate) async fn workspace_icons(
        &self,
    ) -> Result<HashMap<WorkspaceId, String>, StorageError> {
        let database_path = self.database_path.as_ref().clone();
        on_blocking_task("reading Workspace Icons", move || {
            let mut connection = connect(&database_path)?;
            let rows = workspaces::table
                .select(WorkspaceRow::as_select())
                .load::<WorkspaceRow>(&mut connection)
                .map_err(|error| StorageError::Read(error.to_string()))?;
            Ok(rows
                .into_iter()
                .filter_map(WorkspaceRow::into_icon)
                .collect())
        })
        .await
    }

    fn save_model_catalog(
        &self,
        remembered: RememberedProviderCatalog,
    ) -> Result<(), StorageError> {
        let row = ModelCatalogRow::from_remembered(remembered)?;
        let mut connection = connect(&self.database_path)?;
        diesel::insert_into(model_catalog::table)
            .values(&row)
            .on_conflict(model_catalog::provider)
            .do_update()
            .set(&row)
            .execute(&mut connection)
            .map_err(|error| StorageError::WriteModelCatalog(error.to_string()))?;
        Ok(())
    }

    /// Records a Workspace's Icon for good. `on_conflict` guards the same
    /// invariant the caller already checked in memory — a Workspace's Icon,
    /// once landed, is never replaced — against two Sessions in the same
    /// Workspace racing derivations that both succeed: whichever write lands
    /// first in the database wins, and the second's is silently a no-op
    /// rather than a later derivation overwriting an earlier one.
    fn save_workspace_icon(
        &self,
        workspace_id: WorkspaceId,
        icon: String,
    ) -> Result<(), StorageError> {
        let row = WorkspaceRow::from_icon(workspace_id, icon, SessionTimestamp::now());
        let mut connection = connect(&self.database_path)?;
        diesel::insert_into(workspaces::table)
            .values(&row)
            .on_conflict(workspaces::id)
            .do_nothing()
            .execute(&mut connection)
            .map_err(|error| StorageError::WriteWorkspaceIcon(error.to_string()))?;
        Ok(())
    }

    /// Records a Workspace's Icon for good, replacing whatever the table
    /// already held for it. Where [`Self::save_workspace_icon`]'s
    /// `on_conflict` guards a derivation's write-once rule, this is the other
    /// half of that same rule: a user's choice always stands, so it must land
    /// whether the table already carries a derived Icon, an earlier choice, or
    /// nothing at all — an upsert rather than an insert-if-absent. Only the
    /// Icon and `updated_at` move on conflict; `created_at` stays whatever the
    /// row's first write stamped it, exactly as an ordinary update would leave
    /// it.
    fn replace_workspace_icon(
        &self,
        workspace_id: WorkspaceId,
        icon: String,
    ) -> Result<(), StorageError> {
        let stamp = SessionTimestamp::now();
        let row = WorkspaceRow::from_icon(workspace_id, icon.clone(), stamp);
        let updated_at = i64::try_from(stamp.0).unwrap_or(i64::MAX);
        let mut connection = connect(&self.database_path)?;
        diesel::insert_into(workspaces::table)
            .values(&row)
            .on_conflict(workspaces::id)
            .do_update()
            .set((
                workspaces::icon.eq(Some(icon)),
                workspaces::updated_at.eq(updated_at),
            ))
            .execute(&mut connection)
            .map_err(|error| StorageError::WriteWorkspaceIcon(error.to_string()))?;
        Ok(())
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

    fn save_location(
        &self,
        session: &crate::protocol::Session,
        revision: crate::protocol::SessionRevision,
    ) -> Result<(), StorageError> {
        let mut connection = connect(&self.database_path)?;
        let revision = i64::try_from(revision.0).map_err(|error| StorageError::Write {
            session_id: session.id,
            message: error.to_string(),
        })?;
        diesel::update(sessions::table.filter(sessions::id.eq(session.id.to_string())))
            .set((
                sessions::workspace.eq(SessionRow::session_metadata_payload(session)?),
                sessions::revision.eq(revision),
            ))
            .execute(&mut connection)
            .map_err(|error| StorageError::Write {
                session_id: session.id,
                message: error.to_string(),
            })?;
        Ok(())
    }

    /// Deletes a Session's rows, and with them every Attachment it references
    /// that no other Session does and that was last uploaded or bound longer
    /// ago than the grace period. A younger one may be bound by a Prompt still
    /// in admission, so it waits for the orphan sweep instead.
    fn delete_session(&self, session_id: SessionId) -> Result<(), StorageError> {
        let mut connection = connect(&self.database_path)?;
        let id = session_id.to_string();
        let referenced_before = attachment_table::millis(self.clock.now())
            .saturating_sub(i64::try_from(self.attachment_grace.as_millis()).unwrap_or(i64::MAX));
        connection
            .transaction::<_, diesel::result::Error, _>(|connection| {
                let joined = attachment_table::session_attachment_ids(connection, &id)?;
                diesel::delete(sessions::table.filter(sessions::id.eq(&id))).execute(connection)?;
                attachment_table::delete_unjoined_attachments(
                    connection,
                    &joined,
                    referenced_before,
                )
            })
            .map_err(|error| StorageError::Write {
                session_id,
                message: error.to_string(),
            })
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

fn load_sessions(repository: StorageRepository) -> Result<RestoredSessions, StorageError> {
    let mut connection = connect(&repository.database_path)?;
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
        repository,
        summaries: HashMap::new(),
        parents: HashMap::new(),
        child_ids: HashSet::new(),
    };
    for row in rows {
        let unreadable = row.unreadable_session()?;
        if row.is_child() {
            deferred.child_ids.insert(unreadable.summary.id);
        }
        // Keep malformed parent metadata inside the per-Session decode
        // boundary below; an invalid link must never fail server startup.
        let parent = row.parent_id().ok().flatten();
        deferred.parents.insert(unreadable.summary.id, parent);
        let turns = by_session.remove(&row.id).unwrap_or_default();
        let brokered = row.brokered();
        let result = (|| {
            let (mut summary, revision) = row.into_summary_and_revision()?;
            let turns = turns
                .into_iter()
                .map(TurnRow::into_turn)
                .collect::<Result<Vec<_>, _>>()?;
            summary.standing_inputs.latest_turn =
                crate::protocol::SessionStandingInputs::from_turns(&turns).latest_turn;
            let snapshot = SessionSnapshot {
                title: summary.title.clone(),
                icon: summary.icon.clone(),
                session: summary.session.clone(),
                revision,
                turns,
                prompts: Vec::new(),
                messages: Vec::new(),
                activities: Vec::new(),
                transcript: Vec::new(),
                subagent_interventions: Vec::new(),
                pending_approvals: Vec::new(),
                submitting_approvals: Vec::new(),
                pending_approvals_revision: crate::protocol::SessionRevision(0),
                watches: Vec::new(),
                subagent_usage: None,
                total_cost: None,
            };
            summary.total_usage = snapshot.total_usage();
            Ok::<_, StorageError>(PersistedSession {
                summary,
                snapshot,
                resume_states: HashMap::new(),
                subagent_identity: None,
                brokered,
            })
        })();
        match result {
            Ok(session) => {
                deferred
                    .summaries
                    .insert(unreadable.summary.id, unreadable.summary);
                restored.readable.push(session);
            }
            Err(error @ StorageError::InvalidSession { .. }) => {
                tracing::warn!(%error, "listing persisted Session as unreadable");
                restored.unreadable.push(unreadable);
            }
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
    let brokered = row.brokered();
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
    let subagent_identity_row = provider_subagent_identities::table
        .filter(provider_subagent_identities::session_id.eq(&stored_session_id))
        .select(ProviderSubagentIdentityRow::as_select())
        .first::<ProviderSubagentIdentityRow>(connection)
        .optional()
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
        title: summary.title.clone(),
        icon: summary.icon.clone(),
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
        subagent_interventions: Vec::new(),
        pending_approvals: Vec::new(),
        submitting_approvals: Vec::new(),
        pending_approvals_revision: crate::protocol::SessionRevision(0),
        watches: Vec::new(),
        subagent_usage: None,
        total_cost: None,
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
        subagent_identity: subagent_identity_row.map(ProviderSubagentIdentityRow::into_identity),
        brokered,
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
            attachment_table::join_session_attachments(
                connection,
                &rows.session.id,
                &rows.attachment_ids,
            )?;
            if let Some(identity) = &rows.subagent_identity {
                diesel::insert_into(provider_subagent_identities::table)
                    .values(identity)
                    .on_conflict(provider_subagent_identities::session_id)
                    .do_update()
                    .set(identity)
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
    connect_with_busy_timeout(database_path, std::time::Duration::from_secs(5))
}

fn connect_with_busy_timeout(
    database_path: &Path,
    busy_timeout: std::time::Duration,
) -> Result<SqliteConnection, StorageError> {
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
    // Journal-mode setup itself can encounter a concurrent writer, so install
    // the busy handler before any operation that needs a database lock.
    connection
        .batch_execute(&format!(
            "PRAGMA busy_timeout = {}; PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;",
            busy_timeout.as_millis()
        ))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_setup_honors_busy_timeout_while_database_is_locked() {
        let directory = tempfile::tempdir().expect("create database directory");
        let path = directory.path().join("locked.db");
        let mut owner = SqliteConnection::establish(path.to_str().expect("UTF-8 fixture path"))
            .expect("open lock owner");
        owner
            .batch_execute("CREATE TABLE fixture (id INTEGER); BEGIN EXCLUSIVE;")
            .expect("hold an exclusive database lock");

        let wait = std::time::Duration::from_millis(20);
        let started = std::time::Instant::now();
        let result = connect_with_busy_timeout(&path, wait);
        let elapsed = started.elapsed();
        assert!(
            matches!(result, Err(StorageError::Open { ref message, .. }) if message.contains("locked"))
        );
        assert!(
            elapsed >= wait,
            "connection setup must wait for the configured busy timeout; returned after {elapsed:?}"
        );
        owner
            .batch_execute("ROLLBACK;")
            .expect("release database lock");
        connect_with_busy_timeout(&path, wait).expect("connect after the lock is released");
    }
}
