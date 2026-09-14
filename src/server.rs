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
use crate::model_catalog::{CatalogMemory, ModelCatalogService};
use crate::protocol::{
    Activity, AdmitPromptRequest, AgentSelection, CreateSessionRequest, InitialPrompt,
    InterruptOutcome, IssueInviteRequest, LifecycleState, MODEL_CATALOG_EVENT, Message, MessageId,
    MessageRole, MessageStatus, ModelCatalog, PROTOCOL_VERSION, Peer, ProviderId,
    RedeemInviteRequest, Remote, ResolveWorkspaceRequest, RuntimeDescriptor, SERVER_SHUTDOWN_EVENT,
    SESSION_CATALOG_SNAPSHOT_EVENT, SESSION_CATALOG_UPDATED_EVENT, SESSION_SNAPSHOT_EVENT,
    SESSION_UPDATED_EVENT, SETTINGS_SNAPSHOT_EVENT, SKILL_CATALOG_UPDATED_EVENT, ServerIdentity,
    ServerShutdown, SessionCatalogRevision, SessionChange, SessionError, SessionErrorCode,
    SessionId, SessionRevision, SessionUpdate, SettingMutation, SettingsSnapshot,
    SettleSessionRequest, ShutdownReason, SkillCatalog, SkillCatalogRequest, SkillPromptDelivery,
    TurnId, UpdateAgentSelectionRequest, UpdateApprovalPostureRequest, ViewSessionRequest,
};
use crate::provider::{
    ProviderOrchestrator, ProviderRuntime, ProviderUpdateGate, built_in_runtimes, wait_for_shutdown,
};
use crate::runtime::protect_current_user_file;
use crate::serving::ServingController;
use crate::sessions::{
    AdmitPromptError, AgentSelectionMutationError, ApprovalPostureMutationError,
    CreateSessionError, DeleteSessionError, InterruptSessionError, PromptAdmissionDisposition,
    PromptMutationError, SessionCatalogFeed, SessionFeed, SessionStore, SettleSessionError,
    StoreOutcome, TitleDerivation,
};
use crate::settings::{ConfigDocuments, SettingsMutationError};
use crate::skill_catalog::{SkillCatalogError, SkillCatalogService};
use crate::storage::{StorageRepository, StorageSink, StorageWriter};

pub type ServerConfig = RuntimeConfig;

/// Wall-clock intervals the server schedules against; injectable so tests can
/// observe periodic behavior without waiting out production-scale delays.
#[derive(Clone, Copy, Debug)]
pub struct ServerTimings {
    pub sse_keepalive_interval: Duration,
    pub checkout_observation_interval: Duration,
    pub checkout_skill_timeout: Duration,
    /// How long an accepted shutdown keeps health and existing streams
    /// available so the final authenticated intent can reach clients before
    /// graceful transport closure.
    pub shutdown_grace: Duration,
    /// How long an Errand may take before Suru stops waiting on it.
    pub errand_timeout: Duration,
    /// How long a newly issued Invite remains redeemable.
    pub invite_ttl: Duration,
    /// Server-to-Server protocol version, injectable for compatibility tests.
    pub pairing_protocol_version: u32,
}

impl Default for ServerTimings {
    fn default() -> Self {
        Self {
            sse_keepalive_interval: Duration::from_secs(10),
            checkout_observation_interval: Duration::from_secs(1),
            checkout_skill_timeout: Duration::from_secs(30),
            shutdown_grace: Duration::from_millis(100),
            errand_timeout: DEFAULT_ERRAND_TIMEOUT,
            invite_ttl: Duration::from_secs(10 * 60),
            pairing_protocol_version: PROTOCOL_VERSION,
        }
    }
}

impl ServerTimings {
    pub fn with_checkout_skill_timeout(mut self, timeout: Duration) -> Self {
        self.checkout_skill_timeout = timeout;
        self
    }
    pub fn with_checkout_observation_interval(mut self, interval: Duration) -> Self {
        self.checkout_observation_interval = interval;
        self
    }

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
    pub async fn emit(&self, session_id: SessionId, output: AgentOutput) -> Result<SessionUpdate> {
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
        self.session_events.sessions.hydrate(session_id).await?;
        if *self.session_events.lifecycle.borrow() != LifecycleState::Ready {
            anyhow::bail!("server is not accepting Session updates");
        }
        self.session_events
            .sessions
            .publish_agent_output(session_id, change)
    }

