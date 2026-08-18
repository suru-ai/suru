use std::{
    collections::HashMap,
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
    routing::{get, post},
};
use fs2::FileExt;
use futures_util::{StreamExt, stream};
use serde::{Deserialize, de::DeserializeOwned};
use tokio::{
    net::TcpListener,
    sync::{broadcast, mpsc, oneshot, watch},
    task::JoinHandle,
    time::{Duration, Instant},
};
use uuid::Uuid;

use crate::RuntimeConfig;
use crate::build_identity;
use crate::protocol::{
    Activity, ActivityId, ActivityKind, AdmitPromptRequest, CreateSessionRequest, LifecycleState,
    Message, MessageId, MessageRole, MessageStatus, PROTOCOL_VERSION, RuntimeDescriptor,
    SERVER_SHUTDOWN_EVENT, SESSION_SNAPSHOT_EVENT, SESSION_UPDATED_EVENT, ServerIdentity,
    ServerShutdown, SessionChange, SessionError, SessionErrorCode, SessionId, SessionUpdate,
    ShutdownReason, TurnId,
};
use crate::provider::{
    ProviderEvent, ProviderEventStream, ProviderRuntime, ProviderSession, ProviderSessionRequest,
    ProviderTurnRequest, UnavailableProviderRuntime,
};
use crate::runtime::protect_current_user_file;
use crate::sessions::{
    AdmitPromptError, CreateSessionError, InterruptTurnError, ListSessionsError,
    PromptMutationError, SessionFeed, SessionStore, StoreOutcome,
};

pub type ServerConfig = RuntimeConfig;

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
        activity_id: ActivityId,
        turn_id: TurnId,
        kind: ActivityKind,
        text: String,
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
            AgentOutput::Activity {
                activity_id,
                turn_id,
                kind,
                text,
            } => SessionChange::ActivityAdded {
                activity: Activity {
                    id: activity_id,
                    turn_id,
                    kind,
                    text,
                },
            },
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
}

impl ShutdownController {
    fn lifecycle(&self) -> LifecycleState {
        self.lifecycle.borrow().clone()
    }

    fn subscribe_to_intent(&self) -> watch::Receiver<Option<ServerShutdown>> {
        self.shutdown_intent.subscribe()
    }

    fn request(&self, request: ServerShutdown) {
        self.lifecycle.send_replace(LifecycleState::Stopping);
        self.shutdown_intent.send_replace(Some(request));
        let shutdown = self
            .shutdown
            .lock()
            .expect("shutdown sender lock is not poisoned")
            .take();
        if let Some(shutdown) = shutdown {
            tokio::spawn(async move {
                // Keep health and existing streams available briefly so the accepted response and
                // final authenticated intent can reach clients before graceful transport closure.
                tokio::time::sleep(Duration::from_millis(100)).await;
                let _ = shutdown.send(());
            });
        }
    }
}

#[derive(Clone)]
struct AppState {
    descriptor: Arc<RuntimeDescriptor>,
    sessions: SessionStore,
    providers: ProviderOrchestrator,
    shutdown: ShutdownController,
}

#[derive(Clone)]
struct ProviderOrchestrator {
    runtime: Arc<dyn ProviderRuntime>,
    sessions: SessionStore,
    actors: Arc<Mutex<HashMap<SessionId, mpsc::UnboundedSender<ProviderCommand>>>>,
}

#[derive(Clone, Copy)]
struct ProviderCommand {
    prompt_id: crate::protocol::PromptId,
}

struct ConnectedProviderSession {
    session: Arc<dyn ProviderSession>,
    events: ProviderEventStream,
}

struct ActiveProviderTurn {
    turn_id: TurnId,
    streaming_message_id: Option<MessageId>,
}

