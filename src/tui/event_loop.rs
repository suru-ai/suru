//! Terminal lifecycle and the async run loop: the terminal session guard, the
//! `tokio::select!` loop that feeds the Application, and the tasks it spawns to
//! carry out the transitions the Application returns.

use std::{
    future::pending,
    io::{Stdout, stdout},
    ops::ControlFlow,
    path::PathBuf,
    pin::Pin,
    time::Duration,
};

use crate::{
    managed_client::{
        ManagedClient, ManagedEvent, SessionCommandClient, SessionEvent, SessionStreamError,
        SessionSubscription,
    },
    protocol::{
        AdmitPromptRequest, AgentSelection, AgentSelectionOperationId, CreateSessionRequest,
        ModelCatalog, PromptId, SessionId, SessionListItem, SessionSnapshot, TurnId,
        UpdateAgentSelectionRequest,
    },
};
use anyhow::{Result, anyhow};
use crossterm::{
    cursor::{Hide, Show},
    event::{
        DisableBracketedPaste, EnableBracketedPaste, Event as InputEvent, EventStream,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::sync::mpsc::UnboundedSender;

use super::state::{
    Application, ApplicationEvent, ApplicationTransition, ModelListRequest, SessionListRequest,
};

const RECONNECT_GRACE_PERIOD: Duration = Duration::from_secs(1);

pub async fn run(client: ManagedClient) -> Result<()> {
    let workspace =
        std::env::current_dir().map_err(|error| anyhow!("read current Workspace: {error}"))?;
    let mut session = TerminalSession::enter()?;
    run_loop(&mut session.terminal, client, workspace).await
}

/// How the run loop leaves the screen when an event ends the Session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Exit {
    /// Stop immediately.
    Now,
    /// Render one last frame so the closing state reaches the screen first.
    AfterFinalFrame,
}

/// The Session-scoped work the run loop owns: the live event subscription and
/// the tasks establishing it, attaching a Session, and filling the pickers.
#[derive(Default)]
struct SessionTasks {
    subscription: Option<SessionSubscription>,
    subscribing: Option<(SessionId, tokio::task::JoinHandle<()>)>,
    attaching: Option<tokio::task::JoinHandle<()>>,
    listing_sessions: Option<(SessionListRequest, tokio::task::JoinHandle<()>)>,
    listing_models: Option<(ModelListRequest, tokio::task::JoinHandle<()>)>,
}

impl SessionTasks {
    /// Drops the current Session's subscription along with any attempt to
    /// re-establish it, so nothing reconnects to a Session left behind.
    fn detach(&mut self) {
        self.subscription = None;
        self.abort_subscribing();
    }

    fn abort_subscribing(&mut self) {
        if let Some((_, task)) = self.subscribing.take() {
            task.abort();
        }
    }

    /// Subscribes to `session_id`, replacing any subscription attempt already
    /// in flight for an earlier Session.
    fn subscribe(
        &mut self,
        commands: SessionCommandClient,
        session_id: SessionId,
        connected: &UnboundedSender<ConnectedSessionSubscription>,
    ) {
        self.abort_subscribing();
        self.subscribing = Some((
            session_id,
            spawn_session_subscription(commands, session_id, connected.clone()),
        ));
    }

    /// Attaches to `session_id` unless an attachment is already in flight; the
    /// picker stays on the original Session until the target hydrates.
    fn attach(
        &mut self,
        commands: SessionCommandClient,
        session_id: SessionId,
        results: &UnboundedSender<SessionPickerResult>,
    ) {
        if self.attaching.is_none() {
            self.attaching = Some(spawn_session_attachment(
                commands,
                session_id,
                results.clone(),
            ));
        }
    }

    fn list_sessions(
        &mut self,
        commands: SessionCommandClient,
        request: SessionListRequest,
        results: &UnboundedSender<SessionPickerResult>,
    ) {
        let results = results.clone();
        replace_listing(&mut self.listing_sessions, request, |request| {
            spawn_session_listing(commands, request, results)
        });
    }

    fn list_models(
        &mut self,
        commands: SessionCommandClient,
        request: ModelListRequest,
        results: &UnboundedSender<ModelPickerResult>,
    ) {
        let results = results.clone();
        replace_listing(&mut self.listing_models, request, |request| {
            spawn_model_listing(commands, request, results)
        });
    }
}

/// The channels the run loop's spawned tasks report their results back on.
struct TaskChannels {
    submissions: UnboundedSender<SubmissionResult>,
    subscriptions: UnboundedSender<ConnectedSessionSubscription>,
    pickers: UnboundedSender<SessionPickerResult>,
    models: UnboundedSender<ModelPickerResult>,
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut client: ManagedClient,
    workspace: PathBuf,
) -> Result<()> {
    let mut application = Application::new(workspace);
    let mut input = EventStream::new();
    let mut tasks = SessionTasks::default();
    let mut reconnect_grace: Option<Pin<Box<tokio::time::Sleep>>> = None;
    let (submissions, mut submission_rx) = tokio::sync::mpsc::unbounded_channel();
    let (subscriptions, mut subscription_rx) = tokio::sync::mpsc::unbounded_channel();
    let (pickers, mut picker_rx) = tokio::sync::mpsc::unbounded_channel();
    let (models, mut model_rx) = tokio::sync::mpsc::unbounded_channel();
    let channels = TaskChannels {
        submissions,
        subscriptions,
        pickers,
        models,
    };
    let mut needs_redraw = true;

    loop {
        if needs_redraw {
            terminal.draw(|frame| application.render(frame))?;
            needs_redraw = false;
        }
        let step = tokio::select! {
            managed_event = client.next() => {
                needs_redraw = true;
                handle_managed_event(
                    managed_event,
                    &mut application,
                    &mut tasks,
                    &mut reconnect_grace,
                )?
            }
            () = wait_for_reconnect_grace(&mut reconnect_grace) => {
                needs_redraw = true;
                application.handle_event(ApplicationEvent::ReconnectGraceElapsed)?;
                reconnect_grace = None;
                ControlFlow::Continue(())
            }
            session_event = next_session_event(&mut tasks.subscription) => {
                needs_redraw = true;
                handle_session_stream_event(
                    session_event,
                    &mut application,
                    &client,
                    &mut tasks,
                    &channels,
                )?;
                ControlFlow::Continue(())
            }
            connected = subscription_rx.recv() => {
                let connected = connected.ok_or_else(|| {
                    anyhow!("Session subscription task channel stopped unexpectedly")
                })?;
                adopt_session_subscription(connected, &application, &mut tasks);
                ControlFlow::Continue(())
            }
            submission = submission_rx.recv() => {
                needs_redraw = true;
                let submission = submission.ok_or_else(|| {
                    anyhow!("Prompt admission task channel stopped unexpectedly")
                })?;
                handle_submission_result(
                    submission,
                    &mut application,
                    &client,
                    &mut tasks,
                    &channels,
                )?;
                ControlFlow::Continue(())
            }
            model = model_rx.recv() => {
                needs_redraw = true;
                let model = model.ok_or_else(|| {
                    anyhow!("Model picker task channel stopped unexpectedly")
                })?;
                handle_model_listing_result(model, &mut application, &mut tasks)?;
                ControlFlow::Continue(())
            }
            picker = picker_rx.recv() => {
                needs_redraw = true;
                let picker = picker.ok_or_else(|| {
                    anyhow!("Session picker task channel stopped unexpectedly")
                })?;
                handle_session_picker_result(
                    picker,
                    &mut application,
                    &client,
                    &mut tasks,
                    &channels,
                )?;
                ControlFlow::Continue(())
            }
            input_event = input.next() => {
                match input_event {
                    Some(Ok(event)) => handle_input_event(
                        event,
                        &mut application,
                        &client,
                        &mut tasks,
                        &channels,
                        &mut needs_redraw,
                    )?,
                    Some(Err(error)) => return Err(error.into()),
                    None => ControlFlow::Break(Exit::Now),
                }
            }
        };
        if let ControlFlow::Break(exit) = step {
            return leave_run_loop(terminal, &application, exit);
        }

        // Coalesce input that is already pending into this frame so a burst of
        // events (wheel scrolling, key auto-repeat) costs one redraw instead of
        // one per event. Bounded so a continuous flood cannot starve rendering.
        //
        // The stream must be polled with the run loop's own task context: a
        // detached poll (`now_or_never`) would hand the stream a no-op waker,
        // and a Pending poll would then leave nothing to wake this task when
        // the next event arrives, deadlocking all input.
        for _ in 0..128 {
            let pending_input = std::future::poll_fn(|context| {
                std::task::Poll::Ready(match input.poll_next_unpin(context) {
                    std::task::Poll::Ready(event) => Some(event),
                    std::task::Poll::Pending => None,
                })
            })
            .await;
            let Some(pending_input) = pending_input else {
                break;
            };
            let step = match pending_input {
                Some(Ok(event)) => handle_input_event(
                    event,
                    &mut application,
                    &client,
                    &mut tasks,
                    &channels,
                    &mut needs_redraw,
                )?,
                Some(Err(error)) => return Err(error.into()),
                None => ControlFlow::Break(Exit::Now),
            };
            if let ControlFlow::Break(exit) = step {
                return leave_run_loop(terminal, &application, exit);
            }
        }
    }
}

fn leave_run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    application: &Application,
    exit: Exit,
) -> Result<()> {
    if exit == Exit::AfterFinalFrame {
        terminal.draw(|frame| application.render(frame))?;
    }
    Ok(())
}

