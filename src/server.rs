use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::to_bytes,
    extract::{Path as AxumPath, Query, Request, State},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response, sse::Event, sse::Sse},
    routing::{get, post, put},
};
use fs2::FileExt;
use futures_util::{StreamExt, stream};
use serde::{Deserialize, de::DeserializeOwned};
use tokio::{
    net::TcpListener,
    sync::{broadcast, oneshot, watch},
    task::JoinHandle,
    time::{Duration, Instant},
};
use uuid::Uuid;

use crate::RuntimeConfig;
use crate::build_identity;
use crate::errands::{DEFAULT_ERRAND_TIMEOUT, ErrandRunner};
use crate::model_catalog::ModelCatalogService;
use crate::protocol::{
    Activity, AdmitPromptRequest, AgentSelection, CreateSessionRequest, LifecycleState, Message,
    MessageId, MessageRole, MessageStatus, PROTOCOL_VERSION, ProviderId, RuntimeDescriptor,
    SERVER_SHUTDOWN_EVENT, SESSION_CATALOG_SNAPSHOT_EVENT, SESSION_CATALOG_UPDATED_EVENT,
    SESSION_SNAPSHOT_EVENT, SESSION_UPDATED_EVENT, SETTINGS_SNAPSHOT_EVENT, ServerIdentity,
    ServerShutdown, SessionCatalogRevision, SessionChange, SessionError, SessionErrorCode,
    SessionId, SessionRevision, SessionUpdate, SettingMutation, SettingsSnapshot, ShutdownReason,
    TurnId, UpdateAgentSelectionRequest,
};
use crate::provider::{
    ProviderOrchestrator, ProviderRuntime, ProviderUpdateGate, built_in_runtimes, wait_for_shutdown,
};
use crate::runtime::protect_current_user_file;
use crate::sessions::{
    AdmitPromptError, AgentSelectionMutationError, CreateSessionError, DeleteSessionError,
    InterruptTurnError, ListSessionsError, PromptAdmissionDisposition, PromptMutationError,
    SessionCatalogFeed, SessionFeed, SessionStore, StoreOutcome, TitleDerivation,
};
use crate::settings::{ConfigDocuments, SettingsMutationError};
use crate::storage::{StorageRepository, StorageSink, StorageWriter};

pub type ServerConfig = RuntimeConfig;

/// Wall-clock intervals the server schedules against; injectable so tests can
/// observe periodic behavior without waiting out production-scale delays.
#[derive(Clone, Copy, Debug)]
pub struct ServerTimings {
    pub sse_keepalive_interval: Duration,
    /// How long an accepted shutdown keeps health and existing streams
    /// available so the final authenticated intent can reach clients before
    /// graceful transport closure.
    pub shutdown_grace: Duration,
    /// How long an Errand may take before Suru stops waiting on it.
    pub errand_timeout: Duration,
}

impl Default for ServerTimings {
    fn default() -> Self {
        Self {
            sse_keepalive_interval: Duration::from_secs(10),
            shutdown_grace: Duration::from_millis(100),
            errand_timeout: DEFAULT_ERRAND_TIMEOUT,
        }
    }
}

