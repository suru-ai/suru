//! Terminal lifecycle and the async run loop: the terminal session guard, the
//! `tokio::select!` loop that feeds the Application, and the tasks it spawns to
//! carry out the transitions the Application returns.

use std::{
    future::{Future, pending},
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
        ModelCatalog, PromptId, SessionId, SessionListItem, SessionSnapshot, SettingMutation,
        SettingsSnapshot, TurnId, UpdateAgentSelectionRequest,
    },
};
use anyhow::{Result, anyhow};
use crossterm::{
    cursor::{Hide, Show},
    event::{DisableBracketedPaste, EnableBracketedPaste, Event as InputEvent, EventStream},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::sync::mpsc::UnboundedSender;

use super::spinner;
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

/// How the run loop leaves the screen when an event ends the run.
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
    /// Drops the live subscription, leaving any attempt to establish a new one
    /// running: the Session itself is still current.
    fn end_subscription(&mut self) {
        self.subscription = None;
    }

    /// Drops the live subscription along with any attempt to re-establish it,
    /// so nothing reconnects to a Session left behind.
    fn detach(&mut self) {
        self.end_subscription();
        self.abort_subscribing();
    }

    fn abort_subscribing(&mut self) {
        if let Some((_, task)) = self.subscribing.take() {
            task.abort();
        }
    }

    /// Takes over a subscription a spawned task established.
    fn adopt(&mut self, subscription: SessionSubscription) {
        self.subscription = Some(subscription);
    }

    /// Subscribes to `session_id`, abandoning any subscription attempt already
    /// in flight for an earlier Session.
    fn resubscribe(
        &mut self,
        commands: SessionCommandClient,
        session_id: SessionId,
        connected: &UnboundedSender<ConnectedSessionSubscription>,
    ) {
        self.abort_subscribing();
        self.spawn_subscribe(commands, session_id, connected);
    }

    /// Subscribes to `session_id` only when no attempt is already in flight, so
    /// stream recovery never restarts a connection that is still retrying.
    fn subscribe_if_idle(
        &mut self,
        commands: SessionCommandClient,
        session_id: SessionId,
        connected: &UnboundedSender<ConnectedSessionSubscription>,
    ) {
        if self.subscribing.is_none() {
            self.spawn_subscribe(commands, session_id, connected);
        }
    }

    fn spawn_subscribe(
        &mut self,
        commands: SessionCommandClient,
        session_id: SessionId,
        connected: &UnboundedSender<ConnectedSessionSubscription>,
    ) {
        self.subscribing = Some((
            session_id,
            spawn_session_subscription(commands, session_id, connected.clone()),
        ));
    }

    /// Forgets the subscription attempt for `session_id` now that it connected.
    fn finish_subscribing(&mut self, session_id: SessionId) {
        if self
            .subscribing
            .as_ref()
            .is_some_and(|(subscribing, _)| *subscribing == session_id)
        {
            self.subscribing = None;
        }
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

    fn finish_attaching(&mut self) {
        self.attaching = None;
    }

    fn finish_listing_sessions(&mut self, request: &SessionListRequest) {
        finish_listing(&mut self.listing_sessions, request);
    }

    fn finish_listing_models(&mut self, request: &ModelListRequest) {
        finish_listing(&mut self.listing_models, request);
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

/// The run loop's mutable world: the Application it feeds, the client it sends
/// Session commands through, the Session work it owns, and the channels its
/// spawned tasks report back on.
struct RunLoop {
    client: ManagedClient,
    application: Application,
    tasks: SessionTasks,
    channels: TaskChannels,
    reconnect_grace: Option<Pin<Box<tokio::time::Sleep>>>,
    /// Armed only while something on screen animates a Spinner, so an idle
    /// TUI schedules zero wakeups (ADR 0009). Re-armed on every fire.
    spinner_tick: Option<Pin<Box<tokio::time::Sleep>>>,
    /// Set by anything that changes what is on screen, so an event the user
    /// cannot see costs no frame.
    needs_redraw: bool,
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    client: ManagedClient,
    workspace: PathBuf,
) -> Result<()> {
    let (submissions, mut submission_rx) = tokio::sync::mpsc::unbounded_channel();
    let (subscriptions, mut subscription_rx) = tokio::sync::mpsc::unbounded_channel();
    let (pickers, mut picker_rx) = tokio::sync::mpsc::unbounded_channel();
    let (models, mut model_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut run = RunLoop {
        client,
        application: Application::new(workspace),
        tasks: SessionTasks::default(),
        channels: TaskChannels {
            submissions,
            subscriptions,
            pickers,
            models,
        },
        reconnect_grace: None,
        spinner_tick: None,
        needs_redraw: true,
    };
    let mut input = EventStream::new();

    loop {
        run.sync_spinner_tick();
        if run.needs_redraw {
            terminal.draw(|frame| run.application.render(frame))?;
            run.needs_redraw = false;
        }
        // Every arm reports through ControlFlow so the two events that can end
        // the run -- a Provider shutdown and the exit command -- leave by the
        // same path as the input stream closing.
        let step = tokio::select! {
            managed_event = run.client.next() => run.receive_managed_event(managed_event)?,
            () = wait_for_reconnect_grace(&mut run.reconnect_grace) => {
                run.expire_reconnect_grace()?
            }
            () = wait_for_spinner_tick(&mut run.spinner_tick) => run.advance_spinner(),
            session_event = next_session_event(&mut run.tasks.subscription) => {
                run.receive_session_event(session_event)?
            }
            connected = subscription_rx.recv() => run.receive_subscription(connected)?,
            submission = submission_rx.recv() => run.receive_submission(submission)?,
            model = model_rx.recv() => run.receive_model_listing(model)?,
            picker = picker_rx.recv() => run.receive_session_picker(picker)?,
            input_event = input.next() => match input_event {
                Some(Ok(event)) => run.handle_input_event(event)?,
                Some(Err(error)) => return Err(error.into()),
                None => ControlFlow::Break(Exit::Now),
            },
        };
        if let ControlFlow::Break(exit) = step {
            return leave_run_loop(terminal, &run.application, exit);
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
                Some(Ok(event)) => run.handle_input_event(event)?,
                Some(Err(error)) => return Err(error.into()),
                None => ControlFlow::Break(Exit::Now),
            };
            if let ControlFlow::Break(exit) = step {
                return leave_run_loop(terminal, &run.application, exit);
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

impl RunLoop {
    fn handle_input_event(&mut self, event: InputEvent) -> Result<ControlFlow<Exit>> {
        if matches!(event, InputEvent::Resize(..)) {
            self.needs_redraw = true;
        }
        if self.application.note_interaction(&event) {
            self.needs_redraw = true;
        }
        let Some(command) = self.application.command_for_terminal_input(event) else {
            return Ok(ControlFlow::Continue(()));
        };
        self.needs_redraw = true;
        let transition = self
            .application
            .handle_event(ApplicationEvent::Command(command))?;
        Ok(self.dispatch_transition(transition))
    }

    /// Carries out the transition a terminal command produced, spawning
    /// whatever Session command it asked for.
    fn dispatch_transition(&mut self, transition: ApplicationTransition) -> ControlFlow<Exit> {
        match transition {
            ApplicationTransition::Continue => {}
            ApplicationTransition::Exit => return ControlFlow::Break(Exit::Now),
            ApplicationTransition::SessionEnded => self.tasks.end_subscription(),
            ApplicationTransition::DetachSession => self.tasks.detach(),
            ApplicationTransition::CreateSession(request) => {
                spawn_session_creation(
                    self.client.session_commands(),
                    request,
                    self.channels.submissions.clone(),
                );
            }
            ApplicationTransition::AdmitPrompt {
                session_id,
                request,
            } => {
                spawn_prompt_admission(
                    self.client.session_commands(),
                    session_id,
                    request,
                    self.channels.submissions.clone(),
                );
            }
            ApplicationTransition::PromotePrompt {
                session_id,
                prompt_id,
            } => self.spawn_operation(SessionOperation::PromotePrompt {
                session_id,
                prompt_id,
            }),
            ApplicationTransition::CancelPrompt {
                session_id,
                prompt_id,
            } => self.spawn_operation(SessionOperation::CancelPrompt {
                session_id,
                prompt_id,
            }),
            ApplicationTransition::InterruptTurn {
                session_id,
                turn_id,
            } => self.spawn_operation(SessionOperation::InterruptTurn {
                session_id,
                turn_id,
            }),
            ApplicationTransition::DeleteSession(session_id) => {
                self.spawn_operation(SessionOperation::DeleteSession { session_id });
            }
            ApplicationTransition::SubscribeSession(_) => {
                unreachable!("terminal input cannot end a Session subscription")
            }
            ApplicationTransition::AttachSession(session_id) => {
                self.tasks.attach(
                    self.client.session_commands(),
                    session_id,
                    &self.channels.pickers,
                );
            }
            ApplicationTransition::ListSessions(request) => self.list_sessions(request),
            ApplicationTransition::ListModels(request) => {
                self.tasks.list_models(
                    self.client.session_commands(),
                    request,
                    &self.channels.models,
                );
            }
            ApplicationTransition::ConfirmLandingAgentSelection(selection) => {
                spawn_landing_agent_selection_confirmation(
                    self.client.session_commands(),
                    selection,
                    self.channels.submissions.clone(),
                );
            }
            ApplicationTransition::UpdateAgentSelection {
                session_id,
                request,
            } => {
                spawn_agent_selection_update(
                    self.client.session_commands(),
                    session_id,
                    request,
                    self.channels.submissions.clone(),
                );
            }
            ApplicationTransition::MutateSetting(mutation) => {
                spawn_setting_mutation(
                    self.client.session_commands(),
                    mutation,
                    self.channels.submissions.clone(),
                );
            }
        }
        ControlFlow::Continue(())
    }

    fn spawn_operation(&self, operation: SessionOperation) {
        spawn_session_operation(
            self.client.session_commands(),
            operation,
            self.channels.submissions.clone(),
        );
    }

    fn list_sessions(&mut self, request: SessionListRequest) {
        self.tasks.list_sessions(
            self.client.session_commands(),
            request,
            &self.channels.pickers,
        );
    }

    fn receive_managed_event(&mut self, event: Option<ManagedEvent>) -> Result<ControlFlow<Exit>> {
        self.needs_redraw = true;
        let event = event.ok_or_else(|| anyhow!("managed client stopped unexpectedly"))?;
        let was_recovering = self.application.is_recovering();
        let transition = self
            .application
            .handle_event(ApplicationEvent::Managed(event))?;
        if self.application.is_recovering() {
            if !was_recovering {
                self.reconnect_grace = Some(Box::pin(tokio::time::sleep(RECONNECT_GRACE_PERIOD)));
            }
        } else {
            self.reconnect_grace = None;
        }
        match transition {
            ApplicationTransition::Continue => {}
            ApplicationTransition::SessionEnded => self.tasks.end_subscription(),
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
            | ApplicationTransition::UpdateAgentSelection { .. }
            | ApplicationTransition::MutateSetting(_) => {
                unreachable!("managed events do not issue Session commands");
            }
        }
        if self.application.session_id().is_none() {
            self.tasks.detach();
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Arms the Spinner tick while anything on screen animates and drops it
    /// the moment nothing does, keeping the run loop idle-by-default. Called
    /// once per loop iteration, so every event that starts or settles work
    /// re-decides the tick before the frame it changed draws.
    fn sync_spinner_tick(&mut self) {
        if self.application.wants_spinner() {
            if self.spinner_tick.is_none() {
                self.spinner_tick = Some(Box::pin(tokio::time::sleep(spinner::TICK_PERIOD)));
            }
        } else {
            self.spinner_tick = None;
        }
    }

    fn advance_spinner(&mut self) -> ControlFlow<Exit> {
        self.needs_redraw = true;
        self.application.advance_spinner();
        self.spinner_tick = Some(Box::pin(tokio::time::sleep(spinner::TICK_PERIOD)));
        ControlFlow::Continue(())
    }

    fn expire_reconnect_grace(&mut self) -> Result<ControlFlow<Exit>> {
        self.needs_redraw = true;
        self.application
            .handle_event(ApplicationEvent::ReconnectGraceElapsed)?;
        self.reconnect_grace = None;
        Ok(ControlFlow::Continue(()))
    }

    fn receive_session_event(
        &mut self,
        event: Option<std::result::Result<SessionEvent, SessionStreamError>>,
    ) -> Result<ControlFlow<Exit>> {
        self.needs_redraw = true;
        match event {
            Some(Ok(event)) => {
                self.application
                    .handle_event(ApplicationEvent::Session(event))?;
            }
            Some(Err(error)) if !error.is_recoverable() => return Err(error.into()),
            Some(Err(_)) | None => self.recover_session_subscription()?,
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Re-establishes the Session subscription after the stream dropped,
    /// leaving any attempt already in flight to finish rather than restarting.
    fn recover_session_subscription(&mut self) -> Result<()> {
        self.tasks.end_subscription();
        let transition = self
            .application
            .handle_event(ApplicationEvent::SessionSubscriptionEnded)?;
        if let ApplicationTransition::SubscribeSession(session_id) = transition {
            self.tasks.subscribe_if_idle(
                self.client.session_commands(),
                session_id,
                &self.channels.subscriptions,
            );
        }
        Ok(())
    }

    /// Adopts a subscription a spawned task established. Nothing on screen
    /// changes, so this is the one event that does not ask for a redraw.
    fn receive_subscription(
        &mut self,
        connected: Option<ConnectedSessionSubscription>,
    ) -> Result<ControlFlow<Exit>> {
        let connected = connected
            .ok_or_else(|| anyhow!("Session subscription task channel stopped unexpectedly"))?;
        self.tasks.finish_subscribing(connected.session_id);
        if self.application.session_id() == Some(connected.session_id) {
            self.tasks.adopt(connected.subscription);
        }
        Ok(ControlFlow::Continue(()))
    }

    fn receive_submission(
        &mut self,
        submission: Option<SubmissionResult>,
    ) -> Result<ControlFlow<Exit>> {
        self.needs_redraw = true;
        let submission = submission
            .ok_or_else(|| anyhow!("Prompt admission task channel stopped unexpectedly"))?;
        match submission {
            SubmissionResult::SessionCreated(created) => {
                let session_id = created.session.id;
                self.application
                    .handle_event(ApplicationEvent::SessionCreated(*created))?;
                self.tasks.resubscribe(
                    self.client.session_commands(),
                    session_id,
                    &self.channels.subscriptions,
                );
            }
            SubmissionResult::PromptAdmitted(prompt_id) => {
                self.application
                    .handle_event(ApplicationEvent::PromptAdmissionSucceeded(prompt_id))?;
            }
            SubmissionResult::Failed { prompt_id, error } => {
                self.application
                    .handle_event(ApplicationEvent::PromptAdmissionFailed { prompt_id, error })?;
            }
            SubmissionResult::OperationSucceeded => {}
            SubmissionResult::OperationFailed(error) => {
                self.application
                    .handle_event(ApplicationEvent::SessionOperationFailed(error))?;
            }
            SubmissionResult::SessionDeletionFailed { session_id, error } => {
                self.application
                    .handle_event(ApplicationEvent::SessionDeletionFailed { session_id, error })?;
            }
            SubmissionResult::LandingAgentSelectionConfirmed(selection) => {
                let transition = self
                    .application
                    .handle_event(ApplicationEvent::LandingAgentSelectionConfirmed(selection))?;
                self.flush_landing_agent_selection(transition);
            }
            SubmissionResult::LandingAgentSelectionConfirmationFailed(error) => {
                let transition = self.application.handle_event(
                    ApplicationEvent::LandingAgentSelectionConfirmationFailed(error),
                )?;
                self.flush_landing_agent_selection(transition);
            }
            SubmissionResult::AgentSelectionUpdated {
                operation_id,
                selection,
            } => {
                let transition =
                    self.application
                        .handle_event(ApplicationEvent::AgentSelectionUpdated {
                            operation_id,
                            selection,
                        })?;
                self.flush_agent_selection(transition);
            }
            SubmissionResult::AgentSelectionUpdateFailed {
                operation_id,
                error,
            } => {
                let transition = self.application.handle_event(
                    ApplicationEvent::AgentSelectionUpdateFailed {
                        operation_id,
                        error,
                    },
                )?;
                self.flush_agent_selection(transition);
            }
            SubmissionResult::SettingMutated(snapshot) => {
                self.application
                    .handle_event(ApplicationEvent::SettingMutated(*snapshot))?;
            }
            SubmissionResult::SettingMutationFailed(error) => {
                self.application
                    .handle_event(ApplicationEvent::SettingMutationFailed(error))?;
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Dispatches the follow-up request when settling one Agent Selection
    /// operation released a coalesced newer selection.
    fn flush_agent_selection(&self, transition: ApplicationTransition) {
        if let ApplicationTransition::UpdateAgentSelection {
            session_id,
            request,
        } = transition
        {
            spawn_agent_selection_update(
                self.client.session_commands(),
                session_id,
                request,
                self.channels.submissions.clone(),
            );
        }
    }

    fn flush_landing_agent_selection(&self, transition: ApplicationTransition) {
        if let ApplicationTransition::ConfirmLandingAgentSelection(selection) = transition {
            spawn_landing_agent_selection_confirmation(
                self.client.session_commands(),
                selection,
                self.channels.submissions.clone(),
            );
        }
    }

    fn receive_model_listing(
        &mut self,
        result: Option<ModelPickerResult>,
    ) -> Result<ControlFlow<Exit>> {
        self.needs_redraw = true;
        let result =
            result.ok_or_else(|| anyhow!("Model picker task channel stopped unexpectedly"))?;
        match result {
            ModelPickerResult::Listed { request, catalog } => {
                self.application
                    .handle_event(ApplicationEvent::ModelsListed { request, catalog })?;
            }
            ModelPickerResult::Refreshed { request, catalog } => {
                self.tasks.finish_listing_models(&request);
                self.application
                    .handle_event(ApplicationEvent::ModelsRefreshed { request, catalog })?;
            }
            ModelPickerResult::Failed { request, error } => {
                self.tasks.finish_listing_models(&request);
                self.application
                    .handle_event(ApplicationEvent::ModelListingFailed { request, error })?;
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    fn receive_session_picker(
        &mut self,
        result: Option<SessionPickerResult>,
    ) -> Result<ControlFlow<Exit>> {
        self.needs_redraw = true;
        let result =
            result.ok_or_else(|| anyhow!("Session picker task channel stopped unexpectedly"))?;
        match result {
            SessionPickerResult::Listed { request, sessions } => {
                self.tasks.finish_listing_sessions(&request);
                self.application
                    .handle_event(ApplicationEvent::SessionsListed { request, sessions })?;
            }
            SessionPickerResult::ListingFailed { request, error } => {
                self.tasks.finish_listing_sessions(&request);
                self.application
                    .handle_event(ApplicationEvent::SessionListingFailed { request, error })?;
            }
            SessionPickerResult::Attached {
                snapshot,
                subscription,
            } => {
                self.tasks.finish_attaching();
                self.application
                    .handle_event(ApplicationEvent::SessionAttached(*snapshot))?;
                self.tasks.adopt(subscription);
                self.tasks.abort_subscribing();
            }
            SessionPickerResult::AttachmentFailed(error) => {
                self.tasks.finish_attaching();
                let transition = self
                    .application
                    .handle_event(ApplicationEvent::SessionAttachmentFailed(error))?;
                if let ApplicationTransition::ListSessions(request) = transition {
                    self.list_sessions(request);
                }
            }
        }
        Ok(ControlFlow::Continue(()))
    }
}

fn spawn_session_creation(
    commands: SessionCommandClient,
    request: CreateSessionRequest,
    results: UnboundedSender<SubmissionResult>,
) {
    let prompt_id = request.prompt.id;
    spawn_prompt_delivery(prompt_id, results, async move {
        let created = commands.create_session(request).await?;
        Ok(SubmissionResult::SessionCreated(Box::new(created)))
    });
}

fn spawn_prompt_admission(
    commands: SessionCommandClient,
    session_id: SessionId,
    request: AdmitPromptRequest,
    results: UnboundedSender<SubmissionResult>,
) {
    let prompt_id = request.prompt.id;
    spawn_prompt_delivery(prompt_id, results, async move {
        let prompt = commands.admit_prompt(session_id, request).await?;
        Ok(SubmissionResult::PromptAdmitted(prompt.id))
    });
}

/// Delivers a Prompt, reporting any transport failure against `prompt_id` so
/// the composer can restore the text the user submitted.
fn spawn_prompt_delivery(
    prompt_id: PromptId,
    results: UnboundedSender<SubmissionResult>,
    deliver: impl Future<Output = Result<SubmissionResult>> + Send + 'static,
) {
    tokio::spawn(async move {
        let result = deliver
            .await
            .unwrap_or_else(|error| SubmissionResult::Failed {
                prompt_id,
                error: error.to_string(),
            });
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
    SettingMutated(Box<SettingsSnapshot>),
    SettingMutationFailed(String),
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

/// Sends one Setting's typed edit to the server, which owns the Config
/// Document, and brings back the effective settings the edit left in force.
fn spawn_setting_mutation(
    commands: SessionCommandClient,
    mutation: SettingMutation,
    results: UnboundedSender<SubmissionResult>,
) {
    tokio::spawn(async move {
        let result = match commands.mutate_setting(mutation).await {
            Ok(snapshot) => SubmissionResult::SettingMutated(Box::new(snapshot)),
            Err(error) => SubmissionResult::SettingMutationFailed(error.to_string()),
        };
        let _ = results.send(result);
    });
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

async fn wait_for_spinner_tick(tick: &mut Option<Pin<Box<tokio::time::Sleep>>>) {
    match tick {
        Some(tick) => tick.as_mut().await,
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

    /// A Windows console hands mouse input to the program as console records
    /// rather than as the ANSI replies this sequence asks for, and it only does
    /// so once quick-edit selection is off — otherwise the console keeps the
    /// clicks for its own text selection. Nothing written to the output stream
    /// turns quick-edit off, so Windows always takes the console API path. That
    /// path has no dial for motion tracking, so the motion records it delivers
    /// are dropped later, when `command_for_terminal_event` maps the event.
    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        false
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

    /// Restores the console mode [`EnableMouseButtonReporting`] replaced, so it
    /// has to take the same console API path.
    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        false
    }
}

/// Asks for modified-key disambiguation, the first level of the kitty keyboard
/// protocol. crossterm's own `PushKeyboardEnhancementFlags` reports the escape
/// sequence as unsupported on Windows and fails outright there, so this writes
/// the sequence itself; terminals that do not implement the protocol ignore it.
struct PushModifiedKeyReporting;

impl crossterm::Command for PushModifiedKeyReporting {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        f.write_str("\x1b[>1u")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        crossterm::event::PushKeyboardEnhancementFlags(
            crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
        )
        .execute_winapi()
    }
}

struct PopModifiedKeyReporting;

impl crossterm::Command for PopModifiedKeyReporting {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        f.write_str("\x1b[<1u")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        crossterm::event::PopKeyboardEnhancementFlags.execute_winapi()
    }
}

/// Terminal features are progressive: a terminal that does not implement one
/// ignores its escape sequence, and the legacy Windows console API answers
/// `Unsupported` for the features it has no equivalent of. Neither is a reason
/// to refuse to draw the TUI, while a terminal that has gone away still is.
fn ignore_unsupported(result: std::io::Result<()>) -> std::io::Result<()> {
    match result {
        Err(error) if error.kind() == std::io::ErrorKind::Unsupported => Ok(()),
        result => result,
    }
}

fn enable_terminal_features(output: &mut impl std::io::Write) -> std::io::Result<()> {
    ignore_unsupported(execute!(output, EnableBracketedPaste))?;
    // Mouse reporting is the one feature with no graceful degradation: a TUI
    // that cannot read clicks or the wheel is worth refusing to start.
    execute!(output, EnableMouseButtonReporting)?;
    ignore_unsupported(execute!(output, PushModifiedKeyReporting))
}

/// Every restore is attempted even after one of them fails: leaving the terminal
/// in mouse capture is worse than a restore whose error nobody could act on. The
/// first failure is the one reported.
fn disable_terminal_features(output: &mut impl std::io::Write) -> std::io::Result<()> {
    let modified_keys = ignore_unsupported(execute!(output, PopModifiedKeyReporting));
    let mouse = execute!(output, DisableMouseButtonReporting);
    let paste = ignore_unsupported(execute!(output, DisableBracketedPaste));
    modified_keys.and(mouse).and(paste)
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
        if let Err(error) = disable_terminal_features(self.terminal.backend_mut()) {
            tracing::warn!("could not disable terminal features: {error}");
        }
        if let Err(error) = execute!(self.terminal.backend_mut(), LeaveAlternateScreen, Show) {
            tracing::warn!("could not leave the alternate screen: {error}");
        }
        if let Err(error) = disable_raw_mode() {
            tracing::warn!("could not disable raw mode: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::Command;

    use super::{
        DisableMouseButtonReporting, EnableMouseButtonReporting, PopModifiedKeyReporting,
        PushModifiedKeyReporting, ignore_unsupported,
    };

    /// Pins the sequence an ANSI terminal receives. Windows takes the console
    /// API for mouse reporting instead, which the sibling tests cover.
    #[test]
    fn terminal_input_capabilities_enable_mouse_and_modified_key_reporting() {
        let mut enabled = String::new();
        EnableMouseButtonReporting
            .write_ansi(&mut enabled)
            .expect("format mouse reporting command");
        PushModifiedKeyReporting
            .write_ansi(&mut enabled)
            .expect("format modified key reporting command");
        assert!(
            enabled.contains("\x1b[?1000h"),
            "mouse capture was not enabled"
        );
        assert!(
            enabled.contains("\x1b[>1u"),
            "modified key reporting was not enabled"
        );

        let mut disabled = String::new();
        PopModifiedKeyReporting
            .write_ansi(&mut disabled)
            .expect("format modified key restoration command");
        DisableMouseButtonReporting
            .write_ansi(&mut disabled)
            .expect("format mouse reporting restoration command");
        assert!(
            disabled.contains("\x1b[?1000l"),
            "mouse capture was not disabled"
        );
        assert!(
            disabled.contains("\x1b[<1u"),
            "modified key reporting was not restored"
        );
    }

    #[test]
    fn a_feature_the_terminal_cannot_support_is_not_fatal() {
        let unsupported =
            std::io::Error::new(std::io::ErrorKind::Unsupported, "no keyboard enhancement");
        ignore_unsupported(Err(unsupported)).expect("an unsupported feature is not an error");
    }

    #[test]
    fn a_failure_other_than_an_unsupported_feature_still_propagates() {
        let broken = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "the terminal went away");
        let error = ignore_unsupported(Err(broken)).expect_err("a broken terminal is fatal");
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }

    /// Windows reports mouse input as console records rather than the ANSI
    /// replies the escape sequence asks for, so the escape sequence alone leaves
    /// the TUI blind to clicks.
    #[cfg(windows)]
    #[test]
    fn mouse_reporting_uses_the_windows_console_api() {
        assert!(
            !EnableMouseButtonReporting.is_ansi_code_supported(),
            "enabling mouse reporting bypassed the Windows console API"
        );
        assert!(
            !DisableMouseButtonReporting.is_ansi_code_supported(),
            "disabling mouse reporting bypassed the Windows console API"
        );
    }

    /// crossterm forces its own `PushKeyboardEnhancementFlags` onto the Windows
    /// console API, where the flags have no equivalent and the call can only
    /// fail. Modified-key reporting has to keep asking the terminal instead.
    #[cfg(windows)]
    #[test]
    fn modified_key_reporting_follows_the_terminals_own_ansi_support() {
        let ansi = crossterm::ansi_support::supports_ansi();
        assert_eq!(
            PushModifiedKeyReporting.is_ansi_code_supported(),
            ansi,
            "enabling modified key reporting ignored the terminal's ANSI support"
        );
        assert_eq!(
            PopModifiedKeyReporting.is_ansi_code_supported(),
            ansi,
            "restoring modified key reporting ignored the terminal's ANSI support"
        );
    }
}