fn handle_input_event(
    event: InputEvent,
    application: &mut Application,
    client: &ManagedClient,
    tasks: &mut SessionTasks,
    channels: &TaskChannels,
    needs_redraw: &mut bool,
) -> Result<ControlFlow<Exit>> {
    if matches!(event, InputEvent::Resize(..)) {
        *needs_redraw = true;
    }
    let Some(command) = application.command_for_terminal_input(event) else {
        return Ok(ControlFlow::Continue(()));
    };
    *needs_redraw = true;
    let transition = application.handle_event(ApplicationEvent::Command(command))?;
    Ok(dispatch_transition(transition, client, tasks, channels))
}

/// Carries out the transition a terminal command produced, spawning whatever
/// Session command it asked for.
fn dispatch_transition(
    transition: ApplicationTransition,
    client: &ManagedClient,
    tasks: &mut SessionTasks,
    channels: &TaskChannels,
) -> ControlFlow<Exit> {
    match transition {
        ApplicationTransition::Continue => {}
        ApplicationTransition::Exit => return ControlFlow::Break(Exit::Now),
        ApplicationTransition::SessionEnded => tasks.subscription = None,
        ApplicationTransition::DetachSession => tasks.detach(),
        ApplicationTransition::CreateSession(request) => {
            spawn_session_creation(
                client.session_commands(),
                request,
                channels.submissions.clone(),
            );
        }
        ApplicationTransition::AdmitPrompt {
            session_id,
            request,
        } => {
            spawn_prompt_admission(
                client.session_commands(),
                session_id,
                request,
                channels.submissions.clone(),
            );
        }
        ApplicationTransition::PromotePrompt {
            session_id,
            prompt_id,
        } => {
            spawn_session_operation(
                client.session_commands(),
                SessionOperation::PromotePrompt {
                    session_id,
                    prompt_id,
                },
                channels.submissions.clone(),
            );
        }
        ApplicationTransition::CancelPrompt {
            session_id,
            prompt_id,
        } => {
            spawn_session_operation(
                client.session_commands(),
                SessionOperation::CancelPrompt {
                    session_id,
                    prompt_id,
                },
                channels.submissions.clone(),
            );
        }
        ApplicationTransition::InterruptTurn {
            session_id,
            turn_id,
        } => {
            spawn_session_operation(
                client.session_commands(),
                SessionOperation::InterruptTurn {
                    session_id,
                    turn_id,
                },
                channels.submissions.clone(),
            );
        }
        ApplicationTransition::DeleteSession(session_id) => {
            spawn_session_operation(
                client.session_commands(),
                SessionOperation::DeleteSession { session_id },
                channels.submissions.clone(),
            );
        }
        ApplicationTransition::SubscribeSession(_) => {
            unreachable!("terminal input cannot end a Session subscription")
        }
        ApplicationTransition::AttachSession(session_id) => {
            tasks.attach(client.session_commands(), session_id, &channels.pickers);
        }
        ApplicationTransition::ListSessions(request) => {
            tasks.list_sessions(client.session_commands(), request, &channels.pickers);
        }
        ApplicationTransition::ListModels(request) => {
            tasks.list_models(client.session_commands(), request, &channels.models);
        }
        ApplicationTransition::ConfirmLandingAgentSelection(selection) => {
            spawn_landing_agent_selection_confirmation(
                client.session_commands(),
                selection,
                channels.submissions.clone(),
            );
        }
        ApplicationTransition::UpdateAgentSelection {
            session_id,
            request,
        } => {
            spawn_agent_selection_update(
                client.session_commands(),
                session_id,
                request,
                channels.submissions.clone(),
            );
        }
    }
    ControlFlow::Continue(())
}

