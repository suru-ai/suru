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
    http::{HeaderMap, HeaderValue, StatusCode, header::AUTHORIZATION},
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
use crate::broker::{self, BrokerAccess, BrokerTools};
use crate::build_identity;
use crate::errands::{DEFAULT_ERRAND_TIMEOUT, ErrandRunner};
use crate::model_catalog::{CatalogMemory, ModelCatalogService};
use crate::protocol::{
    Activity, AdmitPromptRequest, AgentSelection, CompactSessionRequest, CreateSessionRequest,
    InterruptOutcome, IssueInviteRequest, LifecycleState, MODEL_CATALOG_EVENT, Message, MessageId,
    MessageRole, MessageStatus, ModelCatalog, PROTOCOL_VERSION, Peer, ProviderId,
    RedeemInviteRequest, Remote, ResolveWorkspaceRequest, RuntimeDescriptor, SERVER_SHUTDOWN_EVENT,
    SESSION_CATALOG_SNAPSHOT_EVENT, SESSION_CATALOG_UPDATED_EVENT, SESSION_ERROR_CODE_HEADER,
    SESSION_SNAPSHOT_EVENT, SESSION_UPDATED_EVENT, SETTINGS_SNAPSHOT_EVENT,
    SKILL_CATALOG_UPDATED_EVENT, SUBAGENT_TREE_SNAPSHOT_EVENT, SUBAGENT_TREE_UPDATED_EVENT,
    ServerIdentity, ServerShutdown, SessionCatalogRevision, SessionChange, SessionError,
    SessionErrorCode, SessionId, SessionRevision, SessionUpdate, SetSessionIconRequest,
    SetWorkspaceIconRequest, SettingMutation, SettingsSnapshot, SettleSessionRequest,
    ShutdownReason, SkillCatalog, SkillCatalogRequest, SubagentTreeRevision, SubagentTreeUpdate,
    TurnId, UpdateAgentSelectionRequest, UpdateApprovalPostureRequest, ViewSessionRequest,
};
use crate::provider::{
    ContextBreakdownError, ProviderOrchestrator, ProviderRuntime, ProviderUpdateGate,
    built_in_runtimes, wait_for_shutdown,
};
use crate::runtime::protect_current_user_file;
use crate::serving::ServingController;
use crate::sessions::{
    AgentSelectionMutationError, ApprovalPostureMutationError, CompactSessionError,
    DeleteSessionError, Derivation, InterruptSessionError, PromptMutationError, SessionCatalogFeed,
    SessionFeed, SessionStore, SetIconError, SetWorkspaceIconError, StoreOutcome,
};
use crate::settings::{ConfigDocuments, SettingsMutationError};
use crate::skill_catalog::{SkillCatalogError, SkillCatalogService};
use crate::storage::{StorageRepository, StorageSink, StorageWriter};

mod attachments;
mod operations;
mod reclaim;

use operations::{
    AgentSelectionRefusal, AnswerRefusal, PromptRefusal, SessionOperations, SettleRefusal,
};

pub use crate::clock::{ManualClock, ServerClock};

pub type ServerConfig = RuntimeConfig;

/// Wall-clock intervals the server schedules against; injectable so tests can
/// observe periodic behavior without waiting out production-scale delays.
#[derive(Clone, Debug)]
pub struct ServerTimings {
    pub sse_keepalive_interval: Duration,
    pub checkout_observation_interval: Duration,
    /// Delay after startup Workspace discovery before the first Reclaim pass.
    pub worktree_reclaim_startup_delay: Duration,
    /// Cadence of later automatic Reclaim passes.
    pub worktree_reclaim_interval: Duration,
    /// How long one day of the Reclaim threshold lasts. A Session held
    /// Monitoring by a live Watch cannot be backdated past the threshold,
    /// since no Watch survives the restart that would read the backdated
    /// row, so tests shorten the day instead.
    pub worktree_reclaim_day: Duration,
    pub checkout_skill_timeout: Duration,
    /// How long an accepted shutdown keeps health and existing streams
    /// available so the final authenticated intent can reach clients before
    /// graceful transport closure.
    pub shutdown_grace: Duration,
    /// How long an Errand may take before Suru stops waiting on it.
    pub errand_timeout: Duration,
    /// How long a newly issued Invite remains redeemable.
    pub invite_ttl: Duration,
    /// How long removing a Remote waits for that Remote to acknowledge the
    /// withdrawal before forgetting it locally regardless.
    pub remote_withdrawal_timeout: Duration,
    /// Server-to-Server protocol version, injectable for compatibility tests.
    pub pairing_protocol_version: u32,
    /// How long a starting server waits for the channel's election lock to
    /// come free before conceding that another server owns the channel. See
    /// `take_election_lock` for why a stopped server's lock can outlive it.
    pub election_handoff: Duration,
    /// How long one of the seconds a Broker wait's `timeout_seconds` counts
    /// lasts, so a test observes a wait's bounds without waiting them out.
    pub broker_wait_second: Duration,
    /// How often a Broker wait still waiting reports progress, keeping the
    /// idle window of the harness that called it open (ADR 0034).
    pub broker_wait_progress_interval: Duration,
    /// How long after it was last uploaded or bound by an admitted Prompt an
    /// Attachment is left in place even once no Session references it, since
    /// a Prompt still in admission may be about to bind it (ADR 0037).
    pub attachment_grace: Duration,
    /// How long a Server that has had no Session work to flush waits between
    /// sweeps of orphaned Attachments, since an upload alone never wakes the
    /// storage writer.
    pub attachment_sweep_interval: Duration,
    /// Where the Server reads the time an Attachment's grace and the sweep
    /// interval are measured by, and the moment a Sidekick's listing of
    /// Sessions reads auto-settle against.
    pub clock: ServerClock,
}