impl ServerTimings {
    /// Bounds how long an Errand may take; injectable so tests exercise a
    /// Provider that never answers without waiting out the default.
    pub fn with_errand_timeout(mut self, timeout: Duration) -> Self {
        self.errand_timeout = timeout;
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentOutput {
    MessageStarted {
        message_id: MessageId,
        turn_id: TurnId,
    },
    MessageDelta {
        message_id: MessageId,
        content: String,
    },
    MessageCompleted {
        message_id: MessageId,
    },
    Activity {
        activity: Activity,
    },
}

#[derive(Clone)]
pub struct AgentOutputSink {
    session_events: SessionEventSink,
}

impl AgentOutputSink {
    pub fn emit(&self, session_id: SessionId, output: AgentOutput) -> Result<SessionUpdate> {
        let change = match output {
            AgentOutput::MessageStarted {
                message_id,
                turn_id,
            } => SessionChange::MessageAdded {
                message: Message {
                    id: message_id,
                    turn_id,
                    role: MessageRole::Agent,
                    status: MessageStatus::Streaming,
                    content: String::new(),
                    skill_invocations: Vec::new(),
                    truncated: false,
                },
            },
            AgentOutput::MessageDelta {
                message_id,
                content,
            } => SessionChange::MessageContentAppended {
                message_id,
                content,
            },
            AgentOutput::MessageCompleted { message_id } => {
                SessionChange::MessageCompleted { message_id }
            }
            AgentOutput::Activity { activity } => SessionChange::ActivityAdded { activity },
        };
        if *self.session_events.lifecycle.borrow() != LifecycleState::Ready {
            anyhow::bail!("server is not accepting Session updates");
        }
        self.session_events
            .sessions
            .publish_agent_output(session_id, change)
    }

    pub fn continuation_boundary(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> Result<Vec<crate::protocol::Prompt>> {
        if *self.session_events.lifecycle.borrow() != LifecycleState::Ready {
            anyhow::bail!("server is not accepting Session updates");
        }
        self.session_events
            .sessions
            .continuation_boundary(session_id, turn_id)
    }
}

pub struct RunningServer {
    descriptor: RuntimeDescriptor,
    session_events: SessionEventSink,
    shutdown: ShutdownController,
    task: JoinHandle<Result<()>>,
}

#[derive(Clone)]
pub struct SessionEventSink {
    sessions: SessionStore,
    lifecycle: watch::Receiver<LifecycleState>,
}

impl SessionEventSink {
    pub fn publish(
        &self,
        session_id: SessionId,
        changes: Vec<SessionChange>,
    ) -> Result<SessionUpdate> {
        if *self.lifecycle.borrow() != LifecycleState::Ready {
            anyhow::bail!("server is not accepting Session updates");
        }
        self.sessions.publish(session_id, changes)
    }
}

impl RunningServer {
    pub fn descriptor(&self) -> &RuntimeDescriptor {
        &self.descriptor
    }

    pub fn session_event_sink(&self) -> SessionEventSink {
        self.session_events.clone()
    }

    pub fn agent_output(&self) -> AgentOutputSink {
        AgentOutputSink {
            session_events: self.session_events.clone(),
        }
    }

    pub async fn shutdown(self) -> Result<()> {
        self.request_shutdown();
        self.task.await.context("server task panicked")?
    }

    pub async fn run_until_ctrl_c(mut self) -> Result<()> {
        tokio::select! {
            task = &mut self.task => task.context("server task panicked")?,
            signal = tokio::signal::ctrl_c() => {
                signal.context("listen for Ctrl-C")?;
                self.request_shutdown();
                self.task.await.context("server task panicked")?
            }
        }
    }

    fn request_shutdown(&self) {
        self.shutdown.request(ServerShutdown {
            instance_id: self.descriptor.identity.instance_id,
            reason: ShutdownReason::Manual,
        });
    }
}

#[derive(Clone)]
struct ShutdownController {
    lifecycle: watch::Sender<LifecycleState>,
    shutdown_intent: watch::Sender<Option<ServerShutdown>>,
    shutdown: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    provider_shutdown: watch::Sender<bool>,
    provider_updates: ProviderUpdateGate,
    shutdown_grace: Duration,
}

impl ShutdownController {
    fn lifecycle(&self) -> LifecycleState {
        self.lifecycle.borrow().clone()
    }

    fn subscribe_to_intent(&self) -> watch::Receiver<Option<ServerShutdown>> {
        self.shutdown_intent.subscribe()
    }

    fn request(&self, request: ServerShutdown) {
        let shutdown = self
            .shutdown
            .lock()
            .expect("shutdown sender lock is not poisoned")
            .take();
        let Some(shutdown) = shutdown else {
            return;
        };
        tracing::info!(reason = ?request.reason, "server shutdown accepted");
        self.provider_updates.stop();
        self.lifecycle.send_replace(LifecycleState::Stopping);
        self.shutdown_intent.send_replace(Some(request));
        self.provider_shutdown.send_replace(true);
        let grace = self.shutdown_grace;
        tokio::spawn(async move {
            // Keep health and existing streams available briefly so the accepted response and
            // final authenticated intent can reach clients before graceful transport closure.
            tokio::time::sleep(grace).await;
            let _ = shutdown.send(());
        });
    }

    fn stop_providers(&self) {
        self.provider_updates.stop();
        self.provider_shutdown.send_replace(true);
    }
}

#[derive(Clone)]
struct LandingAgentSelectionStore {
    current: Arc<Mutex<Option<AgentSelection>>>,
    storage: StorageSink,
}

impl LandingAgentSelectionStore {
    fn new(current: Option<AgentSelection>, storage: StorageSink) -> Self {
        Self {
            current: Arc::new(Mutex::new(current)),
            storage,
        }
    }

    fn current(&self) -> Option<AgentSelection> {
        self.current
            .lock()
            .expect("landing Agent Selection lock is not poisoned")
            .clone()
    }

    fn confirm(&self, selection: AgentSelection) {
        let mut current = self
            .current
            .lock()
            .expect("landing Agent Selection lock is not poisoned");
        self.storage.save_landing_agent_selection(selection.clone());
        *current = Some(selection);
    }
}

#[derive(Clone)]
struct AppState {
    descriptor: Arc<RuntimeDescriptor>,
    sessions: SessionStore,
    providers: ProviderOrchestrator,
    /// Derives a Session's Title from its first Prompt, in the background and
    /// beside the first Turn rather than in front of it.
    title_derivation: TitleDerivation,
    model_catalog: ModelCatalogService,
    landing_agent_selection: LandingAgentSelectionStore,
    /// The loaded effective-settings view. A watch channel so every attached
    /// lifecycle stream re-pushes the snapshot when a mutation (or a future
    /// Config Document watcher) replaces it.
    settings: Arc<watch::Sender<SettingsSnapshot>>,
    /// The Config Documents behind that view, which this server alone writes.
    config_documents: ConfigDocuments,
    /// Held so an accepted mutation can hand every hosted Provider runtime
    /// the Server Settings it now runs under.
    runtimes: Arc<Vec<Arc<dyn ProviderRuntime>>>,
    /// The Providers this server hosts, in the fixed built-in order. Agent
    /// Selections normalize against this set rather than any single Provider
    /// identity.
    hosted_providers: Arc<Vec<ProviderId>>,
    shutdown: ShutdownController,
    timings: ServerTimings,
}

impl AppState {
    /// Whether a remembered Agent Selection may still be handed to a new
    /// Session: its Provider is one this server hosts, and one the user has
    /// left enabled. Availability is deliberately not asked here — a Provider
    /// the user can fix from outside Suru keeps the selection they made, and
    /// the fresh-Landing default behind this is what passes over one that
    /// cannot work.
    fn is_selectable_provider(&self, provider: &ProviderId) -> bool {
        self.hosted_providers.contains(provider)
            && self.settings.borrow().settings.provider_enabled(provider)
    }
}

pub async fn spawn(config: ServerConfig) -> Result<RunningServer> {
    spawn_with_providers(config, built_in_runtimes()).await
}

pub async fn spawn_with_provider(
    config: ServerConfig,
    runtime: Arc<dyn ProviderRuntime>,
) -> Result<RunningServer> {
    spawn_with_providers(config, vec![runtime]).await
}

pub async fn spawn_with_providers(
    config: ServerConfig,
    runtimes: Vec<Arc<dyn ProviderRuntime>>,
) -> Result<RunningServer> {
    spawn_with_providers_and_timings(config, runtimes, ServerTimings::default()).await
}

pub async fn spawn_with_timings(
    config: ServerConfig,
    timings: ServerTimings,
) -> Result<RunningServer> {
    spawn_with_providers_and_timings(config, built_in_runtimes(), timings).await
}

pub async fn spawn_with_provider_and_timings(
    config: ServerConfig,
    runtime: Arc<dyn ProviderRuntime>,
    timings: ServerTimings,
) -> Result<RunningServer> {
    spawn_with_providers_and_timings(config, vec![runtime], timings).await
}

pub async fn spawn_with_providers_and_timings(
    config: ServerConfig,
    runtimes: Vec<Arc<dyn ProviderRuntime>>,
    timings: ServerTimings,
) -> Result<RunningServer> {
    anyhow::ensure!(
        !runtimes.is_empty(),
        "server requires at least one Provider runtime"
    );
    let hosted_providers: Vec<ProviderId> = runtimes
        .iter()
        .map(|runtime| runtime.provider_id())
        .collect();
    {
        let mut seen = std::collections::HashSet::new();
        for provider in &hosted_providers {
            anyhow::ensure!(
                seen.insert(provider),
                "server hosts Provider `{provider}` more than once"
            );
        }
    }
    config.create_private_runtime_dir()?;

    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(config.lock_path())
        .context("open server election lock")?;
    lock.try_lock_exclusive()
        .context("another server already owns this channel")?;
    protect_current_user_file(&config.lock_path())?;

    let config_documents = ConfigDocuments::new(config.config_dir());
    let (settings, _) = watch::channel(SettingsSnapshot::default());
    // Before any Session starts, so the first Turn already runs under the
    // Server Settings the Config Documents pinned.
    adopt_settings(&settings, &runtimes, &config_documents.load());

    let repository = StorageRepository::open(config.data_dir())
        .await
        .context("initialize Session repository")?;
    let persisted_sessions = repository
        .load_sessions()
        .await
        .context("load persisted Sessions")?;
    let persisted_landing_agent_selection = repository
        .landing_agent_selection()
        .await
        .context("load persisted landing Agent Selection")?;

    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .context("bind loopback server")?;
    let address = listener.local_addr().context("read server address")?;
    let descriptor = RuntimeDescriptor::new(
        format!("http://{address}"),
        format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
        ServerIdentity {
            instance_id: Uuid::new_v4(),
            pid: std::process::id(),
            protocol_version: PROTOCOL_VERSION,
            build_identity: build_identity::for_current_executable()?,
        },
    );
    write_descriptor(&config.descriptor_path(), &descriptor)?;

    let (lifecycle, _) = watch::channel(LifecycleState::Starting);
    let (shutdown_intent, _) = watch::channel(None);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (provider_shutdown, provider_shutdown_rx) = watch::channel(false);
    let provider_updates = ProviderUpdateGate::new();
    let shutdown = ShutdownController {
        lifecycle: lifecycle.clone(),
        shutdown_intent: shutdown_intent.clone(),
        shutdown: Arc::new(Mutex::new(Some(shutdown_tx))),
        provider_shutdown,
        provider_updates: provider_updates.clone(),
        shutdown_grace: timings.shutdown_grace,
    };
    let (storage_writer, storage) = StorageWriter::spawn(repository, &persisted_sessions.readable);
    let sessions = SessionStore::new(persisted_sessions, storage.clone());
    let landing_agent_selection =
        LandingAgentSelectionStore::new(persisted_landing_agent_selection, storage);
    let model_catalog = ModelCatalogService::new(runtimes.iter().cloned(), settings.subscribe());
    let providers = ProviderOrchestrator::new(
        runtimes.clone(),
        sessions.clone(),
        provider_shutdown_rx.clone(),
        provider_updates,
        settings.subscribe(),
    );
    let runtimes = Arc::new(runtimes);
    // Errands are abandoned on the same signal that stops Provider work, so a
    // shutting-down server never waits on one and never resumes one.
    let title_derivation = TitleDerivation::new(
        ErrandRunner::new(runtimes.clone(), provider_shutdown_rx, settings.subscribe())
            .with_timeout(timings.errand_timeout),
        model_catalog.clone(),
        sessions.clone(),
        settings.subscribe(),
    );
    let state = AppState {
        descriptor: Arc::new(descriptor.clone()),
        sessions: sessions.clone(),
        providers: providers.clone(),
        title_derivation,
        model_catalog,
        landing_agent_selection,
        settings: Arc::new(settings),
        config_documents,
        runtimes,
        hosted_providers: Arc::new(hosted_providers),
        shutdown: shutdown.clone(),
        timings,
    };
    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/events", get(events))
        .route("/v1/session-events", get(session_catalog_events))
        .route("/v1/settings", post(mutate_setting))
        .route("/v1/models", get(list_models))
        .route("/v1/models/refresh", post(refresh_models))
        .route(
            "/v1/landing-agent-selection",
            put(confirm_landing_agent_selection),
        )
        .route("/v1/sessions", get(list_sessions).post(create_session))
        .route(
            "/v1/sessions/{session_id}",
            get(read_session).delete(delete_session),
        )
        .route(
            "/v1/sessions/{session_id}/agent-selection",
            post(update_agent_selection),
        )
        .route("/v1/sessions/{session_id}/prompts", post(admit_prompt))
        .route(
            "/v1/sessions/{session_id}/prompts/{prompt_id}/promote",
            post(promote_prompt),
        )
        .route(
            "/v1/sessions/{session_id}/prompts/{prompt_id}/cancel",
            post(cancel_prompt),
        )
        .route(
            "/v1/sessions/{session_id}/turns/{turn_id}/interrupt",
            post(interrupt_turn),
        )
        .route("/v1/sessions/{session_id}/events", get(session_events))
        .route("/v1/server/stop", post(stop_server))
        .with_state(state);
    let descriptor_path = config.descriptor_path();
    let instance_id = descriptor.identity.instance_id;
    let task_lifecycle = lifecycle.clone();
    let task_shutdown = shutdown.clone();
    let providers_for_shutdown = providers.clone();
    let mut provider_shutdown_requested = task_shutdown.provider_shutdown.subscribe();
    let provider_shutdown_task = tokio::spawn(async move {
        wait_for_shutdown(&mut provider_shutdown_requested).await;
        providers_for_shutdown.shutdown().await;
    });
    let task = tokio::spawn(async move {
        let _lock = lock;
        let result = axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .context("serve local HTTP API");
        task_shutdown.stop_providers();
        let provider_shutdown_result = provider_shutdown_task
            .await
            .context("Provider shutdown task panicked");
        let storage_shutdown_result = storage_writer
            .shutdown()
            .await
            .context("shut down storage writer");
        if let Err(error) = &result {
            tracing::error!("server task failed: {error:#}");
            task_lifecycle.send_replace(LifecycleState::Failed);
        }
        remove_own_descriptor(&descriptor_path, instance_id);
        result
            .and(provider_shutdown_result)
            .and(storage_shutdown_result)
    });
    lifecycle.send_if_modified(|state| {
        if *state == LifecycleState::Starting {
            *state = LifecycleState::Ready;
            true
        } else {
            false
        }
    });
    tracing::info!(
        %address,
        instance_id = %descriptor.identity.instance_id,
        pid = descriptor.identity.pid,
        channel = config.channel(),
        "Suru server ready"
    );

    Ok(RunningServer {
        descriptor,
        session_events: SessionEventSink {
            sessions,
            lifecycle: lifecycle.subscribe(),
        },
        shutdown,
        task,
    })
}

async fn list_models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(state.model_catalog.list().await).into_response()
}

async fn refresh_models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(state.model_catalog.refresh().await).into_response()
}

async fn events(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if state.shutdown.lifecycle() != LifecycleState::Ready {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }

    Sse::new(event_stream(
        state.shutdown.subscribe_to_intent(),
        state.settings.subscribe(),
        state.timings.sse_keepalive_interval,
    ))
    .into_response()
}

struct EventStreamState {
    shutdown: watch::Receiver<Option<ServerShutdown>>,
    settings: watch::Receiver<SettingsSnapshot>,
    keepalive: tokio::time::Interval,
    finished: bool,
}

fn event_stream(
    shutdown: watch::Receiver<Option<ServerShutdown>>,
    mut settings: watch::Receiver<SettingsSnapshot>,
    keepalive_interval: Duration,
) -> impl futures_util::Stream<Item = std::result::Result<Event, std::convert::Infallible>> {
    // Every connecting client receives the effective-settings snapshot before
    // any other protocol event; later replacements re-push through the watch.
    let snapshot = settings_snapshot_event(&settings.borrow_and_update());
    let first = stream::once(async move {
        Ok::<_, std::convert::Infallible>(Event::default().comment("connected"))
    })
    .chain(stream::once(async move {
        Ok::<_, std::convert::Infallible>(snapshot)
    }));
    let state = EventStreamState {
        shutdown,
        settings,
        keepalive: tokio::time::interval_at(
            Instant::now() + keepalive_interval,
            keepalive_interval,
        ),
        finished: false,
    };
    let updates = stream::unfold(state, |mut state| async move {
        if state.finished {
            return None;
        }
        tokio::select! {
            biased;
            changed = state.shutdown.changed() => {
                if changed.is_err() {
                    return None;
                }
                let shutdown = state.shutdown.borrow_and_update().clone()?;
                let event = Event::default()
                    .event(SERVER_SHUTDOWN_EVENT)
                    .json_data(shutdown)
                    .expect("server shutdown intents always serialize");
                state.finished = true;
                Some((Ok::<_, std::convert::Infallible>(event), state))
            }
            changed = state.settings.changed() => {
                if changed.is_err() {
                    return None;
                }
                let event = settings_snapshot_event(&state.settings.borrow_and_update());
                Some((Ok::<_, std::convert::Infallible>(event), state))
            }
            _ = state.keepalive.tick() => Some((
                Ok::<_, std::convert::Infallible>(
                    Event::default().comment("keep-alive"),
                ),
                state,
            )),
        }
    });

    first.chain(updates)
}

/// The one way a client changes a Setting. The server applies the typed
/// mutation to its Config Document, hands the Provider runtime the Server
/// Settings the edit leaves in force, and pushes the refreshed snapshot to
/// every attached client — the mutating one included, which also reads it
/// back as this command's answer.
async fn mutate_setting(State(state): State<AppState>, request: Request) -> Response {
    let mutation = match decode_session_command::<SettingMutation>(
        &state,
        request,
        "Setting mutation",
    )
    .await
    {
        Ok(mutation) => mutation,
        Err(response) => return response,
    };
    // The edit is filesystem work, and the CST handles it parses the document
    // into are not `Send`; both stay on a blocking thread, where the read,
    // the edit, and the write are one scope.
    let documents = state.config_documents.clone();
    let mutated = tokio::task::spawn_blocking(move || documents.mutate(&mutation))
        .await
        .expect("Config Document edit runs to completion");
    match mutated {
        Ok(snapshot) => {
            adopt_settings(&state.settings, &state.runtimes, &snapshot);
            Json(snapshot).into_response()
        }
        Err(error @ SettingsMutationError::NoConfigRoot) => session_error_response(
            StatusCode::CONFLICT,
            SessionErrorCode::ConfigRootUnavailable,
            error.to_string(),
        ),
        Err(error @ SettingsMutationError::NotEditable { .. }) => session_error_response(
            StatusCode::CONFLICT,
            SessionErrorCode::ConfigDocumentNotEditable,
            error.to_string(),
        ),
        Err(error @ SettingsMutationError::Io { .. }) => {
            tracing::error!("Setting mutation failed: {error}");
            session_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                SessionErrorCode::ConfigDocumentWriteFailed,
                error.to_string(),
            )
        }
    }
}