fn handle_managed_event(
    event: Option<ManagedEvent>,
    application: &mut Application,
    tasks: &mut SessionTasks,
    reconnect_grace: &mut Option<Pin<Box<tokio::time::Sleep>>>,
) -> Result<ControlFlow<Exit>> {
    let event = event.ok_or_else(|| anyhow!("managed client stopped unexpectedly"))?;
    let was_recovering = application.is_recovering();
    let transition = application.handle_event(ApplicationEvent::Managed(event))?;
    if application.is_recovering() {
        if !was_recovering {
            *reconnect_grace = Some(Box::pin(tokio::time::sleep(RECONNECT_GRACE_PERIOD)));
        }
    } else {
        *reconnect_grace = None;
    }
    match transition {
        ApplicationTransition::Continue => {}
        ApplicationTransition::SessionEnded => tasks.subscription = None,
        // The shutdown state is worth one last frame before the screen goes.
        ApplicationTransition::Exit => return Ok(ControlFlow::Break(Exit::AfterFinalFrame)),
        ApplicationTransition::CreateSession(_)
        | ApplicationTransition::DetachSession
        | ApplicationTransition::DeleteSession(_)
        | ApplicationTransition::AdmitPrompt { .. }
        | ApplicationTransition::PromotePrompt { .. }
        | ApplicationTransition::CancelPrompt { .. }
        | ApplicationTransition::InterruptTurn { .. }
        | ApplicationTransition::SubscribeSession(_)
        | ApplicationTransition::AttachSession(_)
        | ApplicationTransition::ListSessions(_)
        | ApplicationTransition::ListModels(_)
        | ApplicationTransition::ConfirmLandingAgentSelection(_)
        | ApplicationTransition::UpdateAgentSelection { .. } => {
            unreachable!("managed events do not issue Session commands");
        }
    }
    if application.session_id().is_none() {
        tasks.detach();
    }
    Ok(ControlFlow::Continue(()))
}