    pub async fn continuation_boundary(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> Result<Vec<crate::protocol::Prompt>> {
        if *self.session_events.lifecycle.borrow() != LifecycleState::Ready {
            anyhow::bail!("server is not accepting Session updates");
        }
        self.session_events.sessions.hydrate(session_id).await?;
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
    serving: ServingController,
    workspace_discovery: watch::Receiver<bool>,
    task: JoinHandle<Result<()>>,
}

#[derive(Clone)]
pub struct SessionEventSink {
    sessions: SessionStore,
    lifecycle: watch::Receiver<LifecycleState>,
}

impl SessionEventSink {
    pub async fn publish(
        &self,
        session_id: SessionId,
        changes: Vec<SessionChange>,
    ) -> Result<SessionUpdate> {
        if *self.lifecycle.borrow() != LifecycleState::Ready {
            anyhow::bail!("server is not accepting Session updates");
        }
        self.sessions.hydrate(session_id).await?;
        if *self.lifecycle.borrow() != LifecycleState::Ready {
            anyhow::bail!("server is not accepting Session updates");
        }
        self.sessions.publish(session_id, changes)
    }
}

impl RunningServer {
    /// Startup Workspace discovery runs behind readiness. Resolves once every
    /// persisted Session has been regrouped, or discovery was abandoned.
    pub async fn workspace_discovery_settled(&self) {
        let mut discovery = self.workspace_discovery.clone();
        let _ = discovery.wait_for(|settled| *settled).await;
    }
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

    /// The address currently owned by the opt-in Serving listener. `None`
    /// means Serving is disabled or its requested address could not be bound.
    pub fn serving_address(&self) -> Option<std::net::SocketAddr> {
        self.serving.address()
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
    preparations: crate::source_control::PreparationStore,
    source_control: crate::source_control::SourceControlService,
    workspace_paths: crate::protocol::WorkspacePaths,
    descriptor: Arc<RuntimeDescriptor>,
    sessions: SessionStore,
    providers: ProviderOrchestrator,
    /// Derives a Session's Title from its first Prompt, in the background and
    /// beside the first Turn rather than in front of it.
    title_derivation: TitleDerivation,
    model_catalog: ModelCatalogService,
    skill_catalog: SkillCatalogService,
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
    serving: ServingController,
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
    let runtimes = built_in_runtimes(config.data_dir());
    spawn_with_providers(config, runtimes).await
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
    let runtimes = built_in_runtimes(config.data_dir());
    spawn_with_providers_and_timings(config, runtimes, timings).await
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
    spawn_with_source_control(
        config,
        runtimes,
        timings,
        Arc::new(crate::source_control::GitSourceControl::default()),
    )
    .await
}

/// Installs source control through its own interface, independent of Agent Providers.
pub async fn spawn_with_source_control(
    config: ServerConfig,
    runtimes: Vec<Arc<dyn ProviderRuntime>>,
    timings: ServerTimings,
    source_control: Arc<dyn crate::source_control::SourceControl>,
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
    let opening_settings = config_documents.load();

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
    let remembered_model_catalog = repository
        .model_catalog()
        .await
        .context("load remembered Model Catalog")?;

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
    let serving = ServingController::new(
        config.data_dir(),
        timings.invite_ttl,
        timings.pairing_protocol_version,
        descriptor.base_url.clone(),
        descriptor.token.clone(),
    )?;
    write_descriptor(&config.descriptor_path(), &descriptor)?;

    // After all fallible local-server setup, so an error returning from spawn
    // can never leave a detached Serving listener behind; still before any
    // Session starts, so the first Turn runs under the pinned Server Settings.
    if let Err(error) = adopt_settings(&settings, &runtimes, &serving, &opening_settings).await {
        tracing::error!("could not adopt Serving settings at startup: {error:#}");
    }

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
    let (storage_writer, storage) = StorageWriter::spawn(repository, &[]);
    let preparations = crate::source_control::PreparationStore::new(config.data_dir());
    let sessions = SessionStore::new(
        persisted_sessions,
        storage.clone(),
        preparations.resumable_sessions(),
    );
    for update in sessions.reconcile_approval_postures(&opening_settings.settings) {
        sessions.mark_approval_posture_application(
            update,
            crate::protocol::ApprovalPostureApplication::Applied,
        );
    }
    let source_control = crate::source_control::SourceControlService::new(source_control);
    // Persisted grouping is served at once; discovery regroups Sessions behind
    // readiness and publishes catalog changes, so a cold source control
    // system never delays the server's readiness.
    let (workspace_discovery, workspace_discovery_rx) = watch::channel(false);
    {
        let sessions = sessions.clone();
        let source_control = source_control.clone();
        let mut shutdown = provider_shutdown_rx.clone();
        tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = wait_for_shutdown(&mut shutdown) => {
                    tracing::info!("Workspace discovery abandoned for shutdown");
                }
                result = sessions.discover_workspaces(&source_control) => {
                    if let Err(error) = result {
                        tracing::error!("Workspace discovery failed: {error:#}");
                    }
                }
            }
            workspace_discovery.send_replace(true);
        });
    }
    sessions.observe_checkouts(
        source_control.clone(),
        timings.checkout_observation_interval,
        provider_shutdown_rx.clone(),
    );
    let landing_agent_selection =
        LandingAgentSelectionStore::new(persisted_landing_agent_selection, storage.clone());
    let model_catalog = ModelCatalogService::new(
        runtimes.iter().cloned(),
        settings.subscribe(),
        CatalogMemory {
            remembered: remembered_model_catalog,
            remember: Some(Arc::new(move |remembered| {
                storage.save_model_catalog(remembered);
            })),
        },
    );
    let runtimes = Arc::new(runtimes);
    let skill_catalog = SkillCatalogService::new(runtimes.clone(), settings.subscribe());
    let providers = ProviderOrchestrator::new(
        runtimes.as_ref().clone(),
        sessions.clone(),
        provider_shutdown_rx.clone(),
        provider_updates,
        settings.subscribe(),
        skill_catalog.clone(),
        source_control.clone(),
        timings.checkout_skill_timeout,
    );
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
        preparations,
        source_control,
        workspace_paths: crate::protocol::WorkspacePaths::discover(),
        descriptor: Arc::new(descriptor.clone()),
        sessions: sessions.clone(),
        providers: providers.clone(),
        title_derivation,
        model_catalog,
        skill_catalog,
        landing_agent_selection,
        settings: Arc::new(settings),
        config_documents,
        runtimes,
        hosted_providers: Arc::new(hosted_providers),
        serving: serving.clone(),
        shutdown: shutdown.clone(),
        timings,
    };
    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/events", get(events))
        .route("/v1/session-events", get(session_catalog_events))
        .route("/v1/settings", post(mutate_setting))
        .route("/v1/pairing/invites", post(issue_invite))
        .route("/v1/pairing/invites/preview", post(preview_invite))
        .route("/v1/pairing/remotes", get(list_remotes).post(redeem_invite))
        .route("/v1/pairing/remotes/{name}/health", post(probe_remote))
        .route(
            "/v1/remotes/{name}/{*path}",
            axum::routing::any(proxy_remote),
        )
        .route("/v1/pairing/peers", get(list_peers))
        .route(
            "/v1/pairing/peers/{peer_id}",
            axum::routing::delete(remove_peer),
        )
        .route("/v1/models", get(list_models))
        .route("/v1/models/refresh", post(refresh_models))
        .route("/v1/models/warm", post(warm_models))
        .route("/v1/skills", post(list_skills))
        .route("/v1/skills/refresh", post(refresh_skills))
        .route(
            "/v1/landing-agent-selection",
            put(confirm_landing_agent_selection),
        )
        .route("/v1/workspaces/resolve", post(resolve_workspace))
        .route("/v1/checkouts/prepare", post(prepare_checkout))
        .route(
            "/v1/checkouts/removal-preview",
            post(preview_checkout_removal),
        )
        .route("/v1/checkouts/remove", post(remove_checkout))
        .route("/v1/sessions", get(list_sessions).post(create_session))
        .merge(
            Router::new()
                .route(
                    "/v1/sessions/{session_id}",
                    get(read_session).delete(delete_session),
                )
                .route(
                    "/v1/sessions/{session_id}/agent-selection",
                    post(update_agent_selection),
                )
                .route(
                    "/v1/sessions/{session_id}/approval-posture",
                    post(update_approval_posture),
                )
                .route("/v1/sessions/{session_id}/settlement", post(settle_session))
                .route("/v1/sessions/{session_id}/viewed", post(view_session))
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
                    "/v1/sessions/{session_id}/questionnaires/{id}",
                    post(submit_questionnaire),
                )
                .route(
                    "/v1/sessions/{session_id}/approvals/{id}/decision",
                    post(submit_decision),
                )
                .route(
                    "/v1/sessions/{session_id}/interrupt",
                    post(interrupt_session),
                )
                .route("/v1/sessions/{session_id}/events", get(session_events))
                .route_layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    hydrate_session_request,
                )),
        )
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
    let serving_for_shutdown = serving.clone();
    let task = tokio::spawn(async move {
        let _lock = lock;
        let result = axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .context("serve local HTTP API");
        serving_for_shutdown.shutdown().await;
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
        serving,
        workspace_discovery: workspace_discovery_rx,
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

/// What a client connecting says to a server that may so far serve only what
/// it remembered: ask each Provider this process has not yet heard from. The
/// answer arrives on the events stream as each discovery settles, so this
/// returns as soon as the asking has begun.
async fn warm_models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    state.model_catalog.warm();
    StatusCode::ACCEPTED.into_response()
}

async fn list_skills(State(state): State<AppState>, request: Request) -> Response {
    let request = match decode_session_command::<SkillCatalogRequest>(
        &state,
        request,
        "Skill Catalog listing",
    )
    .await
    {
        Ok(request) => request,
        Err(response) => return response,
    };
    match state.skill_catalog.list(request).await {
        Ok(catalog) => Json(catalog).into_response(),
        Err(error) => skill_catalog_error_response(error),
    }
}

async fn refresh_skills(State(state): State<AppState>, request: Request) -> Response {
    let request = match decode_session_command::<SkillCatalogRequest>(
        &state,
        request,
        "Skill Catalog refresh",
    )
    .await
    {
        Ok(request) => request,
        Err(response) => return response,
    };
    match state.skill_catalog.refresh(request).await {
        Ok(catalog) => Json(catalog).into_response(),
        Err(error) => skill_catalog_error_response(error),
    }
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
        state.model_catalog.clone(),
        state.timings.sse_keepalive_interval,
    ))
    .into_response()
}

struct EventStreamState {
    shutdown: watch::Receiver<Option<ServerShutdown>>,
    settings: watch::Receiver<SettingsSnapshot>,
    model_catalog: ModelCatalogService,
    catalog_changes: watch::Receiver<u64>,
    /// The catalog this stream last pushed, so a change that leaves the
    /// catalog as the client already has it is not pushed again.
    pushed_catalog: ModelCatalog,
    keepalive: tokio::time::Interval,
    finished: bool,
}