/// Puts a freshly loaded effective-settings view in force: its problems reach
/// the Log, every hosted Provider runtime takes the Server Settings it now
/// runs under, and every attached client receives the snapshot. Startup and an
/// accepted mutation adopt a view the same way, and a future Config Document
/// watcher will too.
fn adopt_settings(
    settings: &watch::Sender<SettingsSnapshot>,
    runtimes: &[Arc<dyn ProviderRuntime>],
    snapshot: &SettingsSnapshot,
) {
    crate::settings::log_diagnostics(&snapshot.diagnostics);
    for runtime in runtimes {
        runtime.apply_settings(&snapshot.settings);
    }
    settings.send_replace(snapshot.clone());
}

fn settings_snapshot_event(snapshot: &SettingsSnapshot) -> Event {
    Event::default()
        .event(SETTINGS_SNAPSHOT_EVENT)
        .json_data(snapshot)
        .expect("settings snapshots always serialize")
}

async fn session_catalog_events(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let shutdown = state.shutdown.subscribe_to_intent();
    if state.shutdown.lifecycle() != LifecycleState::Ready || shutdown.borrow().is_some() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    Sse::new(session_catalog_event_stream(
        state.sessions.subscribe_catalog(),
        shutdown,
        state.timings.sse_keepalive_interval,
    ))
    .into_response()
}