fn handle_session_stream_event(
    event: Option<std::result::Result<SessionEvent, SessionStreamError>>,
    application: &mut Application,
    client: &ManagedClient,
    tasks: &mut SessionTasks,
    channels: &TaskChannels,
) -> Result<()> {
    match event {
        Some(Ok(event)) => {
            application.handle_event(ApplicationEvent::Session(event))?;
        }
        Some(Err(error)) if !error.is_recoverable() => return Err(error.into()),
        Some(Err(_)) | None => recover_session_subscription(application, client, tasks, channels)?,
    }
    Ok(())
}

/// Adopts a subscription a spawned task established, unless the Application
/// has moved on to another Session while the task was connecting.
fn adopt_session_subscription(
    connected: ConnectedSessionSubscription,
    application: &Application,
    tasks: &mut SessionTasks,
) {
    if tasks
        .subscribing
        .as_ref()
        .is_some_and(|(session_id, _)| *session_id == connected.session_id)
    {
        tasks.subscribing = None;
    }
    if application.session_id() == Some(connected.session_id) {
        tasks.subscription = Some(connected.subscription);
    }
}

fn handle_submission_result(
    result: SubmissionResult,
    application: &mut Application,
    client: &ManagedClient,
    tasks: &mut SessionTasks,
    channels: &TaskChannels,
) -> Result<()> {
    match result {
        SubmissionResult::SessionCreated(created) => {
            let session_id = created.session.id;
            application.handle_event(ApplicationEvent::SessionCreated(*created))?;
            tasks.subscribe(
                client.session_commands(),
                session_id,
                &channels.subscriptions,
            );
        }
        SubmissionResult::PromptAdmitted(prompt_id) => {
            application.handle_event(ApplicationEvent::PromptAdmissionSucceeded(prompt_id))?;
        }
        SubmissionResult::Failed { prompt_id, error } => {
            application
                .handle_event(ApplicationEvent::PromptAdmissionFailed { prompt_id, error })?;
        }
        SubmissionResult::OperationSucceeded => {}
        SubmissionResult::OperationFailed(error) => {
            application.handle_event(ApplicationEvent::SessionOperationFailed(error))?;
        }
        SubmissionResult::SessionDeletionFailed { session_id, error } => {
            application
                .handle_event(ApplicationEvent::SessionDeletionFailed { session_id, error })?;
        }
        SubmissionResult::LandingAgentSelectionConfirmed(selection) => {
            let transition = application
                .handle_event(ApplicationEvent::LandingAgentSelectionConfirmed(selection))?;
            flush_landing_agent_selection(client, transition, &channels.submissions);
        }
        SubmissionResult::LandingAgentSelectionConfirmationFailed(error) => {
            let transition = application.handle_event(
                ApplicationEvent::LandingAgentSelectionConfirmationFailed(error),
            )?;
            flush_landing_agent_selection(client, transition, &channels.submissions);
        }
        SubmissionResult::AgentSelectionUpdated {
            operation_id,
            selection,
        } => {
            let transition = application.handle_event(ApplicationEvent::AgentSelectionUpdated {
                operation_id,
                selection,
            })?;
            flush_agent_selection(client, transition, &channels.submissions);
        }
        SubmissionResult::AgentSelectionUpdateFailed {
            operation_id,
            error,
        } => {
            let transition =
                application.handle_event(ApplicationEvent::AgentSelectionUpdateFailed {
                    operation_id,
                    error,
                })?;
            flush_agent_selection(client, transition, &channels.submissions);
        }
    }
    Ok(())
}