fn event_stream(
    shutdown: watch::Receiver<Option<ServerShutdown>>,
    mut settings: watch::Receiver<SettingsSnapshot>,
    model_catalog: ModelCatalogService,
    keepalive_interval: Duration,
) -> impl futures_util::Stream<Item = std::result::Result<Event, std::convert::Infallible>> {
    // Every connecting client receives the effective-settings snapshot before
    // any other protocol event; later replacements re-push through the watch.
    let snapshot = settings_snapshot_event(&settings.borrow_and_update());
    // The Model Catalog as the server holds it follows the Settings snapshot;
    // the change watch is read first so an answer landing between here and
    // the snapshot is pushed rather than lost.
    let mut catalog_changes = model_catalog.subscribe();
    catalog_changes.mark_unchanged();
    let pushed_catalog = model_catalog.current();
    let catalog = model_catalog_event(&pushed_catalog);
    let first = stream::once(async move {
        Ok::<_, std::convert::Infallible>(Event::default().comment("connected"))
    })
    .chain(stream::once(async move {
        Ok::<_, std::convert::Infallible>(snapshot)
    }))
    .chain(stream::once(async move {
        Ok::<_, std::convert::Infallible>(catalog)
    }));
    let state = EventStreamState {
        shutdown,
        settings,
        model_catalog,
        catalog_changes,
        pushed_catalog,
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
        loop {
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
                    return Some((Ok::<_, std::convert::Infallible>(event), state));
                }
                changed = state.settings.changed() => {
                    if changed.is_err() {
                        return None;
                    }
                    let event = settings_snapshot_event(&state.settings.borrow_and_update());
                    return Some((Ok::<_, std::convert::Infallible>(event), state));
                }
                changed = state.catalog_changes.changed() => {
                    if changed.is_err() {
                        return None;
                    }
                    // A change that leaves the catalog as this client already
                    // has it — a discovery settling on the very Models it was
                    // shown mid-refresh — is not worth a push.
                    let catalog = state.model_catalog.current();
                    if catalog == state.pushed_catalog {
                        continue;
                    }
                    let event = model_catalog_event(&catalog);
                    state.pushed_catalog = catalog;
                    return Some((Ok::<_, std::convert::Infallible>(event), state));
                }
                _ = state.keepalive.tick() => return Some((
                    Ok::<_, std::convert::Infallible>(
                        Event::default().comment("keep-alive"),
                    ),
                    state,
                )),
            }
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
            match adopt_settings(&state.settings, &state.runtimes, &state.serving, &snapshot).await
            {
                Ok(()) => {
                    let changed = state
                        .sessions
                        .reconcile_approval_postures(&snapshot.settings);
                    apply_live_posture_updates(&state, changed).await;
                    Json(snapshot).into_response()
                }
                Err(error) => {
                    tracing::error!("could not adopt Serving settings: {error:#}");
                    session_error_response(
                        StatusCode::CONFLICT,
                        SessionErrorCode::ServingListenerFailed,
                        error.to_string(),
                    )
                }
            }
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

async fn issue_invite(State(state): State<AppState>, request: Request) -> Response {
    let request = match decode_session_command::<IssueInviteRequest>(
        &state,
        request,
        "Invite issuance",
    )
    .await
    {
        Ok(request) => request,
        Err(response) => return response,
    };
    match state.serving.issue_invite(request).await {
        Ok(invite) => Json(invite).into_response(),
        Err(error) => pairing_error_response(error),
    }
}

async fn preview_invite(State(state): State<AppState>, request: Request) -> Response {
    let request = match decode_session_command::<crate::protocol::PreviewInviteRequest>(
        &state,
        request,
        "Invite preview",
    )
    .await
    {
        Ok(request) => request,
        Err(response) => return response,
    };
    match state.serving.preview_invite(&request.invite) {
        Ok(preview) => Json(preview).into_response(),
        Err(error) => pairing_error_response(error),
    }
}

async fn redeem_invite(State(state): State<AppState>, request: Request) -> Response {
    let request =
        match decode_session_command::<RedeemInviteRequest>(&state, request, "Invite redemption")
            .await
        {
            Ok(request) => request,
            Err(response) => return response,
        };
    match state.serving.redeem_invite(request).await {
        Ok(remote) => Json(remote).into_response(),
        Err(error) => pairing_error_response(error),
    }
}

async fn list_peers(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json::<Vec<Peer>>(state.serving.list_peers()).into_response()
}

async fn list_remotes(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json::<Vec<Remote>>(state.serving.list_remotes()).into_response()
}

async fn probe_remote(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(name): AxumPath<String>,
) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match state.serving.probe_remote(&name).await {
        Ok(health) => Json(health).into_response(),
        Err(error) => pairing_error_response(error),
    }
}

async fn proxy_remote(
    State(state): State<AppState>,
    AxumPath((name, _path)): AxumPath<(String, String)>,
    mut request: Request,
) -> Response {
    if !is_authenticated(request.headers(), &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(path_and_query) = request
        .uri()
        .path_and_query()
        .map(axum::http::uri::PathAndQuery::as_str)
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some((_, remote_path_and_query)) = path_and_query
        .strip_prefix("/v1/remotes/")
        .and_then(|path| path.split_once('/'))
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Ok(uri) = format!("/{remote_path_and_query}").parse() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    *request.uri_mut() = uri;
    match state.serving.proxy_remote(&name, request).await {
        Ok(response) => response,
        Err(error) => pairing_error_response(error),
    }
}

async fn remove_peer(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(peer_id): AxumPath<String>,
) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match state.serving.remove_peer(&peer_id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => pairing_error_response(error),
    }
}

fn pairing_error_response(error: crate::serving::PairingFailure) -> Response {
    session_error_response(error.status(), error.code, error.message)
}

/// Puts a freshly loaded effective-settings view in force: its problems reach
/// the Log, every hosted Provider runtime takes the Server Settings it now
/// runs under, and every attached client receives the snapshot. Startup and an
/// accepted mutation adopt a view the same way, and a future Config Document
/// watcher will too.
async fn adopt_settings(
    settings: &watch::Sender<SettingsSnapshot>,
    runtimes: &[Arc<dyn ProviderRuntime>],
    serving: &ServingController,
    snapshot: &SettingsSnapshot,
) -> Result<()> {
    crate::settings::log_diagnostics(&snapshot.diagnostics);
    for runtime in runtimes {
        runtime.apply_settings(&snapshot.settings);
    }
    let serving_result = serving.adopt(snapshot.settings.serving).await;
    settings.send_replace(snapshot.clone());
    serving_result
}

fn model_catalog_event(catalog: &ModelCatalog) -> Event {
    Event::default()
        .event(MODEL_CATALOG_EVENT)
        .json_data(catalog)
        .expect("Model Catalogs always serialize")
}

fn settings_snapshot_event(snapshot: &SettingsSnapshot) -> Event {
    Event::default()
        .event(SETTINGS_SNAPSHOT_EVENT)
        .json_data(snapshot)
        .expect("settings snapshots always serialize")
}

#[derive(Default, Deserialize)]
struct SessionCatalogQuery {
    #[serde(default)]
    warm_models: bool,
}

async fn session_catalog_events(
    State(state): State<AppState>,
    Query(query): Query<SessionCatalogQuery>,
    headers: HeaderMap,
) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let shutdown = state.shutdown.subscribe_to_intent();
    if state.shutdown.lifecycle() != LifecycleState::Ready || shutdown.borrow().is_some() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let events = session_catalog_event_stream(
        state
            .sessions
            .subscribe_catalog(state.workspace_paths.clone()),
        state.skill_catalog.subscribe(),
        state.model_catalog.clone(),
        shutdown,
        state.timings.sse_keepalive_interval,
    );
    // Capture the remembered catalog before asking Providers. Warming through
    // the subscription keeps a Remote on one transport connection and never
    // waits for discovery before delivering the snapshot.
    if query.warm_models {
        state.model_catalog.warm();
    }
    Sse::new(events).into_response()
}

struct SessionCatalogEventStreamState {
    keepalive: tokio::time::Interval,
    shutdown: watch::Receiver<Option<ServerShutdown>>,
    sessions: broadcast::Receiver<crate::protocol::SessionCatalogUpdate>,
    skills: broadcast::Receiver<SkillCatalog>,
    delivered_revision: SessionCatalogRevision,
    models: ModelCatalogService,
    model_changes: watch::Receiver<u64>,
    pushed_models: ModelCatalog,
}