fn session_catalog_event_stream(
    feed: SessionCatalogFeed,
    shutdown: watch::Receiver<Option<ServerShutdown>>,
    keepalive_interval: Duration,
) -> impl futures_util::Stream<Item = std::result::Result<Event, std::convert::Infallible>> {
    let revision = feed.snapshot.revision;
    snapshot_first_event_stream(
        feed.snapshot,
        revision,
        feed.updates,
        shutdown,
        keepalive_interval,
        RevisionedEventProtocol {
            event_names: RevisionedEventNames {
                snapshot: SESSION_CATALOG_SNAPSHOT_EVENT,
                update: SESSION_CATALOG_UPDATED_EVENT,
            },
            update_revision: |update| update.revision,
        },
    )
}

async fn health(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    Json(
        state
            .descriptor
            .health(state.shutdown.lifecycle())
            .with_landing_agent_selection(state.landing_agent_selection.current()),
    )
    .into_response()
}

async fn confirm_landing_agent_selection(
    State(state): State<AppState>,
    request: Request,
) -> Response {
    let selection = match decode_session_command::<AgentSelection>(
        &state,
        request,
        "Landing Agent Selection update",
    )
    .await
    {
        Ok(selection) => selection,
        Err(response) => return response,
    };
    let selection = match normalize_agent_selection(
        &state,
        selection,
        landing_agent_selection_provider_conflict_response,
    ) {
        Ok(selection) => selection,
        Err(response) => return *response,
    };
    state.landing_agent_selection.confirm(selection.clone());
    Json(selection).into_response()
}