fn handle_model_listing_result(
    result: ModelPickerResult,
    application: &mut Application,
    tasks: &mut SessionTasks,
) -> Result<()> {
    match result {
        ModelPickerResult::Listed { request, catalog } => {
            application.handle_event(ApplicationEvent::ModelsListed { request, catalog })?;
        }
        ModelPickerResult::Refreshed { request, catalog } => {
            finish_listing(&mut tasks.listing_models, &request);
            application.handle_event(ApplicationEvent::ModelsRefreshed { request, catalog })?;
        }
        ModelPickerResult::Failed { request, error } => {
            finish_listing(&mut tasks.listing_models, &request);
            application.handle_event(ApplicationEvent::ModelListingFailed { request, error })?;
        }
    }
    Ok(())
}

fn handle_session_picker_result(
    result: SessionPickerResult,
    application: &mut Application,
    client: &ManagedClient,
    tasks: &mut SessionTasks,
    channels: &TaskChannels,
) -> Result<()> {
    match result {
        SessionPickerResult::Listed { request, sessions } => {
            finish_listing(&mut tasks.listing_sessions, &request);
            application.handle_event(ApplicationEvent::SessionsListed { request, sessions })?;
        }
        SessionPickerResult::ListingFailed { request, error } => {
            finish_listing(&mut tasks.listing_sessions, &request);
            application.handle_event(ApplicationEvent::SessionListingFailed { request, error })?;
        }
        SessionPickerResult::Attached {
            snapshot,
            subscription,
        } => {
            tasks.attaching = None;
            application.handle_event(ApplicationEvent::SessionAttached(*snapshot))?;
            tasks.subscription = Some(subscription);
            tasks.abort_subscribing();
        }
        SessionPickerResult::AttachmentFailed(error) => {
            tasks.attaching = None;
            let transition =
                application.handle_event(ApplicationEvent::SessionAttachmentFailed(error))?;
            if let ApplicationTransition::ListSessions(request) = transition {
                tasks.list_sessions(client.session_commands(), request, &channels.pickers);
            }
        }
    }
    Ok(())
}

fn spawn_session_creation(
    commands: SessionCommandClient,
    request: CreateSessionRequest,
    results: UnboundedSender<SubmissionResult>,
) {
    let prompt_id = request.prompt.id;
    tokio::spawn(async move {
        let result = match commands.create_session(request).await {
            Ok(created) => SubmissionResult::SessionCreated(Box::new(created)),
            Err(error) => SubmissionResult::Failed {
                prompt_id,
                error: error.to_string(),
            },
        };
        let _ = results.send(result);
    });
}

fn spawn_prompt_admission(
    commands: SessionCommandClient,
    session_id: SessionId,
    request: AdmitPromptRequest,
    results: UnboundedSender<SubmissionResult>,
) {
    let prompt_id = request.prompt.id;
    tokio::spawn(async move {
        let result = match commands.admit_prompt(session_id, request).await {
            Ok(prompt) => SubmissionResult::PromptAdmitted(prompt.id),
            Err(error) => SubmissionResult::Failed {
                prompt_id,
                error: error.to_string(),
            },
        };
        let _ = results.send(result);
    });
}