impl Default for ServerTimings {
    fn default() -> Self {
        Self {
            sse_keepalive_interval: Duration::from_secs(10),
            checkout_observation_interval: Duration::from_secs(1),
            worktree_reclaim_startup_delay: Duration::from_secs(1),
            worktree_reclaim_interval: Duration::from_secs(60 * 60),
            worktree_reclaim_day: Duration::from_secs(24 * 60 * 60),
            checkout_skill_timeout: Duration::from_secs(30),
            shutdown_grace: Duration::from_millis(100),
            errand_timeout: DEFAULT_ERRAND_TIMEOUT,
            invite_ttl: Duration::from_secs(10 * 60),
            remote_withdrawal_timeout: Duration::from_secs(5),
            pairing_protocol_version: PROTOCOL_VERSION,
            election_handoff: Duration::from_secs(1),
            broker_wait_second: broker::WaitTimings::default().second,
            broker_wait_progress_interval: broker::WaitTimings::default().progress_every,
            attachment_grace: crate::attachments::ATTACHMENT_GRACE,
            attachment_sweep_interval: crate::attachments::ATTACHMENT_SWEEP_INTERVAL,
            clock: ServerClock::default(),
        }
    }
}

impl ServerTimings {
    pub fn with_remote_withdrawal_timeout(mut self, timeout: Duration) -> Self {
        self.remote_withdrawal_timeout = timeout;
        self
    }
    pub fn with_checkout_skill_timeout(mut self, timeout: Duration) -> Self {
        self.checkout_skill_timeout = timeout;
        self
    }
    pub fn with_checkout_observation_interval(mut self, interval: Duration) -> Self {
        self.checkout_observation_interval = interval;
        self
    }
    pub fn with_worktree_reclaim_startup_delay(mut self, delay: Duration) -> Self {
        self.worktree_reclaim_startup_delay = delay;
        self
    }
    pub fn with_worktree_reclaim_interval(mut self, interval: Duration) -> Self {
        self.worktree_reclaim_interval = interval;
        self
    }
    pub fn with_worktree_reclaim_day(mut self, day: Duration) -> Self {
        self.worktree_reclaim_day = day;
        self
    }

    /// Bounds how long an Errand may take; injectable so tests exercise a
    /// Provider that never answers without waiting out the default.
    pub fn with_errand_timeout(mut self, timeout: Duration) -> Self {
        self.errand_timeout = timeout;
        self
    }

    /// Shortens the seconds a Broker wait's timeout counts, so a test sees a
    /// wait time out, and its bounds kept, at millisecond scale.
    pub fn with_broker_wait_second(mut self, second: Duration) -> Self {
        self.broker_wait_second = second;
        self
    }

    /// Sets how often a Broker wait still waiting reports progress.
    pub fn with_broker_wait_progress_interval(mut self, interval: Duration) -> Self {
        self.broker_wait_progress_interval = interval;
        self
    }

    /// Sets how long after it was last uploaded or bound an Attachment is
    /// left in place even once no Session references it, before a Session's
    /// deletion or the orphan sweep may reclaim it.
    pub fn with_attachment_grace(mut self, grace: Duration) -> Self {
        self.attachment_grace = grace;
        self
    }

    /// Sets how long a quiet Server waits between sweeps of orphaned
    /// Attachments.
    pub fn with_attachment_sweep_interval(mut self, interval: Duration) -> Self {
        self.attachment_sweep_interval = interval;
        self
    }