async fn create_session(State(state): State<AppState>, request: Request) -> Response {
    let mut request =
        match decode_session_command::<CreateSessionRequest>(&state, request, "Session creation")
            .await
        {
            Ok(request) => request,
            Err(response) => return response,
        };

    if let Some(selection) = request.agent_selection.take() {
        request.agent_selection = match normalize_agent_selection(
            &state,
            selection,
            landing_agent_selection_provider_conflict_response,
        ) {
            Ok(selection) => Some(selection),
            Err(response) => return *response,
        };
    } else {
        // A persisted Landing selection can predate this server's hosted set or
        // the user's own choice of Providers, so one naming a Provider this
        // server does not host — or one the user has since turned off — yields
        // to the built-in default rather than stranding them on it.
        request.agent_selection = state
            .landing_agent_selection
            .current()
            .filter(|selection| state.is_selectable_provider(&selection.provider))
            .or_else(|| state.model_catalog.default_selection());
    }

    match state.sessions.create(request) {
        Ok(StoreOutcome::Created(snapshot)) => {
            if let Some(selection) = snapshot.session.agent_selection.clone() {
                state.landing_agent_selection.confirm(selection);
            }
            state.providers.open_session(
                snapshot.session.id,
                snapshot.session.workspace.path.clone(),
                snapshot.prompts[0].id,
            );
            // After the Turn is scheduled and never in front of it: a Title is
            // cosmetic and the user's actual work does not wait on one. Only a
            // freshly created Session reaches here, which is what makes the
            // derivation once-per-Session — a retried creation answers with the
            // Session it already made and asks for nothing.
            state.title_derivation.derive(
                snapshot.session.id,
                snapshot.session.workspace.path.clone(),
                snapshot
                    .session
                    .agent_selection
                    .as_ref()
                    .map(|selection| selection.provider.clone()),
                &snapshot.prompts[0].text,
            );
            (StatusCode::CREATED, Json(snapshot)).into_response()
        }
        Ok(StoreOutcome::Existing(snapshot)) => (StatusCode::OK, Json(snapshot)).into_response(),
        Err(CreateSessionError::EmptyPrompt) => session_error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            SessionErrorCode::EmptyPrompt,
            "Prompt must contain non-whitespace text",
        ),
        Err(CreateSessionError::InvalidWorkspace) => session_error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            SessionErrorCode::InvalidWorkspace,
            "Workspace must be an existing local directory",
        ),
        Err(CreateSessionError::PromptConflict) => prompt_conflict_response(),
    }
}

async fn update_agent_selection(
    State(state): State<AppState>,
    AxumPath(session_id): AxumPath<SessionId>,
    request: Request,
) -> Response {
    let mut request = match decode_session_command::<UpdateAgentSelectionRequest>(
        &state,
        request,
        "Agent Selection update",
    )
    .await
    {
        Ok(request) => request,
        Err(response) => return response,
    };
    request.selection = match normalize_agent_selection(
        &state,
        request.selection,
        agent_selection_provider_conflict_response,
    ) {
        Ok(selection) => selection,
        Err(response) => return *response,
    };
    match state
        .sessions
        .apply_agent_selection_command(session_id, request)
    {
        Ok(mutation) => {
            if let Some(prompt_id) = mutation.retry_prompt_id {
                state
                    .providers
                    .schedule_prompt(session_id, prompt_id)
                    .expect("stored Sessions retain their Provider actor");
            }
            state
                .landing_agent_selection
                .confirm(mutation.selection.clone());
            Json(mutation.selection).into_response()
        }
        Err(AgentSelectionMutationError::SessionNotFound) => session_error_response(
            StatusCode::NOT_FOUND,
            SessionErrorCode::SessionNotFound,
            "Session does not exist on this server instance",
        ),
        Err(AgentSelectionMutationError::OperationConflict) => session_error_response(
            StatusCode::CONFLICT,
            SessionErrorCode::AgentSelectionOperationConflict,
            "Agent Selection operation identity was already used with different content",
        ),
        Err(AgentSelectionMutationError::ProviderConflict) => {
            agent_selection_provider_conflict_response()
        }
    }
}