enum SubmissionResult {
    SessionCreated(Box<SessionSnapshot>),
    PromptAdmitted(PromptId),
    Failed {
        prompt_id: PromptId,
        error: String,
    },
    OperationSucceeded,
    OperationFailed(String),
    SessionDeletionFailed {
        session_id: SessionId,
        error: String,
    },
    LandingAgentSelectionConfirmed(AgentSelection),
    LandingAgentSelectionConfirmationFailed(String),
    AgentSelectionUpdated {
        operation_id: AgentSelectionOperationId,
        selection: AgentSelection,
    },
    AgentSelectionUpdateFailed {
        operation_id: AgentSelectionOperationId,
        error: String,
    },
}

enum ModelPickerResult {
    Listed {
        request: ModelListRequest,
        catalog: ModelCatalog,
    },
    Refreshed {
        request: ModelListRequest,
        catalog: ModelCatalog,
    },
    Failed {
        request: ModelListRequest,
        error: String,
    },
}

fn spawn_model_listing(
    commands: SessionCommandClient,
    request: ModelListRequest,
    results: UnboundedSender<ModelPickerResult>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let listing_error = match commands.list_models().await {
            Ok(catalog) => {
                if results
                    .send(ModelPickerResult::Listed {
                        request: request.clone(),
                        catalog,
                    })
                    .is_err()
                {
                    return;
                }
                None
            }
            Err(error) => Some(error.to_string()),
        };
        let result = match commands.refresh_models().await {
            Ok(catalog) => ModelPickerResult::Refreshed { request, catalog },
            Err(error) => ModelPickerResult::Failed {
                request,
                error: listing_error.map_or_else(
                    || error.to_string(),
                    |listing| format!("{listing}; refresh failed: {error}"),
                ),
            },
        };
        let _ = results.send(result);
    })
}

/// Dispatches the follow-up request when settling one Agent Selection
/// operation released a coalesced newer selection.
fn flush_agent_selection(
    client: &ManagedClient,
    transition: ApplicationTransition,
    results: &UnboundedSender<SubmissionResult>,
) {
    if let ApplicationTransition::UpdateAgentSelection {
        session_id,
        request,
    } = transition
    {
        spawn_agent_selection_update(
            client.session_commands(),
            session_id,
            request,
            results.clone(),
        );
    }
}

fn flush_landing_agent_selection(
    client: &ManagedClient,
    transition: ApplicationTransition,
    results: &UnboundedSender<SubmissionResult>,
) {
    if let ApplicationTransition::ConfirmLandingAgentSelection(selection) = transition {
        spawn_landing_agent_selection_confirmation(
            client.session_commands(),
            selection,
            results.clone(),
        );
    }
}

fn spawn_landing_agent_selection_confirmation(
    commands: SessionCommandClient,
    selection: AgentSelection,
    results: UnboundedSender<SubmissionResult>,
) {
    tokio::spawn(async move {
        let result = match commands.confirm_landing_agent_selection(selection).await {
            Ok(selection) => SubmissionResult::LandingAgentSelectionConfirmed(selection),
            Err(error) => {
                SubmissionResult::LandingAgentSelectionConfirmationFailed(error.to_string())
            }
        };
        let _ = results.send(result);
    });
}

fn spawn_agent_selection_update(
    commands: SessionCommandClient,
    session_id: SessionId,
    request: UpdateAgentSelectionRequest,
    results: UnboundedSender<SubmissionResult>,
) {
    tokio::spawn(async move {
        let operation_id = request.operation_id;
        let result = match commands.update_agent_selection(session_id, request).await {
            Ok(selection) => SubmissionResult::AgentSelectionUpdated {
                operation_id,
                selection,
            },
            Err(error) => SubmissionResult::AgentSelectionUpdateFailed {
                operation_id,
                error: error.to_string(),
            },
        };
        let _ = results.send(result);
    });
}

enum SessionPickerResult {
    Listed {
        request: SessionListRequest,
        sessions: Vec<SessionListItem>,
    },
    ListingFailed {
        request: SessionListRequest,
        error: String,
    },
    Attached {
        snapshot: Box<SessionSnapshot>,
        subscription: SessionSubscription,
    },
    AttachmentFailed(String),
}