fn session_catalog_event_stream(
    feed: SessionCatalogFeed,
    skills: broadcast::Receiver<SkillCatalog>,
    models: ModelCatalogService,
    shutdown: watch::Receiver<Option<ServerShutdown>>,
    keepalive_interval: Duration,
) -> impl futures_util::Stream<Item = std::result::Result<Event, std::convert::Infallible>> {
    let revision = feed.snapshot.revision;
    let snapshot = Event::default()
        .event(SESSION_CATALOG_SNAPSHOT_EVENT)
        .id(revision.event_id())
        .json_data(feed.snapshot)
        .expect("Session catalog snapshots always serialize");
    // Subscribe before reading the snapshot so a discovery completing during
    // connection setup cannot be missed.
    let mut model_changes = models.subscribe();
    model_changes.mark_unchanged();
    let pushed_models = models.current();
    let model_snapshot = model_catalog_event(&pushed_models);
    let state = SessionCatalogEventStreamState {
        keepalive: tokio::time::interval_at(
            Instant::now() + keepalive_interval,
            keepalive_interval,
        ),
        shutdown,
        sessions: feed.updates,
        skills,
        delivered_revision: revision,
        models,
        model_changes,
        pushed_models,
    };
    let updates = stream::unfold(state, |mut state| async move {
        if state.shutdown.borrow().is_some() {
            return None;
        }
        loop {
            let event = tokio::select! {
                biased;
                changed = state.shutdown.changed() => {
                    let _ = changed;
                    return None;
                }
                received = state.sessions.recv() => {
                    let update = match received {
                        Ok(update) => update,
                        Err(broadcast::error::RecvError::Closed | broadcast::error::RecvError::Lagged(_)) => return None,
                    };
                    if !update.revision.immediately_follows(state.delivered_revision) {
                        return None;
                    }
                    state.delivered_revision = update.revision;
                    Event::default()
                        .event(SESSION_CATALOG_UPDATED_EVENT)
                        .id(update.revision.event_id())
                        .json_data(update)
                        .expect("Session catalog updates always serialize")
                }
                received = state.skills.recv() => {
                    match received {
                        Ok(catalog) => Event::default()
                            .event(SKILL_CATALOG_UPDATED_EVENT)
                            .json_data(catalog)
                            .expect("Skill Catalog updates always serialize"),
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            tracing::warn!(skipped, "client Skill Catalog event stream lagged");
                            Event::default().comment("skill-catalog-updates-lagged")
                        }
                        Err(broadcast::error::RecvError::Closed) => return None,
                    }
                }
                changed = state.model_changes.changed() => {
                    if changed.is_err() {
                        return None;
                    }
                    let catalog = state.models.current();
                    if catalog == state.pushed_models {
                        continue;
                    }
                    let event = model_catalog_event(&catalog);
                    state.pushed_models = catalog;
                    event
                }
                _ = state.keepalive.tick() => Event::default().comment("keep-alive"),
            };
            return Some((Ok::<_, std::convert::Infallible>(event), state));
        }
    });
    stream::once(async move { Ok(snapshot) })
        .chain(stream::once(async move { Ok(model_snapshot) }))
        .chain(updates)
}