fn agent_selection_provider_conflict_response() -> Response {
    session_error_response(
        StatusCode::CONFLICT,
        SessionErrorCode::AgentSelectionProviderConflict,
        "An existing Session cannot change Provider",
    )
}

fn landing_agent_selection_provider_conflict_response() -> Response {
    session_error_response(
        StatusCode::CONFLICT,
        SessionErrorCode::AgentSelectionProviderConflict,
        "Landing Agent Selection must use a Provider available on this server",
    )
}

fn normalize_agent_selection(
    state: &AppState,
    selection: AgentSelection,
    provider_conflict_response: fn() -> Response,
) -> std::result::Result<AgentSelection, Box<Response>> {
    if !state.hosted_providers.contains(&selection.provider) {
        return Err(Box::new(provider_conflict_response()));
    }
    state
        .model_catalog
        .normalize_selection(&selection)
        .map_err(|message| Box::new(invalid_agent_selection_response(message)))
}

fn invalid_agent_selection_response(message: String) -> Response {
    session_error_response(
        StatusCode::UNPROCESSABLE_ENTITY,
        SessionErrorCode::InvalidCommand,
        format!("Agent Selection is invalid: {message}"),
    )
}

async fn admit_prompt(
    State(state): State<AppState>,
    AxumPath(session_id): AxumPath<SessionId>,
    request: Request,
) -> Response {
    let request =
        match decode_session_command::<AdmitPromptRequest>(&state, request, "Prompt admission")
            .await
        {
            Ok(request) => request,
            Err(response) => return response,
        };

    match state.sessions.admit(session_id, request) {
        Ok(StoreOutcome::Created(admission)) => {
            match admission.disposition {
                PromptAdmissionDisposition::StartImmediately => state
                    .providers
                    .schedule_prompt(session_id, admission.prompt.id)
                    .expect("stored Sessions retain their Provider actor"),
                PromptAdmissionDisposition::SteerActive => state
                    .providers
                    .schedule_steer(session_id)
                    .expect("stored Sessions retain their Provider actor"),
                PromptAdmissionDisposition::RemainPending => {}
            }
            (StatusCode::CREATED, Json(admission.prompt)).into_response()
        }
        Ok(StoreOutcome::Existing(admission)) => {
            (StatusCode::OK, Json(admission.prompt)).into_response()
        }
        Err(AdmitPromptError::EmptyPrompt) => session_error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            SessionErrorCode::EmptyPrompt,
            "Prompt must contain non-whitespace text",
        ),
        Err(AdmitPromptError::SessionNotFound) => session_error_response(
            StatusCode::NOT_FOUND,
            SessionErrorCode::SessionNotFound,
            "Session does not exist on this server instance",
        ),
        Err(AdmitPromptError::PromptConflict) => prompt_conflict_response(),
    }
}

async fn promote_prompt(
    State(state): State<AppState>,
    AxumPath((session_id, prompt_id)): AxumPath<(SessionId, crate::protocol::PromptId)>,
    headers: HeaderMap,
) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    prompt_mutation_response(state.sessions.promote(session_id, prompt_id))
}

async fn cancel_prompt(
    State(state): State<AppState>,
    AxumPath((session_id, prompt_id)): AxumPath<(SessionId, crate::protocol::PromptId)>,
    headers: HeaderMap,
) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    prompt_mutation_response(state.sessions.cancel(session_id, prompt_id))
}

fn prompt_mutation_response(
    result: std::result::Result<crate::protocol::Prompt, PromptMutationError>,
) -> Response {
    match result {
        Ok(prompt) => Json(prompt).into_response(),
        Err(PromptMutationError::SessionNotFound) => session_error_response(
            StatusCode::NOT_FOUND,
            SessionErrorCode::SessionNotFound,
            "Session does not exist on this server instance",
        ),
        Err(PromptMutationError::PromptNotFound) => session_error_response(
            StatusCode::NOT_FOUND,
            SessionErrorCode::PromptNotFound,
            "Prompt does not exist in this Session",
        ),
        Err(PromptMutationError::PromptNotPending) => session_error_response(
            StatusCode::CONFLICT,
            SessionErrorCode::PromptNotPending,
            "Prompt is no longer pending",
        ),
    }
}

async fn interrupt_turn(
    State(state): State<AppState>,
    AxumPath((session_id, turn_id)): AxumPath<(SessionId, TurnId)>,
    headers: HeaderMap,
) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match state.providers.interrupt_turn(session_id, turn_id).await {
        Ok(turn) => Json(turn).into_response(),
        Err(InterruptTurnError::SessionNotFound) => session_error_response(
            StatusCode::NOT_FOUND,
            SessionErrorCode::SessionNotFound,
            "Session does not exist on this server instance",
        ),
        Err(InterruptTurnError::TurnNotFound) => session_error_response(
            StatusCode::NOT_FOUND,
            SessionErrorCode::TurnNotFound,
            "Turn does not exist in this Session",
        ),
        Err(InterruptTurnError::TurnNotActive) => session_error_response(
            StatusCode::CONFLICT,
            SessionErrorCode::TurnNotActive,
            "Turn is no longer active",
        ),
        Err(InterruptTurnError::ProviderFailure(message)) => session_error_response(
            StatusCode::BAD_GATEWAY,
            SessionErrorCode::TurnInterruptionFailed,
            message,
        ),
    }
}