    /// Sets the clock an Attachment's grace and the sweep interval are
    /// measured by, and that a Sidekick's listing of Sessions settles them
    /// against.
    pub fn with_clock(mut self, clock: ServerClock) -> Self {
        self.clock = clock;
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
                    attachments: Vec::new(),
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
        self.provider_shutdown.send_replace(true);
    }

    /// Closes the gate Provider actors write Sessions through. Called only
    /// once every Provider has shut down: a stopping actor settles the Turn
    /// and Subagents it was running, and those settlements are the record the
    /// next process reads, so the gate must stay open for them and close
    /// before the storage writer does (ADR 0029).
    fn stop_provider_updates(&self) {
        self.provider_updates.stop();
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
    /// The acts on Sessions, which the Session API's handlers only shape.
    operations: SessionOperations,
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
    serving: ServingController,
    shutdown: ShutdownController,
    timings: ServerTimings,
    /// The Attachments uploaded to this server, stored beside its Sessions.
    attachments: crate::attachments::AttachmentStore,
    /// The Workspace this Server owns, whose Sessions' Agents are Sidekicks.
    sidekick_workspace: crate::sidekick::SidekickWorkspace,
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
    let sidekick_workspace = crate::sidekick::SidekickWorkspace::beside(config.data_dir())
        .context("locate the Sidekick Workspace")?;

    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(config.lock_path())
        .context("open server election lock")?;
    take_election_lock(&lock, timings.election_handoff)
        .await
        .context("another server already owns this channel")?;
    protect_current_user_file(&config.lock_path())?;

    let config_documents = ConfigDocuments::new(config.config_dir());
    let (settings, _) = watch::channel(SettingsSnapshot::default());
    let opening_settings = config_documents.load();

    let repository = StorageRepository::open(config.data_dir())
        .await
        .context("initialize Session repository")?
        .with_attachment_grace(timings.attachment_grace)
        .with_attachment_sweep_interval(timings.attachment_sweep_interval)
        .with_clock(timings.clock.clone());
    let persisted_sessions = repository
        .load_sessions()
        .await
        .context("load persisted Sessions")?;
    // Uploads no stored Prompt or Message came to bind are reclaimed once
    // their grace period has passed; a failed sweep leaves them for the next.
    if let Err(error) = repository.sweep_orphaned_attachments().await {
        tracing::warn!("could not sweep orphaned Attachments at startup: {error}");
    }
    let persisted_landing_agent_selection = repository
        .landing_agent_selection()
        .await
        .context("load persisted landing Agent Selection")?;
    let remembered_model_catalog = repository
        .model_catalog()
        .await
        .context("load remembered Model Catalog")?;
    let workspace_icons = repository
        .workspace_icons()
        .await
        .context("load Workspace Icons")?;

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
    )?
    .with_withdrawal_timeout(timings.remote_withdrawal_timeout);
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
    let attachment_store = crate::attachments::AttachmentStore::new(repository.clone());
    let (storage_writer, storage) = StorageWriter::spawn(repository, &[]);
    let preparations = crate::source_control::PreparationStore::new(config.data_dir());
    let sessions = SessionStore::new(
        persisted_sessions,
        storage.clone(),
        preparations.resumable_sessions(),
        workspace_icons,
    )
    .with_settings(settings.subscribe());
    let source_control = crate::source_control::SourceControlService::new(source_control)
        .with_sidekick_workspace(sidekick_workspace.clone());
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
    reclaim::spawn(
        sessions.clone(),
        source_control.clone(),
        preparations.clone(),
        settings.subscribe(),
        workspace_discovery_rx.clone(),
        provider_shutdown_rx.clone(),
        timings.clone(),
    );
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
    // The Broker lives on this same loopback listener, and its endpoint is
    // handed only to Provider starts: never written into the runtime
    // descriptor, whose token grants the whole API (ADR 0034). Whether a
    // Provider start is a Sidekick's is read from the Sidekick Workspace
    // beside this Channel's data (ADR 0042).
    let broker_access = BrokerAccess::new(
        format!("{}{}", descriptor.base_url, broker::BROKER_PATH),
        settings.subscribe(),
        sidekick_workspace.clone(),
    );
    let providers = ProviderOrchestrator::new(
        runtimes.as_ref().clone(),
        sessions.clone(),
        provider_shutdown_rx.clone(),
        provider_updates,
        settings.subscribe(),
        skill_catalog.clone(),
        source_control.clone(),
        timings.checkout_skill_timeout,
        broker_access.clone(),
        attachment_store.clone(),
    );
    // A Broker Tool spawning a Subagent starts that Subagent's Provider actor
    // through the same orchestrator every other Session's runs on, and one
    // reading a Subagent reads it from the same Session store.
    let broker_routes = broker::router(
        broker_access,
        BrokerTools::new(
            model_catalog.clone(),
            providers.clone(),
            sessions.clone(),
            settings.subscribe(),
        )
        .with_wait_timings(broker::WaitTimings {
            second: timings.broker_wait_second,
            progress_every: timings.broker_wait_progress_interval,
        })
        .with_clock(timings.clock.clone()),
        provider_shutdown_rx.clone(),
    );
    // Errands are abandoned on the same signal that stops Provider work, so a
    // shutting-down server never waits on one and never resumes one.
    let derivation = Derivation::new(
        ErrandRunner::new(runtimes.clone(), provider_shutdown_rx, settings.subscribe())
            .with_timeout(timings.errand_timeout),
        model_catalog.clone(),
        sessions.clone(),
        settings.subscribe(),
        source_control.clone(),
        preparations.clone(),
    );
    let operations = SessionOperations::new(
        sessions.clone(),
        providers.clone(),
        source_control.clone(),
        preparations.clone(),
        skill_catalog.clone(),
        model_catalog.clone(),
        attachment_store.clone(),
        landing_agent_selection.clone(),
        derivation,
        settings.subscribe(),
        Arc::new(hosted_providers),
        timings.checkout_skill_timeout,
    );
    let state = AppState {
        preparations,
        source_control,
        workspace_paths: crate::protocol::WorkspacePaths::discover(),
        descriptor: Arc::new(descriptor.clone()),
        sessions: sessions.clone(),
        providers: providers.clone(),
        operations,
        model_catalog,
        skill_catalog,
        landing_agent_selection,
        settings: Arc::new(settings),
        config_documents,
        runtimes,
        serving: serving.clone(),
        shutdown: shutdown.clone(),
        timings,
        attachments: attachment_store,
        sidekick_workspace,
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
            "/v1/pairing/remotes/{name}",
            axum::routing::delete(remove_remote),
        )
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
        .route("/v1/workspaces/icon", post(set_workspace_icon))
        .route("/v1/workspaces/sidekick", post(resolve_sidekick_workspace))
        .route("/v1/checkouts/prepare", post(prepare_checkout))
        .route(
            "/v1/checkouts/removal-preview",
            post(preview_checkout_removal),
        )
        .route("/v1/checkouts/remove", post(remove_checkout))
        .route("/v1/attachments", post(attachments::upload_attachment))
        .route(
            "/v1/attachments/{attachment_id}",
            get(attachments::fetch_attachment).head(attachments::head_attachment),
        )
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
                .route("/v1/sessions/{session_id}/icon", post(set_session_icon))
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
                .route("/v1/sessions/{session_id}/compact", post(compact_session))
                .route("/v1/sessions/{session_id}/context", get(context_breakdown))
                .route("/v1/sessions/{session_id}/events", get(session_events))
                .route(
                    "/v1/sessions/{session_id}/subagent-tree",
                    get(subagent_tree_events),
                )
                .route_layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    hydrate_session_request,
                )),
        )
        .route("/v1/server/stop", post(stop_server))
        .with_state(state)
        .merge(broker_routes);
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
        task_shutdown.stop_provider_updates();
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