fn replace_listing<Request: Clone>(
    active: &mut Option<(Request, tokio::task::JoinHandle<()>)>,
    request: Request,
    spawn: impl FnOnce(Request) -> tokio::task::JoinHandle<()>,
) {
    if let Some((_, task)) = active.take() {
        task.abort();
    }
    let task = spawn(request.clone());
    *active = Some((request, task));
}

fn finish_listing<Request: PartialEq>(
    active: &mut Option<(Request, tokio::task::JoinHandle<()>)>,
    completed: &Request,
) {
    if active
        .as_ref()
        .is_some_and(|(request, _)| request == completed)
    {
        *active = None;
    }
}

fn spawn_session_listing(
    commands: SessionCommandClient,
    request: SessionListRequest,
    results: UnboundedSender<SessionPickerResult>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let result = match commands
            .list_sessions(request.scope.workspace_filter())
            .await
        {
            Ok(sessions) => SessionPickerResult::Listed { request, sessions },
            Err(error) => SessionPickerResult::ListingFailed {
                request,
                error: error.to_string(),
            },
        };
        let _ = results.send(result);
    })
}

fn spawn_session_attachment(
    commands: SessionCommandClient,
    session_id: SessionId,
    results: UnboundedSender<SessionPickerResult>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let result = async {
            let mut subscription = commands.attach_session(session_id).await?;
            let event = subscription
                .next()
                .await
                .ok_or_else(|| anyhow!("target Session subscription ended before hydration"))??;
            let SessionEvent::Snapshot(snapshot) = event else {
                return Err(anyhow!("target Session updated before hydration"));
            };
            Ok::<_, anyhow::Error>(SessionPickerResult::Attached {
                snapshot: Box::new(snapshot),
                subscription,
            })
        }
        .await
        .unwrap_or_else(|error| SessionPickerResult::AttachmentFailed(error.to_string()));
        let _ = results.send(result);
    })
}

enum SessionOperation {
    DeleteSession {
        session_id: SessionId,
    },
    PromotePrompt {
        session_id: SessionId,
        prompt_id: PromptId,
    },
    CancelPrompt {
        session_id: SessionId,
        prompt_id: PromptId,
    },
    InterruptTurn {
        session_id: SessionId,
        turn_id: TurnId,
    },
}

impl SessionOperation {
    async fn run(self, commands: SessionCommandClient) -> SubmissionResult {
        match self {
            Self::DeleteSession { session_id } => match commands.delete_session(session_id).await {
                Ok(()) => SubmissionResult::OperationSucceeded,
                Err(error) => SubmissionResult::SessionDeletionFailed {
                    session_id,
                    error: error.to_string(),
                },
            },
            Self::PromotePrompt {
                session_id,
                prompt_id,
            } => operation_result(
                commands
                    .promote_prompt(session_id, prompt_id)
                    .await
                    .map(|_| ()),
            ),
            Self::CancelPrompt {
                session_id,
                prompt_id,
            } => operation_result(
                commands
                    .cancel_prompt(session_id, prompt_id)
                    .await
                    .map(|_| ()),
            ),
            Self::InterruptTurn {
                session_id,
                turn_id,
            } => operation_result(
                commands
                    .interrupt_turn(session_id, turn_id)
                    .await
                    .map(|_| ()),
            ),
        }
    }
}

fn operation_result(result: anyhow::Result<()>) -> SubmissionResult {
    match result {
        Ok(()) => SubmissionResult::OperationSucceeded,
        Err(error) => SubmissionResult::OperationFailed(error.to_string()),
    }
}

fn spawn_session_operation(
    commands: SessionCommandClient,
    operation: SessionOperation,
    results: UnboundedSender<SubmissionResult>,
) {
    tokio::spawn(async move {
        let result = operation.run(commands).await;
        let _ = results.send(result);
    });
}

struct ConnectedSessionSubscription {
    session_id: SessionId,
    subscription: SessionSubscription,
}

fn spawn_session_subscription(
    commands: SessionCommandClient,
    session_id: SessionId,
    connected: UnboundedSender<ConnectedSessionSubscription>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut retry_in = tokio::time::Duration::from_millis(50);
        loop {
            if connected.is_closed() {
                return;
            }
            match commands.subscribe_session(session_id).await {
                Ok(subscription) => {
                    let _ = connected.send(ConnectedSessionSubscription {
                        session_id,
                        subscription,
                    });
                    return;
                }
                Err(_) => {
                    tokio::time::sleep(retry_in).await;
                    retry_in = retry_in
                        .saturating_mul(2)
                        .min(tokio::time::Duration::from_secs(1));
                }
            }
        }
    })
}