async fn decode_session_command<T: DeserializeOwned>(
    state: &AppState,
    request: Request,
    command_name: &str,
) -> std::result::Result<T, Response> {
    if !is_authenticated(request.headers(), &state.descriptor.token) {
        return Err(StatusCode::UNAUTHORIZED.into_response());
    }
    let body = to_bytes(request.into_body(), 64 * 1024)
        .await
        .map_err(|_| {
            session_error_response(
                StatusCode::BAD_REQUEST,
                SessionErrorCode::InvalidCommand,
                format!("{command_name} command body is too large"),
            )
        })?;
    serde_json::from_slice(&body).map_err(|_| {
        session_error_response(
            StatusCode::BAD_REQUEST,
            SessionErrorCode::InvalidCommand,
            format!("{command_name} command is not valid JSON"),
        )
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListSessionsQuery {
    workspace: Option<PathBuf>,
}

async fn list_sessions(
    State(state): State<AppState>,
    Query(query): Query<ListSessionsQuery>,
    headers: HeaderMap,
) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match state.sessions.list(query.workspace.as_deref()) {
        Ok(summaries) => Json(summaries).into_response(),
        Err(ListSessionsError::InvalidWorkspace) => session_error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            SessionErrorCode::InvalidWorkspace,
            "Workspace filter must be an existing local directory",
        ),
    }
}

async fn read_session(
    State(state): State<AppState>,
    AxumPath(session_id): AxumPath<SessionId>,
    headers: HeaderMap,
) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match state.sessions.snapshot(session_id) {
        Some(snapshot) => Json(snapshot).into_response(),
        None => session_error_response(
            StatusCode::NOT_FOUND,
            SessionErrorCode::SessionNotFound,
            "Session does not exist on this server instance",
        ),
    }
}

async fn delete_session(
    State(state): State<AppState>,
    AxumPath(session_id): AxumPath<SessionId>,
    headers: HeaderMap,
) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    state.providers.close_session(session_id).await;
    match state.sessions.delete(session_id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(DeleteSessionError::SessionNotFound) => session_error_response(
            StatusCode::NOT_FOUND,
            SessionErrorCode::SessionNotFound,
            "Session does not exist on this server instance",
        ),
        Err(DeleteSessionError::Storage(_)) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn session_events(
    State(state): State<AppState>,
    AxumPath(session_id): AxumPath<SessionId>,
    headers: HeaderMap,
) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let shutdown = state.shutdown.subscribe_to_intent();
    let shutdown_requested = shutdown.borrow().is_some();
    if state.shutdown.lifecycle() != LifecycleState::Ready || shutdown_requested {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let Some(feed) = state.sessions.subscribe(session_id) else {
        return session_error_response(
            StatusCode::NOT_FOUND,
            SessionErrorCode::SessionNotFound,
            "Session does not exist on this server instance",
        );
    };

    Sse::new(session_event_stream(
        feed,
        shutdown,
        state.timings.sse_keepalive_interval,
    ))
    .into_response()
}

fn session_event_stream(
    feed: SessionFeed,
    shutdown: watch::Receiver<Option<ServerShutdown>>,
    keepalive_interval: Duration,
) -> impl futures_util::Stream<Item = std::result::Result<Event, std::convert::Infallible>> {
    let revision = feed.snapshot.revision;
    snapshot_first_event_stream(
        feed.snapshot,
        revision,
        feed.updates,
        shutdown,
        keepalive_interval,
        RevisionedEventProtocol {
            event_names: RevisionedEventNames {
                snapshot: SESSION_SNAPSHOT_EVENT,
                update: SESSION_UPDATED_EVENT,
            },
            update_revision: |update| update.revision,
        },
    )
}

#[derive(Clone, Copy)]
struct RevisionedEventNames {
    snapshot: &'static str,
    update: &'static str,
}

struct RevisionedEventProtocol<Update, Revision> {
    event_names: RevisionedEventNames,
    update_revision: fn(&Update) -> Revision,
}

trait StreamRevision: Copy {
    fn immediately_follows(self, previous: Self) -> bool;
    fn event_id(self) -> String;
}

impl StreamRevision for SessionRevision {
    fn immediately_follows(self, previous: Self) -> bool {
        SessionRevision::immediately_follows(self, previous)
    }

    fn event_id(self) -> String {
        self.0.to_string()
    }
}

impl StreamRevision for SessionCatalogRevision {
    fn immediately_follows(self, previous: Self) -> bool {
        SessionCatalogRevision::immediately_follows(self, previous)
    }

    fn event_id(self) -> String {
        self.0.to_string()
    }
}

fn snapshot_first_event_stream<Snapshot, Update, Revision>(
    snapshot: Snapshot,
    snapshot_revision: Revision,
    updates: broadcast::Receiver<Update>,
    shutdown: watch::Receiver<Option<ServerShutdown>>,
    keepalive_interval: Duration,
    protocol: RevisionedEventProtocol<Update, Revision>,
) -> impl futures_util::Stream<Item = std::result::Result<Event, std::convert::Infallible>>
where
    Snapshot: serde::Serialize,
    Update: Clone + serde::Serialize,
    Revision: StreamRevision,
{
    let event_names = protocol.event_names;
    let update_revision = protocol.update_revision;
    let snapshot_event = Event::default()
        .event(event_names.snapshot)
        .id(snapshot_revision.event_id())
        .json_data(snapshot)
        .expect("snapshot payloads always serialize");
    stream::once(async move { Ok::<_, std::convert::Infallible>(snapshot_event) }).chain(
        stream::unfold(
            (
                tokio::time::interval_at(Instant::now() + keepalive_interval, keepalive_interval),
                shutdown,
                updates,
                snapshot_revision,
            ),
            move |(mut keepalive, mut shutdown, mut updates, delivered_revision)| async move {
                if shutdown.borrow().is_some() {
                    return None;
                }
                tokio::select! {
                    biased;
                    changed = shutdown.changed() => {
                        let _ = changed;
                        None
                    }
                    received = updates.recv() => {
                        let update = match received {
                            Ok(update) => update,
                            Err(broadcast::error::RecvError::Closed | broadcast::error::RecvError::Lagged(_)) => return None,
                        };
                        let next_revision = update_revision(&update);
                        if !next_revision.immediately_follows(delivered_revision) {
                            return None;
                        }
                        let event = Event::default()
                            .event(event_names.update)
                            .id(next_revision.event_id())
                            .json_data(update)
                            .expect("update payloads always serialize");
                        Some((
                            Ok::<_, std::convert::Infallible>(event),
                            (keepalive, shutdown, updates, next_revision),
                        ))
                    }
                    _ = keepalive.tick() => Some((
                        Ok::<_, std::convert::Infallible>(
                            Event::default().comment("keep-alive"),
                        ),
                        (keepalive, shutdown, updates, delivered_revision),
                    )),
                }
            },
        ),
    )
}

fn prompt_conflict_response() -> Response {
    session_error_response(
        StatusCode::CONFLICT,
        SessionErrorCode::PromptConflict,
        "Prompt ID is already associated with different content or admission metadata",
    )
}

fn session_error_response(
    status: StatusCode,
    code: SessionErrorCode,
    message: impl Into<String>,
) -> Response {
    (
        status,
        Json(SessionError {
            code,
            message: message.into(),
        }),
    )
        .into_response()
}

async fn stop_server(State(state): State<AppState>, request: Request) -> StatusCode {
    if !is_authenticated(request.headers(), &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED;
    }
    let Ok(body) = to_bytes(request.into_body(), 16 * 1024).await else {
        return StatusCode::BAD_REQUEST;
    };
    let Ok(request) = serde_json::from_slice::<ServerShutdown>(&body) else {
        return StatusCode::BAD_REQUEST;
    };
    if request.instance_id != state.descriptor.identity.instance_id {
        return StatusCode::CONFLICT;
    }

    state.shutdown.request(request);
    StatusCode::ACCEPTED
}

fn is_authenticated(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == format!("Bearer {token}"))
}