impl ProviderOrchestrator {
    fn new(runtime: Arc<dyn ProviderRuntime>, sessions: SessionStore) -> Self {
        Self {
            runtime,
            sessions,
            actors: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn open_session(
        &self,
        session_id: SessionId,
        workspace: PathBuf,
        prompt_id: crate::protocol::PromptId,
    ) {
        let (commands_tx, commands_rx) = mpsc::unbounded_channel();
        self.actors
            .lock()
            .expect("Provider actor registry lock is not poisoned")
            .insert(session_id, commands_tx.clone());
        let runtime = self.runtime.clone();
        let sessions = self.sessions.clone();
        tokio::spawn(run_provider_session(
            runtime,
            sessions,
            session_id,
            workspace,
            commands_rx,
        ));
        commands_tx
            .send(ProviderCommand { prompt_id })
            .expect("new Provider actor accepts its initial Prompt");
    }

    fn deliver(&self, session_id: SessionId, prompt_id: crate::protocol::PromptId) -> Result<()> {
        let actor = self
            .actors
            .lock()
            .expect("Provider actor registry lock is not poisoned")
            .get(&session_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Session has no Provider actor"))?;
        actor
            .send(ProviderCommand { prompt_id })
            .map_err(|_| anyhow::anyhow!("Session Provider actor stopped unexpectedly"))
    }
}

async fn run_provider_session(
    runtime: Arc<dyn ProviderRuntime>,
    sessions: SessionStore,
    session_id: SessionId,
    workspace: PathBuf,
    mut commands: mpsc::UnboundedReceiver<ProviderCommand>,
) {
    let mut provider: Option<ConnectedProviderSession> = None;
    let mut active: Option<ActiveProviderTurn> = None;

    loop {
        if active.is_none() {
            let Some(command) = commands.recv().await else {
                return;
            };
            if provider.is_none() {
                let connection = runtime
                    .start_session(ProviderSessionRequest {
                        workspace: workspace.clone(),
                    })
                    .await;
                let connection = match connection {
                    Ok(connection) => connection,
                    Err(error) => {
                        let _ = sessions.fail_pending_prompt(
                            session_id,
                            command.prompt_id,
                            format!("Provider startup failed: {error}"),
                        );
                        continue;
                    }
                };
                let (identity, session, events) = connection.into_parts();
                if let Err(error) = sessions.bind_agent(session_id, identity) {
                    let _ = sessions.fail_pending_prompt(
                        session_id,
                        command.prompt_id,
                        format!("Provider startup failed: {error}"),
                    );
                    continue;
                }
                provider = Some(ConnectedProviderSession { session, events });
            }

            let started = match sessions.start_prompt(session_id, command.prompt_id) {
                Ok(Some(started)) => started,
                Ok(None) => continue,
                Err(_) => continue,
            };
            let provider_session = provider
                .as_ref()
                .expect("Provider connection exists before Prompt delivery")
                .session
                .clone();
            if let Err(error) = provider_session
                .start_turn(ProviderTurnRequest {
                    prompt: started.prompt.text,
                })
                .await
            {
                let _ = sessions.fail_turn(
                    session_id,
                    started.turn.id,
                    None,
                    format!("Provider execution failed: {error}"),
                );
                continue;
            }
            active = Some(ActiveProviderTurn {
                turn_id: started.turn.id,
                streaming_message_id: None,
            });
            continue;
        }

        let connected = provider
            .as_mut()
            .expect("an active Provider Turn has a Provider Session");
        tokio::select! {
            command = commands.recv() => {
                if command.is_none() {
                    return;
                }
                // Commands admitted while work is active remain pending. Steering and queued
                // delivery are added at their dedicated orchestration seams.
            }
            event = connected.events.next() => {
                let Some(current) = active.as_mut() else {
                    continue;
                };
                match event {
                    Some(Ok(event)) => {
                        if project_provider_event(&sessions, session_id, current, event) {
                            active = None;
                        }
                    }
                    Some(Err(error)) => {
                        let _ = sessions.fail_turn(
                            session_id,
                            current.turn_id,
                            current.streaming_message_id,
                            format!("Provider execution failed: {error}"),
                        );
                        active = None;
                        provider = None;
                    }
                    None => {
                        let _ = sessions.fail_turn(
                            session_id,
                            current.turn_id,
                            current.streaming_message_id,
                            "Provider execution failed: the Provider Session ended before the Turn completed."
                                .to_owned(),
                        );
                        active = None;
                        provider = None;
                    }
                }
            }
        }
    }
}

fn project_provider_event(
    sessions: &SessionStore,
    session_id: SessionId,
    active: &mut ActiveProviderTurn,
    event: ProviderEvent,
) -> bool {
    let projection = match event {
        ProviderEvent::AgentMessageStarted => {
            if active.streaming_message_id.is_some() {
                Err(anyhow::anyhow!(
                    "Provider started a second Agent Message before completing the first"
                ))
            } else {
                let message_id = MessageId::new();
                active.streaming_message_id = Some(message_id);
                sessions
                    .publish_agent_output(
                        session_id,
                        SessionChange::MessageAdded {
                            message: Message {
                                id: message_id,
                                turn_id: active.turn_id,
                                role: MessageRole::Agent,
                                status: MessageStatus::Streaming,
                                content: String::new(),
                            },
                        },
                    )
                    .map(|_| false)
            }
        }
        ProviderEvent::AgentMessageDelta { content } => {
            let Some(message_id) = active.streaming_message_id else {
                return fail_invalid_provider_event(
                    sessions,
                    session_id,
                    active,
                    "Provider sent Agent Message content before starting a Message",
                );
            };
            sessions
                .publish_agent_output(
                    session_id,
                    SessionChange::MessageContentAppended {
                        message_id,
                        content,
                    },
                )
                .map(|_| false)
        }
        ProviderEvent::AgentMessageCompleted => {
            let Some(message_id) = active.streaming_message_id.take() else {
                return fail_invalid_provider_event(
                    sessions,
                    session_id,
                    active,
                    "Provider completed an Agent Message before starting one",
                );
            };
            sessions
                .publish_agent_output(session_id, SessionChange::MessageCompleted { message_id })
                .map(|_| false)
        }
        ProviderEvent::TurnCompleted => {
            if active.streaming_message_id.is_some() {
                Err(anyhow::anyhow!(
                    "Provider completed the Turn before completing its Agent Message"
                ))
            } else {
                sessions
                    .complete_turn(session_id, active.turn_id)
                    .map(|_| true)
            }
        }
        ProviderEvent::TurnFailed { message } => sessions
            .fail_turn(
                session_id,
                active.turn_id,
                active.streaming_message_id.take(),
                message,
            )
            .map(|_| true),
    };

    projection.unwrap_or_else(|error| {
        let _ = sessions.fail_turn(
            session_id,
            active.turn_id,
            active.streaming_message_id.take(),
            format!("Provider execution failed: {error}"),
        );
        true
    })
}

fn fail_invalid_provider_event(
    sessions: &SessionStore,
    session_id: SessionId,
    active: &mut ActiveProviderTurn,
    message: &str,
) -> bool {
    let _ = sessions.fail_turn(
        session_id,
        active.turn_id,
        active.streaming_message_id.take(),
        format!("Provider execution failed: {message}"),
    );
    true
}

pub async fn spawn(config: ServerConfig) -> Result<RunningServer> {
    spawn_with_provider(config, Arc::new(UnavailableProviderRuntime)).await
}

pub async fn spawn_with_provider(
    config: ServerConfig,
    runtime: Arc<dyn ProviderRuntime>,
) -> Result<RunningServer> {
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
    let shutdown = ShutdownController {
        lifecycle: lifecycle.clone(),
        shutdown_intent: shutdown_intent.clone(),
        shutdown: Arc::new(Mutex::new(Some(shutdown_tx))),
    };
    let sessions = SessionStore::default();
    let providers = ProviderOrchestrator::new(runtime, sessions.clone());
    let state = AppState {
        descriptor: Arc::new(descriptor.clone()),
        sessions: sessions.clone(),
        providers,
        shutdown: shutdown.clone(),
    };
    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/events", get(events))
        .route("/v1/sessions", get(list_sessions).post(create_session))
        .route("/v1/sessions/{session_id}", get(read_session))
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
    let task = tokio::spawn(async move {
        let _lock = lock;
        let result = axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .context("serve local HTTP API");
        if result.is_err() {
            task_lifecycle.send_replace(LifecycleState::Failed);
        }
        remove_own_descriptor(&descriptor_path, instance_id);
        result
    });
    lifecycle.send_if_modified(|state| {
        if *state == LifecycleState::Starting {
            *state = LifecycleState::Ready;
            true
        } else {
            false
        }
    });

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

async fn events(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if state.shutdown.lifecycle() != LifecycleState::Ready {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }

    Sse::new(event_stream(
        state.shutdown.subscribe_to_intent(),
        Duration::from_secs(10),
    ))
    .into_response()
}

struct EventStreamState {
    shutdown: watch::Receiver<Option<ServerShutdown>>,
    keepalive: tokio::time::Interval,
    finished: bool,
}

fn event_stream(
    shutdown: watch::Receiver<Option<ServerShutdown>>,
    keepalive_interval: Duration,
) -> impl futures_util::Stream<Item = std::result::Result<Event, std::convert::Infallible>> {
    let first = stream::once(async move {
        Ok::<_, std::convert::Infallible>(Event::default().comment("connected"))
    });
    let state = EventStreamState {
        shutdown,
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

async fn health(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if !is_authenticated(&headers, &state.descriptor.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    Json(state.descriptor.health(state.shutdown.lifecycle())).into_response()
}

async fn create_session(State(state): State<AppState>, request: Request) -> Response {
    let request =
        match decode_session_command::<CreateSessionRequest>(&state, request, "Session creation")
            .await
        {
            Ok(request) => request,
            Err(response) => return response,
        };

    match state.sessions.create(request) {
        Ok(StoreOutcome::Created(snapshot)) => {
            state.providers.open_session(
                snapshot.session.id,
                snapshot.session.workspace.path.clone(),
                snapshot.prompts[0].id,
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
            if admission.deliver {
                state
                    .providers
                    .deliver(session_id, admission.prompt.id)
                    .expect("stored Sessions retain their Provider actor");
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
    match state.sessions.interrupt(session_id, turn_id) {
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
        Duration::from_secs(10),
    ))
    .into_response()
}

fn session_event_stream(
    feed: SessionFeed,
    shutdown: watch::Receiver<Option<ServerShutdown>>,
    keepalive_interval: Duration,
) -> impl futures_util::Stream<Item = std::result::Result<Event, std::convert::Infallible>> {
    let snapshot = feed.snapshot;
    let delivered_revision = snapshot.revision;
    let snapshot_event = Event::default()
        .event(SESSION_SNAPSHOT_EVENT)
        .id(snapshot.revision.0.to_string())
        .json_data(snapshot)
        .expect("Session snapshots always serialize");
    stream::once(async move { Ok::<_, std::convert::Infallible>(snapshot_event) }).chain(
        stream::unfold(
            (
                tokio::time::interval_at(Instant::now() + keepalive_interval, keepalive_interval),
                shutdown,
                feed.updates,
                delivered_revision,
            ),
            |(mut keepalive, mut shutdown, mut updates, delivered_revision)| async move {
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
                        if !update.revision.immediately_follows(delivered_revision) {
                            return None;
                        }
                        let next_revision = update.revision;
                        let event = Event::default()
                            .event(SESSION_UPDATED_EVENT)
                            .id(update.revision.0.to_string())
                            .json_data(update)
                            .expect("Session updates always serialize");
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
        let first = event_stream(shutdown.subscribe(), Duration::from_secs(60));
        let second = event_stream(shutdown.subscribe(), Duration::from_secs(60));
        pin_mut!(first);
        pin_mut!(second);

        assert!(
            first.next().await.is_some(),
            "first connected comment arrives"
        );
        assert!(
            second.next().await.is_some(),
            "second connected comment arrives"
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