/// Re-establishes the Session subscription after the stream dropped, leaving
/// any attempt already in flight to finish rather than restarting it.
fn recover_session_subscription(
    application: &mut Application,
    client: &ManagedClient,
    tasks: &mut SessionTasks,
    channels: &TaskChannels,
) -> Result<()> {
    tasks.subscription = None;
    let transition = application.handle_event(ApplicationEvent::SessionSubscriptionEnded)?;
    if let ApplicationTransition::SubscribeSession(session_id) = transition
        && tasks.subscribing.is_none()
    {
        tasks.subscribing = Some((
            session_id,
            spawn_session_subscription(
                client.session_commands(),
                session_id,
                channels.subscriptions.clone(),
            ),
        ));
    }
    Ok(())
}

async fn next_session_event(
    subscription: &mut Option<SessionSubscription>,
) -> Option<std::result::Result<SessionEvent, SessionStreamError>> {
    match subscription {
        Some(subscription) => subscription.next().await,
        None => pending().await,
    }
}

async fn wait_for_reconnect_grace(grace: &mut Option<Pin<Box<tokio::time::Sleep>>>) {
    match grace {
        Some(grace) => grace.as_mut().await,
        None => pending().await,
    }
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

/// Enables button-press and wheel reporting (1000) with SGR encoding (1006).
/// Deliberately excludes any-motion tracking (1003), which crossterm's
/// `EnableMouseCapture` turns on: motion tracking floods the input stream with
/// pointer-move events nothing in the TUI consumes.
struct EnableMouseButtonReporting;

impl crossterm::Command for EnableMouseButtonReporting {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        f.write_str(concat!("\x1b[?1000h", "\x1b[?1006h"))
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        crossterm::event::EnableMouseCapture.execute_winapi()
    }
}

struct DisableMouseButtonReporting;

impl crossterm::Command for DisableMouseButtonReporting {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        f.write_str(concat!("\x1b[?1006l", "\x1b[?1000l"))
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        crossterm::event::DisableMouseCapture.execute_winapi()
    }
}

fn enable_terminal_features(output: &mut impl std::io::Write) -> std::io::Result<()> {
    execute!(output, EnableBracketedPaste, EnableMouseButtonReporting)?;
    let _ = execute!(
        output,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    );
    Ok(())
}

fn disable_terminal_features(output: &mut impl std::io::Write) -> std::io::Result<()> {
    let _ = execute!(output, PopKeyboardEnhancementFlags);
    execute!(output, DisableMouseButtonReporting, DisableBracketedPaste)
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut output = stdout();
        if let Err(error) = execute!(output, EnterAlternateScreen, Hide) {
            let _ = execute!(output, LeaveAlternateScreen, Show);
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        if let Err(error) = enable_terminal_features(&mut output) {
            let _ = disable_terminal_features(&mut output);
            let _ = execute!(output, LeaveAlternateScreen, Show);
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        match Terminal::new(CrosstermBackend::new(output)) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                let mut output = stdout();
                let _ = disable_terminal_features(&mut output);
                let _ = execute!(output, LeaveAlternateScreen, Show);
                let _ = disable_raw_mode();
                Err(error.into())
            }
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_terminal_features(self.terminal.backend_mut());
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen, Show);
        let _ = disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::{disable_terminal_features, enable_terminal_features};

    #[test]
    fn terminal_input_capabilities_enable_mouse_and_modified_key_reporting() {
        let mut enabled = Vec::new();
        enable_terminal_features(&mut enabled).expect("enable terminal features");
        let enabled = String::from_utf8(enabled).expect("terminal commands are ANSI");
        assert!(
            enabled.contains("\x1b[?1000h"),
            "mouse capture was not enabled"
        );
        assert!(
            enabled.contains("\x1b[>1u"),
            "modified key reporting was not enabled"
        );

        let mut disabled = Vec::new();
        disable_terminal_features(&mut disabled).expect("disable terminal features");
        let disabled = String::from_utf8(disabled).expect("terminal commands are ANSI");
        assert!(
            disabled.contains("\x1b[?1000l"),
            "mouse capture was not disabled"
        );
        assert!(
            disabled.contains("\x1b[<1u"),
            "modified key reporting was not restored"
        );
    }
}