fn write_descriptor(path: &Path, descriptor: &RuntimeDescriptor) -> Result<()> {
    let runtime_dir = path
        .parent()
        .context("runtime descriptor has no directory")?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".runtime-")
        .suffix(".tmp")
        .tempfile_in(runtime_dir)
        .with_context(|| format!("create temporary runtime descriptor in {runtime_dir:?}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .context("protect temporary runtime descriptor")?;
    }
    serde_json::to_writer(temporary.as_file_mut(), descriptor)
        .context("encode runtime descriptor")?;
    temporary
        .as_file_mut()
        .write_all(b"\n")
        .context("finish runtime descriptor")?;
    temporary
        .as_file()
        .sync_all()
        .context("flush runtime descriptor")?;
    let published = temporary
        .persist(path)
        .map_err(|error| error.error)
        .context("publish runtime descriptor atomically")?;
    protect_current_user_file(path)?;
    published
        .sync_all()
        .context("flush published runtime descriptor")?;
    sync_runtime_directory(runtime_dir)?;
    Ok(())
}

#[cfg(unix)]
fn sync_runtime_directory(path: &Path) -> Result<()> {
    File::open(path)
        .with_context(|| format!("open runtime directory {path:?}"))?
        .sync_all()
        .with_context(|| format!("flush runtime directory {path:?}"))
}

#[cfg(not(unix))]
fn sync_runtime_directory(_path: &Path) -> Result<()> {
    Ok(())
}

fn remove_own_descriptor(path: &Path, instance_id: Uuid) {
    let initially_belongs_to_instance = File::open(path)
        .ok()
        .and_then(|file| serde_json::from_reader::<_, RuntimeDescriptor>(file).ok())
        .is_some_and(|descriptor| descriptor.identity.instance_id == instance_id);
    if !initially_belongs_to_instance {
        return;
    }

    // Move the exact path entry aside before deleting it. If another writer replaced the
    // descriptor after the first identity check, the quarantined file will fail the second check
    // and be restored without overwriting anything newer at the canonical path.
    let quarantine_path = path.with_extension(format!(
        "cleanup-{}-{}",
        instance_id,
        Uuid::new_v4().simple()
    ));
    if fs::rename(path, &quarantine_path).is_err() {
        return;
    }

    let quarantined_belongs_to_instance = File::open(&quarantine_path)
        .ok()
        .and_then(|file| serde_json::from_reader::<_, RuntimeDescriptor>(file).ok())
        .is_some_and(|descriptor| descriptor.identity.instance_id == instance_id);
    if quarantined_belongs_to_instance {
        let _ = fs::remove_file(&quarantine_path);
        return;
    }

    if fs::hard_link(&quarantine_path, path).is_ok() {
        let _ = fs::remove_file(&quarantine_path);
    }
}

#[cfg(test)]
mod tests {
    use futures_util::{StreamExt, pin_mut};

    use super::*;

    #[tokio::test]
    async fn lifecycle_streams_receive_shutdown_intent_independently() {
        let (shutdown, _) = watch::channel(None);
        let (settings, _) = watch::channel(SettingsSnapshot::default());
        let first = event_stream(
            shutdown.subscribe(),
            settings.subscribe(),
            Duration::from_secs(60),
        );
        let second = event_stream(
            shutdown.subscribe(),
            settings.subscribe(),
            Duration::from_secs(60),
        );
        pin_mut!(first);
        pin_mut!(second);

        assert!(
            first.next().await.is_some(),
            "first connected comment arrives"
        );
        assert!(
            first.next().await.is_some(),
            "first settings snapshot arrives"
        );
        assert!(
            second.next().await.is_some(),
            "second connected comment arrives"
        );
        assert!(
            second.next().await.is_some(),
            "second settings snapshot arrives"
        );

        let intent = ServerShutdown {
            instance_id: Uuid::new_v4(),
            reason: ShutdownReason::Manual,
        };
        shutdown.send_replace(Some(intent.clone()));
        let _ = first
            .next()
            .await
            .expect("first subscriber receives shutdown")
            .expect("lifecycle stream is infallible");
        let _ = second
            .next()
            .await
            .expect("second subscriber receives shutdown")
            .expect("lifecycle stream is infallible");
        assert!(first.next().await.is_none());
        assert!(second.next().await.is_none());
    }
}