/// Takes the channel's election lock, waiting up to `handoff` for a hold that
/// is only draining. The lock belongs to an open file description, and a child
/// being spawned shares every description its parent has open until its `exec`
/// closes them. So a lock goes on being held after its owner lets go, for as
/// long as any spawn begun in that owner's process takes to finish — which on
/// macOS, where a program is assessed on its first run, can be a few hundred
/// milliseconds. Only a lock still held once `handoff` has passed belongs to a
/// server that is running.
async fn take_election_lock(lock: &std::fs::File, handoff: Duration) -> std::io::Result<()> {
    const POLL_INTERVAL: Duration = Duration::from_millis(10);
    let deadline = tokio::time::Instant::now() + handoff;
    loop {
        match lock.try_lock_exclusive() {
            Ok(()) => return Ok(()),
            Err(error)
                if error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep_until(
                    (tokio::time::Instant::now() + POLL_INTERVAL).min(deadline),
                )
                .await;
            }
            Err(error) => return Err(error),
        }
    }
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

async fn remove_remote(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(name): AxumPath<String>,
) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match state.serving.remove_remote(&name).await {
        Ok(removal) => Json(removal).into_response(),
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
    if request.prompt.text.trim().is_empty() {
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
        Ok(None) => match state
            .source_control
            .plan_checkout(&request, &state.preparations.intended_destinations())
            .await
        {
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
    match state.operations.rejoin_preparation(&mut preparation).await {
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

async fn removal_preview(
    state: &AppState,
    target: crate::protocol::CheckoutRemovalTarget,
) -> Result<crate::protocol::CheckoutRemovalPreview, String> {
    let inspection = state.source_control.inspect_removal(&target).await?;
    let branch_outcome = state
        .source_control
        .removal_branch_outcome(&target, &inspection)
        .await?;
    let (affected_sessions, working_sessions) =
        state.sessions.checkout_references(&target.checkout);
    Ok(crate::protocol::CheckoutRemovalPreview {
        target,
        inspection,
        branch_outcome,
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
    let mut preview = match removal_preview(&state, request.preview.target.clone()).await {
        Ok(preview) => preview,
        Err(e) => return preparation_error(e),
    };
    let result = async {
        if preview.working_sessions != 0 { return Err("A Session associated with this Worktree is Working on this Server; force cannot override it".to_owned()); }
        if preview != request.preview { return Err("Worktree conditions or affected Sessions changed; review the updated preview and confirm again".to_owned()); }
        if preview.inspection.requires_force() && !request.force { return Err("Git requires explicit force removal for these conditions; review and choose Force remove".to_owned()); }
        let preparations = state.preparations.for_destination(&preview.target.checkout.root)?;
        let recovery = crate::source_control::recovery_for_branch_outcome(
            &preview.inspection.checkout,
            preview.branch_outcome,
        )?;
        state.sessions.record_checkout(recovery.clone()).map_err(|e| format!("Cannot persist recovery facts before removal: {e}"))?;
        if state
            .sessions
            .checkout_references(&preview.target.checkout)
            .1
            != 0
        {
            return Err("A Session became Working; Worktree removal is blocked".into());
        }
        let branch_outcome = match state.source_control.remove_checkout(&preview.target, &preview.inspection, request.force, preview.branch_outcome).await {
            Ok(outcome) => outcome,
            Err(error) => {
                // The Worktree remains usable when Git refuses its removal.
                // Undo the provisional detached facts before reporting failure.
                state.sessions.record_checkout(preview.inspection.checkout.clone()).map_err(|restore| format!("{error}; cannot restore recovery facts after failed removal: {restore}"))?;
                return Err(error);
            }
        };
        // Checkout observation does not take the mutation guard and may have
        // published a branch reading while removal was in flight. Reassert the
        // recovery shape matching the actual branch outcome before Unavailable.
        let final_recovery = crate::source_control::recovery_for_branch_outcome(
            &preview.inspection.checkout,
            branch_outcome,
        )?;
        state.sessions.record_checkout(final_recovery).map_err(|e| format!("Cannot persist final recovery facts after removal: {e}"))?;
        for preparation in preparations {
            if let Err(retire) = state.preparations.retire(preparation.id) {
                let deleted = state.preparations.finish_retirement(preparation.id);
                if let Err(delete) = deleted {
                    tracing::warn!(
                        checkout = %preview.target.checkout.root.display(),
                        preparation = %preparation.id.0,
                        "Removed Worktree preparation could not be retired: {retire}; {delete}"
                    );
                }
                continue;
            }
            if let Err(error) = state
                .source_control
                .checkpoint(
                    crate::source_control::PreparationCheckpoint::IntentRetired,
                    &preparation,
                )
                .await
            {
                tracing::warn!(
                    preparation = %preparation.id.0,
                    "Retired Worktree preparation cleanup interrupted: {error}"
                );
                continue;
            }
            if let Err(error) = state.preparations.finish_retirement(preparation.id) {
                tracing::warn!(
                    preparation = %preparation.id.0,
                    "Retired Worktree preparation cleanup will retry after restart: {error}"
                );
            }
        }
        state.sessions.record_checkout(crate::protocol::CheckoutSummary { association: preview.target.checkout.clone(), revision: None, availability: crate::protocol::SourceControlAvailability::Unavailable { reason: "Worktree was explicitly removed; prompt a retained Session to recover it".into() } }).map_err(|e| e.to_string())?;
        Ok::<_, String>(branch_outcome)
    }.await;
    let (removed, error) = match result {
        Ok(branch_outcome) => {
            preview.branch_outcome = branch_outcome;
            (true, None)
        }
        Err(error) => (false, Some(error)),
    };
    Json(crate::protocol::RemoveCheckoutResult {
        preview,
        removed,
        error,
    })
    .into_response()
}

async fn create_session(State(state): State<AppState>, request: Request) -> Response {
    let request =
        match decode_session_command::<CreateSessionRequest>(&state, request, "Session creation")
            .await
        {
            Ok(request) => request,
            Err(response) => return response,
        };
    match state.operations.begin_session(request).await {
        Ok(StoreOutcome::Created(snapshot)) => {
            (StatusCode::CREATED, Json(snapshot)).into_response()
        }
        Ok(StoreOutcome::Existing(snapshot)) => (StatusCode::OK, Json(snapshot)).into_response(),
        Err(refusal) => prompt_refusal_response(refusal),
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
            // The brokered Subagents beneath the Session hold what was just
            // derived from this posture, each owed it on an actor of its own;
            // they are told alongside it, so the answer finds the tree
            // following the change.
            let own = async {
                match mutation.update {
                    Some(update) => state.providers.update_approval_posture(update).await,
                    None => Ok(crate::provider::ProviderPostureApplication::Applied),
                }
            };
            let (applied, ()) =
                tokio::join!(own, apply_live_posture_updates(&state, mutation.derived));
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
            state.operations.settle_actorless_posture(
                state
                    .sessions
                    .reconcile_tree_approval_posture(session_id, &state.settings.borrow().settings),
            );
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
    state
        .operations
        .normalize_agent_selection(selection)
        .map_err(|refusal| {
            Box::new(agent_selection_refusal_response(
                refusal,
                provider_conflict_response,
            ))
        })
}

fn agent_selection_refusal_response(
    refusal: AgentSelectionRefusal,
    provider_conflict_response: fn() -> Response,
) -> Response {
    match refusal {
        AgentSelectionRefusal::ProviderNotHosted => provider_conflict_response(),
        AgentSelectionRefusal::Invalid(message) => invalid_agent_selection_response(message),
    }
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
    match state.operations.admit_prompt(session_id, request).await {
        Ok(StoreOutcome::Created(prompt)) => (StatusCode::CREATED, Json(prompt)).into_response(),
        Ok(StoreOutcome::Existing(prompt)) => (StatusCode::OK, Json(prompt)).into_response(),
        Err(refusal) => prompt_refusal_response(refusal),
    }
}

/// A refused Prompt as the Session API answers one, whether it was to begin a
/// Session or to be admitted to one.
fn prompt_refusal_response(refusal: PromptRefusal) -> Response {
    match refusal {
        PromptRefusal::SessionNotFound => session_error_response(
            StatusCode::NOT_FOUND,
            SessionErrorCode::SessionNotFound,
            "Session does not exist on this server instance",
        ),
        PromptRefusal::SubagentSession => session_error_response(
            StatusCode::CONFLICT,
            SessionErrorCode::SubagentSession,
            "A Subagent's Session refuses Prompts",
        ),
        PromptRefusal::EmptyPrompt => session_error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            SessionErrorCode::EmptyPrompt,
            "Prompt must contain non-whitespace text",
        ),
        PromptRefusal::PromptConflict => prompt_conflict_response(),
        PromptRefusal::Attachment(refusal) => attachments::binding_refusal_response(&refusal),
        PromptRefusal::Skill(error) => skill_catalog_error_response(error),
        PromptRefusal::AgentSelection(refusal) => agent_selection_refusal_response(
            refusal,
            landing_agent_selection_provider_conflict_response,
        ),
        PromptRefusal::InvalidWorkspace(reason) => preparation_error(reason),
        PromptRefusal::RepositoryLabels(error) => session_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            SessionErrorCode::InvalidCommand,
            error,
        ),
        PromptRefusal::Storage => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
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
        Err(PromptMutationError::CompactionInProgress) => session_error_response(
            StatusCode::CONFLICT,
            SessionErrorCode::CompactionInProgress,
            "A Compaction is running, and its Turn takes no steer; the Prompt stays queued",
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
        .operations
        .answer_questionnaire(session_id, id, submission)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(AnswerRefusal::SubmissionFailed(message)) => session_error_response(
            StatusCode::CONFLICT,
            SessionErrorCode::QuestionnaireSubmissionFailed,
            &message,
        ),
        Err(AnswerRefusal::Storage) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
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
    match state.operations.interrupt_session(session_id).await {
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
            "Session has no active Turn, no working Subagent, and no live Watch",
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

/// Asks a Session's Provider to compact its context now, in a Turn of its own
/// (ADR 0041). The request is taken once that Turn opens, so it answers
/// nothing further: the Turn and its Compaction arrive on the Session's
/// stream, and anything that kept the Provider from compacting is recorded on
/// the Turn. Each refusal is typed, so a client can explain it.
async fn compact_session(
    State(state): State<AppState>,
    AxumPath(session_id): AxumPath<SessionId>,
    request: Request,
) -> Response {
    let request = match decode_session_command::<CompactSessionRequest>(
        &state,
        request,
        "Session compaction",
    )
    .await
    {
        Ok(request) => request,
        Err(response) => return response,
    };
    match state.providers.compact_session(session_id, request) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(CompactSessionError::SessionNotFound) => session_error_response(
            StatusCode::NOT_FOUND,
            SessionErrorCode::SessionNotFound,
            "Session does not exist on this server instance",
        ),
        Err(CompactSessionError::SubagentSession) => session_error_response(
            StatusCode::CONFLICT,
            SessionErrorCode::SubagentSession,
            "A Subagent's context is compacted only when its Provider chooses to",
        ),
        Err(CompactSessionError::Unsupported) => session_error_response(
            StatusCode::CONFLICT,
            SessionErrorCode::CompactionUnsupported,
            "This Session's Provider compacts only when it chooses to",
        ),
        Err(CompactSessionError::InstructionsUnsupported) => session_error_response(
            StatusCode::CONFLICT,
            SessionErrorCode::CompactionInstructionsUnsupported,
            "This Session's Provider takes no instructions for what a Compaction keeps",
        ),
        Err(CompactSessionError::PendingIntervention) => session_error_response(
            StatusCode::CONFLICT,
            SessionErrorCode::PendingIntervention,
            "Session is waiting on an Approval or Questionnaire; answer it before compacting",
        ),
        Err(CompactSessionError::WorkingSession) => session_error_response(
            StatusCode::CONFLICT,
            SessionErrorCode::WorkingSession,
            "Session is Working; compact it once it is idle",
        ),
    }
}

/// Asks a Session's Provider what occupies its context now. Nothing is
/// stored: the answer describes the context only as it was when asked.
async fn context_breakdown(
    State(state): State<AppState>,
    AxumPath(session_id): AxumPath<SessionId>,
    headers: HeaderMap,
) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match state.providers.context_breakdown(session_id).await {
        Ok(breakdown) => (StatusCode::OK, Json(breakdown)).into_response(),
        Err(ContextBreakdownError::SessionNotFound) => session_error_response(
            StatusCode::NOT_FOUND,
            SessionErrorCode::SessionNotFound,
            "Session does not exist on this server instance",
        ),
        Err(ContextBreakdownError::Unsupported) => session_error_response(
            StatusCode::CONFLICT,
            SessionErrorCode::ContextBreakdownUnsupported,
            "This Session's Provider does not say what occupies its context",
        ),
        Err(ContextBreakdownError::NotRunning) => session_error_response(
            StatusCode::CONFLICT,
            SessionErrorCode::ContextBreakdownUnavailable,
            "This Session's Provider is not running; prompt the Session to start it",
        ),
        Err(ContextBreakdownError::RidesAnotherSession) => session_error_response(
            StatusCode::CONFLICT,
            SessionErrorCode::ContextBreakdownUnavailable,
            "This Subagent's context is held by its parent's Provider, which cannot single it out",
        ),
        Err(ContextBreakdownError::Failed(message)) => session_error_response(
            StatusCode::BAD_GATEWAY,
            SessionErrorCode::ContextBreakdownFailed,
            message,
        ),
    }
}

// A rejection is the Response the handler returns as-is, which is the axum
// idiom; boxing it would only add an allocation to every refusal.
#[allow(clippy::result_large_err)]
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

/// Answers with this Server's Sidekick Workspace, made first if it is not
/// there yet, resolved as a Workspace a Session can begin in — which is how
/// `/sidekick` opens the Landing there. A Peer reaches it as it reaches the
/// rest of the Session API, so a Client turned toward a Remote opens that
/// Remote's own.
async fn resolve_sidekick_workspace(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let root = match state.sidekick_workspace.ensure() {
        Ok(root) => root,
        Err(error) => {
            tracing::error!("could not make the Sidekick Workspace: {error:#}");
            return session_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                SessionErrorCode::InvalidWorkspace,
                format!("Could not make the Sidekick Workspace: {error}"),
            );
        }
    };
    // Source control reads it as a directory Workspace of its own, whatever
    // Repository the data root lies within.
    Json(state.source_control.resolve(&root, None).await).into_response()
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
    match state.sessions.delete(session_id) {
        Ok(deleted) => {
            if let Some(repository) = deleted.repository {
                reclaim::spawn_orphans(
                    repository,
                    state.sessions.clone(),
                    state.source_control.clone(),
                    state.preparations.clone(),
                    state.settings.subscribe(),
                    state.shutdown.provider_shutdown.subscribe(),
                    state.timings.worktree_reclaim_day,
                );
            }
            for owner in deleted.actor_owners {
                state.providers.close_session(owner).await;
            }
            StatusCode::NO_CONTENT.into_response()
        }
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
        Err(DeleteSessionError::WorkingSession) => session_error_response(
            StatusCode::CONFLICT,
            SessionErrorCode::WorkingSession,
            "A Working Session or Subagent cannot be deleted",
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
    match state
        .operations
        .settle_session(session_id, settlement.settled)
        .await
    {
        Ok(summary) => Json(summary).into_response(),
        Err(SettleRefusal::SessionNotFound) => session_error_response(
            StatusCode::NOT_FOUND,
            SessionErrorCode::SessionNotFound,
            "Session does not exist on this server instance",
        ),
        Err(SettleRefusal::Storage) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Sets a Session's Icon to the user's own choice from the Icon Catalog,
/// refused where the Catalog does not carry the named entry. Answers with the
/// same `TitleChanged` catalog change a derived Icon publishes, so every
/// client's existing apply path repaints from it.
async fn set_session_icon(
    State(state): State<AppState>,
    AxumPath(session_id): AxumPath<SessionId>,
    request: Request,
) -> Response {
    let request = match decode_session_command::<SetSessionIconRequest>(
        &state,
        request,
        "Session Icon",
    )
    .await
    {
        Ok(request) => request,
        Err(response) => return response,
    };
    match state.sessions.set_icon(session_id, &request.icon) {
        Ok(summary) => Json(summary).into_response(),
        Err(SetIconError::SessionNotFound) => session_error_response(
            StatusCode::NOT_FOUND,
            SessionErrorCode::SessionNotFound,
            "Session does not exist on this server instance",
        ),
        Err(SetIconError::UnknownIcon) => session_error_response(
            StatusCode::BAD_REQUEST,
            SessionErrorCode::InvalidIcon,
            format!("`{}` is not an Icon Catalog name", request.icon),
        ),
        Err(SetIconError::Storage) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Sets a Workspace's Icon to the user's own choice from the Icon Catalog,
/// refused where the Catalog does not carry the named entry or where this
/// server does not know the named Workspace at all. Routed to the
/// Workspace's own Origin exactly like a Session's chosen Icon is routed to
/// its Session's Origin (see `ManagedClient::set_workspace_icon` in
/// `managed_client.rs`); answers with the same `WorkspaceIconChanged` catalog
/// change [`SessionStore::commit_workspace_icon`] publishes for a derived
/// one, so the choosing client and every other client repaint from it.
async fn set_workspace_icon(State(state): State<AppState>, request: Request) -> Response {
    let request =
        match decode_session_command::<SetWorkspaceIconRequest>(&state, request, "Workspace Icon")
            .await
        {
            Ok(request) => request,
            Err(response) => return response,
        };
    match state
        .sessions
        .set_workspace_icon(&request.workspace_id, &request.icon)
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(SetWorkspaceIconError::WorkspaceNotFound) => session_error_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            SessionErrorCode::InvalidWorkspace,
            format!(
                "`{}` is not a Workspace this server knows",
                request.workspace_id.0
            ),
        ),
        Err(SetWorkspaceIconError::UnknownIcon) => session_error_response(
            StatusCode::BAD_REQUEST,
            SessionErrorCode::InvalidIcon,
            format!("`{}` is not an Icon Catalog name", request.icon),
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

/// The live tree the Session belongs to, headed by its top-level Session
/// whichever Session in the tree is asked through: a snapshot, then every
/// change to it, beside the Session's own event stream.
async fn subagent_tree_events(
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
    let Some(feed) = state.sessions.subscribe_subagent_tree(session_id) else {
        return session_error_response(
            StatusCode::NOT_FOUND,
            SessionErrorCode::SessionNotFound,
            "Session does not exist on this server instance",
        );
    };
    let revision = feed.snapshot.revision;
    Sse::new(snapshot_first_event_stream(
        feed.snapshot,
        revision,
        feed.updates,
        shutdown,
        state.timings.sse_keepalive_interval,
        RevisionedEventProtocol {
            event_names: RevisionedEventNames {
                snapshot: SUBAGENT_TREE_SNAPSHOT_EVENT,
                update: SUBAGENT_TREE_UPDATED_EVENT,
            },
            update_revision: |update: &SubagentTreeUpdate| update.revision,
        },
    ))
    .into_response()
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

impl StreamRevision for SubagentTreeRevision {
    fn immediately_follows(self, previous: Self) -> bool {
        SubagentTreeRevision::immediately_follows(self, previous)
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

/// A Session error as every route answers one: its code and message in the
/// body, and its code again in [`SESSION_ERROR_CODE_HEADER`], which a body-less
/// answer keeps.
pub(crate) fn session_error_response(
    status: StatusCode,
    code: SessionErrorCode,
    message: impl Into<String>,
) -> Response {
    let code_header = HeaderValue::from_str(&code.wire_name())
        .expect("a Session error code's wire name is a valid header value");
    (
        status,
        [(SESSION_ERROR_CODE_HEADER, code_header)],
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