async fn health(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    Json(
        state
            .descriptor
            .health(state.shutdown.lifecycle())
            .with_workspace_paths(state.workspace_paths.clone())
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

fn preparation_error(error: impl Into<String>) -> Response {
    session_error_response(
        StatusCode::UNPROCESSABLE_ENTITY,
        SessionErrorCode::InvalidWorkspace,
        error,
    )
}

async fn prepare_checkout(State(state): State<AppState>, request: Request) -> Response {
    use crate::protocol::{PrepareCheckoutRequest, PrepareCheckoutResult, SkillCatalogStatus};
    let request = match decode_session_command::<PrepareCheckoutRequest>(
        &state,
        request,
        "Worktree preparation",
    )
    .await
    {
        Ok(request) => request,
        Err(response) => return response,
    };
    if request.description.trim().is_empty() {
        return preparation_error("Prompt must contain non-whitespace text");
    }
    // A dropped handler leaves no response and no final record; the guard logs that so a
    // preparation that stalls silently can be traced to the request ending early.
    let mut progress = PreparationProgress::begin(request.id);
    // Stable ID allocation and persistence precede Git mutation. The repository
    // guard is also used by admission, and can cover recovery/removal operations.
    let _serial = state.preparations.serial.lock().await;
    let mut planned_guard = None;
    let mut preparation = match state.preparations.load(request.id) {
        Ok(Some(plan)) => {
            progress.stage("resuming retained preparation");
            plan
        }
        Ok(None) => match state.source_control.plan_checkout(&request).await {
            Ok((plan, guard)) => {
                planned_guard = Some(guard);
                if let Err(e) = state.preparations.save(&plan) {
                    return progress.reject(preparation_error(e));
                }
                progress.stage("planned");
                plan
            }
            Err(e) => return progress.reject(preparation_error(e)),
        },
        Err(e) => return progress.reject(preparation_error(e)),
    };
    tracing::info!(
        preparation = %preparation.id.0,
        provider = %request.provider,
        source = %preparation.source.path.display(),
        destination = %preparation.destination.path.display(),
        checkout_created = preparation.checkout_created,
        "Worktree preparation started"
    );
    let source = crate::paths::canonical(&request.source.path)
        .unwrap_or_else(|_| request.source.path.clone());
    if source != preparation.source.path && source != preparation.destination.path {
        return progress.reject(preparation_error(
            "Preparation identity belongs to another execution location",
        ));
    }
    match rejoin_preparation(&state, &mut preparation).await {
        Ok(Some(_)) => {
            progress.finish(&preparation, None);
            return Json(PrepareCheckoutResult {
                preparation,
                location: None,
                error: None,
            })
            .into_response();
        }
        Ok(None) => {}
        Err(error) => {
            progress.finish(&preparation, Some(&error));
            return Json(PrepareCheckoutResult {
                preparation,
                location: None,
                error: Some(error),
            })
            .into_response();
        }
    }
    let _mutation = match planned_guard {
        Some(guard) => guard,
        None => {
            state
                .source_control
                .mutation_guard(&preparation.repository.id)
                .await
        }
    };
    progress.stage("repository mutation guard acquired");
    let mut location = None;
    let operation = async {
        state
            .source_control
            .checkpoint(
                crate::source_control::PreparationCheckpoint::IntentPersisted,
                &preparation,
            )
            .await?;
        let resolved = state.source_control.prepare_checkout(&preparation).await?;
        location = Some(resolved);
        preparation.checkout_created = true;
        state.preparations.save(&preparation)?;
        progress.stage("checkout created");
        progress.stage("checkout ready; refreshing destination Skills");
        let catalog = tokio::time::timeout(
            state.timings.checkout_skill_timeout,
            state.skill_catalog.refresh_current(SkillCatalogRequest {
                provider: request.provider,
                execution_directory: preparation.destination.clone(),
            }),
        )
        .await
        .map_err(|_| {
            "Destination Skill discovery timed out; Worktree retained for retry".to_owned()
        })?
        .map_err(|e| {
            format!("Destination Skills could not refresh; Worktree retained for retry: {e:?}")
        })?;
        match catalog.status {
            SkillCatalogStatus::Fresh { .. } => Ok(()),
            SkillCatalogStatus::Unavailable { message } | SkillCatalogStatus::Stale { message } => {
                Err(format!(
                    "Destination Skills could not refresh; Worktree retained for retry: {message}"
                ))
            }
            _ => Err("Destination Skills are still loading; retry".to_owned()),
        }
    }
    .await;
    let mut error = operation.err().map(|error| {
        format!(
            "Worktree preparation at {}: {error}",
            preparation.destination.path.display()
        )
    });
    preparation.ready = error.is_none();
    if let Err(e) = state.preparations.save(&preparation) {
        error = Some(e);
    }
    progress.finish(&preparation, error.as_deref());
    Json(PrepareCheckoutResult {
        preparation,
        location,
        error,
    })
    .into_response()
}

/// Traces one Worktree preparation request from arrival to its response.
///
/// Every stage logs with the preparation's identity and elapsed time. A handler that is dropped
/// before it answers, which happens when the Client's connection ends, logs the stage it was
/// in, because that outcome otherwise leaves neither a response nor a final record behind.
struct PreparationProgress {
    id: crate::protocol::PreparationId,
    started: std::time::Instant,
    stage: &'static str,
    finished: bool,
}

impl PreparationProgress {
    fn begin(id: crate::protocol::PreparationId) -> Self {
        Self {
            id,
            started: std::time::Instant::now(),
            stage: "received",
            finished: false,
        }
    }

    fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn stage(&mut self, stage: &'static str) {
        self.stage = stage;
        tracing::info!(
            preparation = %self.id.0,
            elapsed_ms = self.elapsed_ms(),
            "Worktree preparation: {stage}"
        );
    }

    /// Ends tracing for a request refused before it reached a preparation record.
    fn reject(mut self, response: Response) -> Response {
        self.finished = true;
        tracing::info!(
            preparation = %self.id.0,
            elapsed_ms = self.elapsed_ms(),
            "Worktree preparation rejected"
        );
        response
    }

    fn finish(&mut self, preparation: &crate::protocol::PreparedCheckout, error: Option<&str>) {
        self.finished = true;
        match error {
            None => tracing::info!(
                preparation = %self.id.0,
                elapsed_ms = self.elapsed_ms(),
                ready = preparation.ready,
                admitted = preparation.admitted_session.is_some(),
                "Worktree preparation responded"
            ),
            Some(error) => tracing::warn!(
                preparation = %self.id.0,
                elapsed_ms = self.elapsed_ms(),
                checkout_created = preparation.checkout_created,
                "Worktree preparation responded with an error: {error}"
            ),
        }
    }
}

impl Drop for PreparationProgress {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        tracing::warn!(
            preparation = %self.id.0,
            elapsed_ms = self.elapsed_ms(),
            stage = self.stage,
            "Worktree preparation handler ended before responding; the request was likely dropped"
        );
    }
}

/// An admitted initial Prompt is immutable even if a Client edits its retry.
/// Hydrate only this preparation's intended Session, never the catalog.
async fn rejoin_preparation(
    state: &AppState,
    plan: &mut crate::protocol::PreparedCheckout,
) -> Result<Option<crate::protocol::SessionSnapshot>, String> {
    state
        .sessions
        .hydrate(plan.intended_session)
        .await
        .map_err(|e| e.to_string())?;
    let Some(snapshot) = state.sessions.snapshot(plan.intended_session) else {
        return if plan.admitted_session.is_some() {
            Err("The admitted Session no longer exists".into())
        } else {
            Ok(None)
        };
    };
    if snapshot.session.execution_directory != plan.destination {
        return Err("Preparation Session has a conflicting execution location".into());
    }
    let pending = snapshot
        .turns
        .is_empty()
        .then(|| {
            snapshot
                .prompts
                .iter()
                .find(|prompt| prompt.status == crate::protocol::PromptStatus::Pending)
        })
        .flatten();
    if let Some(prompt) = pending
        && !state.providers.has_session_actor(snapshot.session.id)
    {
        let guard = state
            .source_control
            .mutation_guard(&plan.repository.id)
            .await;
        state.source_control.prepare_checkout(plan).await?;
        let provider = snapshot
            .session
            .agent_selection
            .as_ref()
            .map(|s| s.provider.clone())
            .or_else(|| state.hosted_providers.first().cloned());
        if let Some(provider) = &provider {
            let catalog = tokio::time::timeout(
                state.timings.checkout_skill_timeout,
                state.skill_catalog.refresh_current(SkillCatalogRequest {
                    provider: provider.clone(),
                    execution_directory: plan.destination.clone(),
                }),
            )
            .await
            .map_err(|_| "Destination Skill discovery timed out; retry".to_owned())?
            .map_err(|error| format!("Destination Skills are unavailable; retry: {error:?}"))?;
            if !matches!(
                catalog.status,
                crate::protocol::SkillCatalogStatus::Fresh { .. }
            ) {
                return Err(
                    "Destination Skills are unavailable; Worktree retained for retry".into(),
                );
            }
        }
        let initial = crate::protocol::InitialPrompt {
            id: prompt.id,
            text: prompt.text.clone(),
            skill_invocations: prompt.skill_invocations.clone(),
        };
        if !initial.skill_invocations.is_empty() {
            let provider =
                provider.ok_or("No Provider is selected for the admitted Skill Invocation")?;
            // This Prompt is already known, but it has never started. Admission's
            // idempotency bypass must not skip destination validation here.
            state.skill_catalog.validate_prompt(provider, &plan.destination.path, &initial, SkillPromptDelivery::Initial)
                .await.map_err(|error| format!("The admitted Prompt's destination Skills must be available before startup: {error:?}"))?;
        }
        state
            .sessions
            .persist_prepared_session(snapshot.session.id)
            .map_err(|e| e.to_string())?;
        state
            .providers
            .hold_checkout_guard(snapshot.session.id, prompt.id, guard);
        state.providers.open_session(
            snapshot.session.id,
            plan.destination.path.clone(),
            prompt.id,
        );
    }
    plan.admitted_session = Some(snapshot.session.id);
    state.preparations.delete_after_admission(plan)?;
    Ok(Some(snapshot))
}

async fn removal_preview(
    state: &AppState,
    target: crate::protocol::CheckoutRemovalTarget,
) -> Result<crate::protocol::CheckoutRemovalPreview, String> {
    let inspection = state.source_control.inspect_removal(&target).await?;
    let (affected_sessions, working_sessions) =
        state.sessions.checkout_references(&target.checkout.id);
    Ok(crate::protocol::CheckoutRemovalPreview {
        target,
        inspection,
        affected_sessions,
        working_sessions,
    })
}
async fn preview_checkout_removal(State(state): State<AppState>, request: Request) -> Response {
    let target = match decode_session_command::<crate::protocol::CheckoutRemovalTarget>(
        &state,
        request,
        "Worktree removal preview",
    )
    .await
    {
        Ok(target) => target,
        Err(response) => return response,
    };
    let _guard = state
        .source_control
        .mutation_guard(&target.repository.id)
        .await;
    match removal_preview(&state, target).await {
        Ok(preview) => Json(preview).into_response(),
        Err(e) => preparation_error(e),
    }
}
async fn remove_checkout(State(state): State<AppState>, request: Request) -> Response {
    let request = match decode_session_command::<crate::protocol::RemoveCheckoutRequest>(
        &state,
        request,
        "Worktree removal",
    )
    .await
    {
        Ok(request) => request,
        Err(response) => return response,
    };
    // Preparation and admission take these locks in this order. Removal joins
    // that order so no intent can be recreated while its Worktree disappears.
    let _preparation_serial = state.preparations.serial.lock().await;
    let _guard = state
        .source_control
        .mutation_guard(&request.preview.target.repository.id)
        .await;
    let preview = match removal_preview(&state, request.preview.target.clone()).await {
        Ok(preview) => preview,
        Err(e) => return preparation_error(e),
    };
    let result = async {
        if preview.working_sessions != 0 { return Err("A Session associated with this Worktree is Working on this Server; force cannot override it".to_owned()); }
        if preview != request.preview { return Err("Worktree conditions or affected Sessions changed; review the updated preview and confirm again".to_owned()); }
        if preview.inspection.requires_force() && !request.force { return Err("Git requires explicit force removal for these conditions; review and choose Force remove".to_owned()); }
        state.sessions.record_checkout(preview.inspection.checkout.clone()).map_err(|e| format!("Cannot persist recovery facts before removal: {e}"))?;
        if state.sessions.checkout_references(&preview.target.checkout.id).1 != 0 {
            return Err("A Session became Working; Worktree removal is blocked".into());
        }
        state.source_control.remove_checkout(&preview.target, &preview.inspection, request.force).await?;
        if let Err(error) = state
            .preparations
            .delete_for_destination(&preview.target.checkout.root)
        {
            tracing::warn!(
                checkout = %preview.target.checkout.root.display(),
                "Removed Worktree preparation intent could not be deleted: {error}"
            );
        }
        state.sessions.record_checkout(crate::protocol::CheckoutSummary { association: preview.target.checkout.clone(), revision: None, availability: crate::protocol::SourceControlAvailability::Unavailable { reason: "Worktree was explicitly removed; prompt a retained Session to recover it".into() } }).map_err(|e| e.to_string())?;
        Ok::<_, String>(())
    }.await;
    Json(crate::protocol::RemoveCheckoutResult {
        preview,
        removed: result.is_ok(),
        error: result.err(),
    })
    .into_response()
}

async fn create_session(State(state): State<AppState>, request: Request) -> Response {
    let mut request =
        match decode_session_command::<CreateSessionRequest>(&state, request, "Session creation")
            .await
        {
            Ok(request) => request,
            Err(response) => return response,
        };

    let _preparation_serial = if request.preparation_id.is_some() {
        Some(state.preparations.serial.lock().await)
    } else {
        None
    };
    if request.preparation_id.is_some()
        && let Err(error) = state.sessions.hydrate_prompt_owner(request.prompt.id).await
    {
        tracing::warn!("Prompt owner hydration failed: {error}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let mut missing_preparation = false;
    let mut preparation = match request.preparation_id {
        Some(id) => match state.preparations.load(id) {
            Ok(Some(plan)) => Some(plan),
            Ok(None) => {
                missing_preparation = true;
                None
            }
            Err(e) => return preparation_error(e),
        },
        None => None,
    };
    if let Some(plan) = &mut preparation {
        if request.execution_directory != plan.destination {
            return preparation_error("Preparation identity belongs to another execution location");
        }
        match rejoin_preparation(&state, plan).await {
            Ok(Some(snapshot)) => return Json(snapshot).into_response(),
            Ok(None) => {}
            Err(error) => return preparation_error(error),
        }
    }
    let mut mutation = if let Some(plan) = &preparation {
        Some(
            state
                .source_control
                .mutation_guard(&plan.repository.id)
                .await,
        )
    } else {
        None
    };
    if let Some(plan) = &preparation {
        if !plan.ready || request.execution_directory != plan.destination {
            return preparation_error("Prepare the intended Worktree before admitting this Prompt");
        }
        if let Err(e) = state.source_control.prepare_checkout(plan).await {
            return preparation_error(e);
        }
    }

    if let Err(error) = state.sessions.hydrate_prompt_owner(request.prompt.id).await {
        tracing::warn!("Prompt owner hydration failed: {error}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

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

    let mut location = state
        .source_control
        .resolve(&request.execution_directory.path, None)
        .await;
    if mutation.is_none()
        && !missing_preparation
        && let Some(repository) = &location.workspace.repository
    {
        mutation = Some(state.source_control.mutation_guard(&repository.id).await);
        let current = state
            .source_control
            .resolve(&request.execution_directory.path, Some(&location.workspace))
            .await;
        if current.checkout.as_ref().map(|c| &c.id) != location.checkout.as_ref().map(|c| &c.id)
            || current.execution_status != crate::protocol::ExecutionDirectoryStatus::Available
        {
            return preparation_error(
                "Execution location changed before admission; choose or restore the Worktree and retry",
            );
        }
        location = current;
    }
    if location.execution_directory.is_none() {
        return session_error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            SessionErrorCode::InvalidWorkspace,
            "Choose a working copy before starting a Session; repository metadata is not an Execution Directory",
        );
    }
    if let Err(error) = state
        .sessions
        .refresh_repository_labels(&state.source_control)
    {
        return session_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            SessionErrorCode::InvalidCommand,
            error.to_string(),
        );
    }
    let provider = request
        .agent_selection
        .as_ref()
        .map(|selection| selection.provider.clone())
        .or_else(|| state.hosted_providers.first().cloned());
    if request.preparation_id.is_some()
        && !request.prompt.skill_invocations.is_empty()
        && let Some(provider) = provider.clone()
    {
        request.prompt = match state
            .skill_catalog
            .rebind_prepared_prompt(provider, &request.execution_directory.path, &request.prompt)
            .await
        {
            Ok(prompt) => prompt,
            Err(error) => {
                return session_error_response(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    SessionErrorCode::InvalidSkillInvocation,
                    format!("Destination Skills must match before Prompt admission: {error:?}"),
                );
            }
        };
    }
    if let Err(response) = validate_new_prompt_skills(
        &state,
        provider,
        &request.execution_directory.path,
        &request.prompt,
        SkillPromptDelivery::Initial,
    )
    .await
    {
        return response;
    }

    if missing_preparation {
        return match state.sessions.existing_prepared_creation(&request) {
            Ok(Some(snapshot)) => Json(snapshot).into_response(),
            Ok(None) => {
                preparation_error("Worktree preparation is unknown; prepare it before admission")
            }
            Err(CreateSessionError::PromptConflict) => prompt_conflict_response(),
            Err(_) => unreachable!("existing creation only reports Prompt conflicts"),
        };
    }

    let admission = if let Some(plan) = &preparation {
        state
            .sessions
            .create_in_with_identity(request, location, Some(plan.intended_session))
    } else {
        state.sessions.create_in(request, location)
    };
    match admission {
        Ok(StoreOutcome::Created(mut snapshot)) => {
            state
                .sessions
                .reconcile_approval_postures(&state.settings.borrow().settings);
            snapshot = state
                .sessions
                .snapshot(snapshot.session.id)
                .unwrap_or(snapshot);
            if let Some(plan) = &mut preparation {
                if let Err(error) = state.sessions.persist_prepared_session(snapshot.session.id) {
                    return preparation_error(error.to_string());
                }
                if let Err(error) = state
                    .source_control
                    .checkpoint(
                        crate::source_control::PreparationCheckpoint::SessionPersisted,
                        plan,
                    )
                    .await
                {
                    return preparation_error(error);
                }
                plan.admitted_session = Some(snapshot.session.id);
                if let Err(e) = state.preparations.delete_after_admission(plan) {
                    tracing::warn!(
                        "Admitted Worktree preparation intent could not be deleted: {e}"
                    );
                }
            }

            if let Some(selection) = snapshot.session.agent_selection.clone() {
                state.landing_agent_selection.confirm(selection);
            }
            if let Some(guard) = mutation.take() {
                state.providers.hold_checkout_guard(
                    snapshot.session.id,
                    snapshot.prompts[0].id,
                    guard,
                );
            }
            state.providers.open_session(
                snapshot.session.id,
                snapshot.session.execution_directory.path.clone(),
                snapshot.prompts[0].id,
            );
            if let Some(plan) = &preparation {
                if let Err(error) = state
                    .source_control
                    .checkpoint(crate::source_control::PreparationCheckpoint::Admitted, plan)
                    .await
                {
                    return preparation_error(error);
                }
            }
            // After the Turn is scheduled and never in front of it: a Title is
            // cosmetic and the user's actual work does not wait on one. Only a
            // freshly created Session reaches here, which is what makes the
            // derivation once-per-Session — a retried creation answers with the
            // Session it already made and asks for nothing.
            state.title_derivation.derive(
                snapshot.session.id,
                snapshot.session.execution_directory.path.clone(),
                snapshot
                    .session
                    .agent_selection
                    .as_ref()
                    .map(|selection| selection.provider.clone()),
                &snapshot.prompts[0],
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

async fn update_approval_posture(
    State(state): State<AppState>,
    AxumPath(session_id): AxumPath<SessionId>,
    request: Request,
) -> Response {
    let request = match decode_session_command::<UpdateApprovalPostureRequest>(
        &state,
        request,
        "Approval Posture update",
    )
    .await
    {
        Ok(request) => request,
        Err(response) => return response,
    };
    let settings = state.settings.borrow().settings.clone();
    match state
        .sessions
        .apply_approval_posture_command(session_id, request, &settings)
    {
        Ok(mutation) => {
            let applied = match mutation.update {
                Some(update) => state.providers.update_approval_posture(update).await,
                None => Ok(crate::provider::ProviderPostureApplication::Applied),
            };
            match applied {
                Ok(_) => Json(
                    state
                        .sessions
                        .snapshot(session_id)
                        .and_then(|snapshot| snapshot.session.approval_posture)
                        .unwrap_or(mutation.posture),
                )
                .into_response(),
                Err(error) => session_error_response(
                    StatusCode::CONFLICT,
                    SessionErrorCode::InvalidCommand,
                    error,
                ),
            }
        }
        Err(ApprovalPostureMutationError::SessionNotFound) => session_error_response(
            StatusCode::NOT_FOUND,
            SessionErrorCode::SessionNotFound,
            "Session does not exist on this server instance",
        ),
        Err(ApprovalPostureMutationError::ProviderUnavailable) => session_error_response(
            StatusCode::CONFLICT,
            SessionErrorCode::InvalidCommand,
            "Session has no Provider Approval Posture",
        ),
        Err(ApprovalPostureMutationError::ProviderConflict) => session_error_response(
            StatusCode::CONFLICT,
            SessionErrorCode::InvalidCommand,
            "Approval Posture belongs to another Provider",
        ),
        Err(ApprovalPostureMutationError::InheritedByParent) => session_error_response(
            StatusCode::CONFLICT,
            SessionErrorCode::InvalidCommand,
            "Subagent Approval Posture is inherited from its parent Session",
        ),
        Err(ApprovalPostureMutationError::Storage) => {
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn apply_live_posture_updates(
    state: &AppState,
    changed: Vec<crate::sessions::ApprovalPostureUpdate>,
) {
    let updates = changed.into_iter().map(|update| async move {
        let session_id = update.session_id;
        if let Err(error) = state.providers.update_approval_posture(update).await {
            tracing::error!(session = %session_id, "could not apply live Approval Posture: {error}");
        }
    });
    futures_util::future::join_all(updates).await;
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
            state
                .sessions
                .reconcile_approval_postures(&state.settings.borrow().settings);
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

    if let Err(error) = state.sessions.hydrate_prompt_owner(request.prompt.id).await {
        tracing::warn!("Prompt owner hydration failed: {error}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let mut execution = None;
    if !state.sessions.knows_prompt(request.prompt.id)
        && let Some(snapshot) = state.sessions.snapshot(session_id)
    {
        if snapshot.session.checkout.is_some() {
            let lease = match state
                .source_control
                .prepare_execution(&snapshot.session, None)
                .await
            {
                Ok(lease) => lease,
                Err(e) => {
                    return preparation_error(format!(
                        "Worktree unavailable; retry after resolving recovery: {e}"
                    ));
                }
            };
            if let Some(reading) = lease.reading.clone()
                && let Err(error) = state.sessions.record_checkout(reading)
            {
                return preparation_error(format!(
                    "Cannot persist current checkout recovery facts: {error}"
                ));
            }
            if snapshot
                .session
                .checkout
                .as_ref()
                .is_some_and(|c| c.kind == crate::protocol::CheckoutKind::Linked)
            {
                let provider = snapshot
                    .session
                    .agent_selection
                    .as_ref()
                    .map(|s| s.provider.clone())
                    .or_else(|| state.hosted_providers.first().cloned());
                if let Some(provider) = provider {
                    match tokio::time::timeout(
                        state.timings.checkout_skill_timeout,
                        state.skill_catalog.refresh_current(SkillCatalogRequest {
                            provider,
                            execution_directory: snapshot.session.execution_directory.clone(),
                        }),
                    )
                    .await
                    {
                        Ok(Ok(catalog))
                            if matches!(
                                catalog.status,
                                crate::protocol::SkillCatalogStatus::Fresh { .. }
                            ) => {}
                        _ => {
                            return preparation_error(
                                "Destination Skills are unavailable; Worktree retained, retry after restoring the catalog",
                            );
                        }
                    }
                }
            }
            execution = Some(lease);
        }
    }

    if !request.prompt.skill_invocations.is_empty()
        && !state.sessions.knows_prompt(request.prompt.id)
    {
        let Some(snapshot) = state.sessions.snapshot(session_id) else {
            return session_error_response(
                StatusCode::NOT_FOUND,
                SessionErrorCode::SessionNotFound,
                "Session does not exist on this server instance",
            );
        };
        let provider = snapshot
            .session
            .agent_selection
            .as_ref()
            .map(|selection| selection.provider.clone())
            .or_else(|| state.hosted_providers.first().cloned());
        // The client's Enter always asks to steer, but an idle Session starts
        // the Prompt as a Turn of its own; judge the delivery it will get.
        let delivery = match crate::sessions::effective_delivery(&snapshot, request.delivery) {
            crate::protocol::PromptDelivery::Queue => SkillPromptDelivery::Queue,
            crate::protocol::PromptDelivery::Steer => SkillPromptDelivery::Steer,
        };
        if let Err(response) = validate_new_prompt_skills(
            &state,
            provider,
            &snapshot.session.execution_directory.path,
            &request.prompt,
            delivery,
        )
        .await
        {
            return response;
        }
    }

    // Recovery and catalog refresh await external work. Admission must use the
    // current Turn state, and the actor repeats this check at native steering.
    if let Some(lease) = &execution
        && let Some(current) = state.sessions.snapshot(session_id)
        && crate::sessions::effective_delivery(&current, request.delivery)
            == crate::protocol::PromptDelivery::Steer
        && current.session.working_since.is_some()
        && state
            .providers
            .connected_incarnation(session_id)
            .is_some_and(|incarnation| incarnation != lease.incarnation)
    {
        return preparation_error(
            "The Worktree was recreated while this Agent is still Working; wait for it to settle before retrying",
        );
    }

    match state.sessions.admit(session_id, request) {
        Ok(StoreOutcome::Created(admission)) => {
            match admission.disposition {
                PromptAdmissionDisposition::StartImmediately => {
                    if let Some(guard) = execution.as_mut().and_then(|lease| lease.guard.take()) {
                        state
                            .providers
                            .hold_checkout_guard(session_id, admission.prompt.id, guard);
                    }
                    state
                        .providers
                        .schedule_prompt(session_id, admission.prompt.id)
                        .expect("stored Sessions retain their Provider actor");
                }
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
        Err(AdmitPromptError::SubagentSession) => session_error_response(
            StatusCode::CONFLICT,
            SessionErrorCode::SubagentSession,
            "A Subagent's Session refuses Prompts",
        ),
        Err(AdmitPromptError::PromptConflict) => prompt_conflict_response(),
    }
}

async fn validate_new_prompt_skills(
    state: &AppState,
    provider: Option<ProviderId>,
    workspace: &Path,
    prompt: &InitialPrompt,
    delivery: SkillPromptDelivery,
) -> std::result::Result<(), Response> {
    if prompt.skill_invocations.is_empty() || state.sessions.knows_prompt(prompt.id) {
        return Ok(());
    }
    let provider = provider.ok_or_else(|| {
        skill_catalog_error_response(SkillCatalogError::InvalidInvocation(
            "No Provider is selected for this Skill Invocation".to_owned(),
        ))
    })?;
    state
        .skill_catalog
        .validate_prompt(provider, workspace, prompt, delivery)
        .await
        .map_err(skill_catalog_error_response)
}

fn skill_catalog_error_response(error: SkillCatalogError) -> Response {
    let (status, code, message) = match error {
        SkillCatalogError::InvalidWorkspace => (
            StatusCode::UNPROCESSABLE_ENTITY,
            SessionErrorCode::InvalidWorkspace,
            "Workspace must be an existing local directory".to_owned(),
        ),
        SkillCatalogError::ProviderNotHosted(provider) => (
            StatusCode::CONFLICT,
            SessionErrorCode::AgentSelectionProviderConflict,
            format!("Provider `{provider}` is not hosted by this server"),
        ),
        SkillCatalogError::InvalidCatalog(message) => (
            StatusCode::BAD_GATEWAY,
            SessionErrorCode::InvalidSkillInvocation,
            message,
        ),
        SkillCatalogError::InvalidInvocation(message) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            SessionErrorCode::InvalidSkillInvocation,
            message,
        ),
    };
    session_error_response(status, code, message)
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

async fn submit_questionnaire(
    State(state): State<AppState>,
    AxumPath((session_id, id)): AxumPath<(SessionId, crate::protocol::QuestionnaireId)>,
    headers: HeaderMap,
    Json(submission): Json<crate::protocol::QuestionnaireSubmission>,
) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match state
        .providers
        .submit_questionnaire(session_id, id, submission)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(message) => session_error_response(
            StatusCode::CONFLICT,
            SessionErrorCode::QuestionnaireSubmissionFailed,
            &message,
        ),
    }
}

async fn submit_decision(
    State(state): State<AppState>,
    AxumPath((session_id, id)): AxumPath<(SessionId, crate::protocol::ApprovalId)>,
    headers: HeaderMap,
    Json(decision): Json<crate::protocol::Decision>,
) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match state
        .providers
        .submit_decision(session_id, id, decision)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(message) => session_error_response(
            StatusCode::CONFLICT,
            SessionErrorCode::DecisionSubmissionFailed,
            &message,
        ),
    }
}

async fn interrupt_session(
    State(state): State<AppState>,
    AxumPath(session_id): AxumPath<SessionId>,
    headers: HeaderMap,
) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match state.providers.interrupt_session(session_id).await {
        // Stopping work says everything it has to say by succeeding; a
        // withdrawal has to name the Prompt it withdrew, so the client that
        // asked can put the text back in its own composer (ADR 0024).
        Ok(InterruptOutcome::StoppedWork) => StatusCode::NO_CONTENT.into_response(),
        Ok(withdrawn @ InterruptOutcome::WithdrewPrompt { .. }) => {
            (StatusCode::OK, Json(withdrawn)).into_response()
        }
        Err(InterruptSessionError::SessionNotFound) => session_error_response(
            StatusCode::NOT_FOUND,
            SessionErrorCode::SessionNotFound,
            "Session does not exist on this server instance",
        ),
        Err(InterruptSessionError::NothingToInterrupt) => session_error_response(
            StatusCode::CONFLICT,
            SessionErrorCode::NothingToInterrupt,
            "Session has no active Turn and no working Subagent",
        ),
        Err(InterruptSessionError::SubagentStopUnsupported) => session_error_response(
            StatusCode::CONFLICT,
            SessionErrorCode::SubagentStopUnsupported,
            "Provider offers no per-Subagent stop",
        ),
        Err(InterruptSessionError::ProviderFailure(message)) => session_error_response(
            StatusCode::BAD_GATEWAY,
            SessionErrorCode::InterruptionFailed,
            message,
        ),
        Err(InterruptSessionError::Storage(_)) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
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

async fn resolve_workspace(State(state): State<AppState>, request: Request) -> Response {
    let request = match decode_session_command::<ResolveWorkspaceRequest>(
        &state,
        request,
        "Workspace resolution",
    )
    .await
    {
        Ok(request) => request,
        Err(response) => return response,
    };
    let known = request
        .workspace_id
        .as_ref()
        .and_then(|id| state.sessions.known_workspace(id))
        .or_else(|| {
            request.workspace_id.as_ref().and_then(|id| {
                state
                    .source_control
                    .workspaces()
                    .into_iter()
                    .find(|workspace| &workspace.id == id)
            })
        });
    let base = match request.base.clone() {
        Some(base) => base,
        None => match std::env::current_dir() {
            Ok(base) => base,
            Err(_) => {
                return session_error_response(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    SessionErrorCode::InvalidWorkspace,
                    "Could not read the Server's current Workspace",
                );
            }
        },
    };
    let named = known.as_ref().map_or_else(
        || base.join(&request.path),
        |workspace| workspace.path.clone(),
    );
    let resolved = match state
        .source_control
        .resolve_selection(&named, known.as_ref(), &request)
        .await
    {
        Ok(resolved) => resolved,
        Err(reason) => {
            return session_error_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                SessionErrorCode::InvalidWorkspace,
                reason,
            );
        }
    };
    if let Err(error) = state
        .sessions
        .refresh_repository_labels(&state.source_control)
    {
        return session_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            SessionErrorCode::InvalidCommand,
            error.to_string(),
        );
    }
    Json(resolved).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListSessionsQuery {
    workspace_id: Option<String>,
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
    let workspace = match (query.workspace_id, query.workspace) {
        (Some(id), None) => Some(crate::protocol::WorkspaceId(id)),
        (None, Some(path)) => {
            let Ok(path) = crate::paths::canonical(path) else {
                return session_error_response(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    SessionErrorCode::InvalidWorkspace,
                    "Workspace filter must be an existing local directory",
                );
            };
            if !path.is_dir() {
                return session_error_response(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    SessionErrorCode::InvalidWorkspace,
                    "Workspace filter must be an existing local directory",
                );
            }
            Some(state.source_control.resolve(&path, None).await.workspace.id)
        }
        (None, None) => None,
        _ => {
            return session_error_response(
                StatusCode::BAD_REQUEST,
                SessionErrorCode::InvalidCommand,
                "Choose a Workspace identity or directory filter",
            );
        }
    };
    Json(state.sessions.list(workspace.as_ref())).into_response()
}

/// All Session-specific HTTP operations enter through one hydration boundary.
/// Delete needs identities only; it deliberately races safely with hydration.
async fn hydrate_session_request(
    State(state): State<AppState>,
    AxumPath(parameters): AxumPath<std::collections::HashMap<String, String>>,
    request: Request,
    next: axum::middleware::Next,
) -> Response {
    if !is_authenticated(request.headers(), &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if request.method() != axum::http::Method::DELETE {
        let session_id = parameters
            .get("session_id")
            .and_then(|id| Uuid::parse_str(id).ok())
            .map(SessionId::from_uuid);
        if let Some(id) = session_id
            && let Err(error) = state.sessions.hydrate(id).await
        {
            tracing::warn!("Session hydration failed: {error}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        let changed = state
            .sessions
            .reconcile_approval_postures(&state.settings.borrow().settings);
        for update in changed {
            if Some(update.session_id) == session_id
                && !state.providers.has_session_actor(update.session_id)
            {
                state.sessions.mark_approval_posture_application(
                    update,
                    crate::protocol::ApprovalPostureApplication::Applied,
                );
            }
        }
    }
    next.run(request).await
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
        Err(DeleteSessionError::SubagentSession) => session_error_response(
            StatusCode::CONFLICT,
            SessionErrorCode::SubagentSession,
            "A Subagent's Session is deleted with its parent",
        ),
        Err(DeleteSessionError::Storage(_)) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Sets a Session aside as done for now, or brings it back. The intent is the
/// request's to state rather than the server's to infer, so a client acting on
/// a stale listing cannot flip a Session it meant to leave alone.
async fn settle_session(
    State(state): State<AppState>,
    AxumPath(session_id): AxumPath<SessionId>,
    request: Request,
) -> Response {
    let settlement =
        match decode_session_command::<SettleSessionRequest>(&state, request, "Session settlement")
            .await
        {
            Ok(settlement) => settlement,
            Err(response) => return response,
        };
    match state.sessions.settle(session_id, settlement.settled) {
        Ok(summary) => Json(summary).into_response(),
        Err(SettleSessionError::SessionNotFound) => session_error_response(
            StatusCode::NOT_FOUND,
            SessionErrorCode::SessionNotFound,
            "Session does not exist on this server instance",
        ),
    }
}

/// Records that a Client has this root Session open in its main view.
async fn view_session(
    State(state): State<AppState>,
    AxumPath(session_id): AxumPath<SessionId>,
    request: Request,
) -> Response {
    let viewed =
        match decode_session_command::<ViewSessionRequest>(&state, request, "Session viewed").await
        {
            Ok(viewed) => viewed,
            Err(response) => return response,
        };
    match state.sessions.view(session_id, viewed) {
        Ok(summary) => Json(summary).into_response(),
        Err(crate::sessions::ViewSessionError::SessionNotFound) => session_error_response(
            StatusCode::NOT_FOUND,
            SessionErrorCode::SessionNotFound,
            "Session does not exist on this server instance",
        ),
        Err(crate::sessions::ViewSessionError::SubagentSession) => session_error_response(
            StatusCode::CONFLICT,
            SessionErrorCode::SubagentSession,
            "A Subagent's Session does not carry Viewed state",
        ),
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
    let published = persist_descriptor(temporary, path)?;
    protect_current_user_file(path)?;
    published
        .sync_all()
        .context("flush published runtime descriptor")?;
    sync_runtime_directory(runtime_dir)?;
    Ok(())
}

#[cfg(not(windows))]
fn persist_descriptor(temporary: tempfile::NamedTempFile, path: &Path) -> Result<File> {
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .context("publish runtime descriptor atomically")
}

#[cfg(windows)]
fn persist_descriptor(temporary: tempfile::NamedTempFile, path: &Path) -> Result<File> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_NORMAL, SetFileAttributesW};

    let temporary_path = temporary.path();
    let temporary_path_utf16 = temporary_path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // NamedTempFile marks the file temporary; a persistent descriptor must be flushed normally.
    // SAFETY: temporary_path_utf16 is NUL-terminated and remains alive for the call.
    if unsafe { SetFileAttributesW(temporary_path_utf16.as_ptr(), FILE_ATTRIBUTE_NORMAL) } == 0 {
        return Err(std::io::Error::last_os_error())
            .context("finalize temporary runtime descriptor");
    }
    fs::rename(temporary_path, path).context("publish runtime descriptor atomically")?;
    Ok(temporary.into_file())
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
        let model_catalog =
            ModelCatalogService::new([], settings.subscribe(), CatalogMemory::none());
        let first = event_stream(
            shutdown.subscribe(),
            settings.subscribe(),
            model_catalog.clone(),
            Duration::from_secs(60),
        );
        let second = event_stream(
            shutdown.subscribe(),
            settings.subscribe(),
            model_catalog,
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
        assert!(first.next().await.is_some(), "first Model Catalog arrives");
        assert!(
            second.next().await.is_some(),
            "second connected comment arrives"
        );
        assert!(
            second.next().await.is_some(),
            "second settings snapshot arrives"
        );
        assert!(
            second.next().await.is_some(),
            "second Model Catalog arrives"
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
