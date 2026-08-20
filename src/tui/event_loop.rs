//! Terminal lifecycle and the async run loop: the terminal session guard, the
//! `tokio::select!` loop that feeds the Application, and the tasks it spawns to
//! carry out the transitions the Application returns.

use std::{
    future::pending,
    io::{Stdout, stdout},
    path::PathBuf,
    pin::Pin,
    time::Duration,
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
use crate::{
    managed_client::{
        ManagedClient, SessionCommandClient, SessionEvent, SessionStreamError, SessionSubscription,
    },
    protocol::{
        AgentSelection, AgentSelectionOperationId, ModelCatalog, PromptId, SessionId,
        SessionListItem, SessionSnapshot, TurnId, UpdateAgentSelectionRequest,
    },
};

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

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut client: ManagedClient,
    workspace: PathBuf,
) -> Result<()> {
    let mut application = Application::new(workspace);
    let mut input = EventStream::new();
    let mut session_subscription: Option<SessionSubscription> = None;
    let mut session_subscription_task: Option<(SessionId, tokio::task::JoinHandle<()>)> = None;
    let mut reconnect_grace: Option<Pin<Box<tokio::time::Sleep>>> = None;
    let (submission_tx, mut submission_rx) = tokio::sync::mpsc::unbounded_channel();
    let (subscription_tx, mut subscription_rx) = tokio::sync::mpsc::unbounded_channel();
    let (picker_tx, mut picker_rx) = tokio::sync::mpsc::unbounded_channel();
    let (model_tx, mut model_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut session_list_task: Option<(SessionListRequest, tokio::task::JoinHandle<()>)> = None;
    let mut session_attachment_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut model_list_task: Option<(ModelListRequest, tokio::task::JoinHandle<()>)> = None;
    let mut needs_redraw = true;

    // Shared between the select arm and the drain loop below; a macro so the
    // body can borrow the run loop's state and use `?`/`return` directly.
    macro_rules! process_input_event {
        ($event:expr) => {{
            let event = $event;
            if matches!(event, InputEvent::Resize(..)) {
                needs_redraw = true;
            }
            if let Some(command) = application.command_for_terminal_input(event) {
                needs_redraw = true;
                let transition = application.handle_event(ApplicationEvent::Command(command))?;
                match transition {
                    ApplicationTransition::Continue => {}
                    ApplicationTransition::SessionEnded => {
                        session_subscription = None;
                    }
                    ApplicationTransition::DetachSession => {
                        session_subscription = None;
                        if let Some((_, task)) = session_subscription_task.take() {
                            task.abort();
                        }
                    }
                    ApplicationTransition::Exit => return Ok(()),
                    ApplicationTransition::CreateSession(request) => {
                        let prompt_id = request.prompt.id;
                        let commands = client.session_commands();
                        let results = submission_tx.clone();
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
                    ApplicationTransition::AdmitPrompt {
                        session_id,
                        request,
                    } => {
                        let prompt_id = request.prompt.id;
                        let commands = client.session_commands();
                        let results = submission_tx.clone();
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
                            submission_tx.clone(),
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
                            submission_tx.clone(),
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
                            submission_tx.clone(),
                        );
                    }
                    ApplicationTransition::DeleteSession(session_id) => {
                        spawn_session_operation(
                            client.session_commands(),
                            SessionOperation::DeleteSession { session_id },
                            submission_tx.clone(),
                        );
                    }
                    ApplicationTransition::SubscribeSession(_) => {
                        unreachable!("terminal input cannot end a Session subscription")
                    }
                    ApplicationTransition::AttachSession(session_id) => {
                        if session_attachment_task.is_none() {
                            session_attachment_task = Some(spawn_session_attachment(
                                client.session_commands(),
                                session_id,
                                picker_tx.clone(),
                            ));
                        }
                    }
                    ApplicationTransition::ListSessions(request) => {
                        replace_session_listing(
                            &mut session_list_task,
                            client.session_commands(),
                            request,
                            picker_tx.clone(),
                        );
                    }
                    ApplicationTransition::ListModels(request) => {
                        replace_listing(&mut model_list_task, request, |request| {
                            spawn_model_listing(
                                client.session_commands(),
                                request,
                                model_tx.clone(),
                            )
                        });
                    }
                    ApplicationTransition::ConfirmLandingAgentSelection(selection) => {
                        spawn_landing_agent_selection_confirmation(
                            client.session_commands(),
                            selection,
                            submission_tx.clone(),
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
                            submission_tx.clone(),
                        );
                    }
                }
            }
        }};
    }

    loop {
        if needs_redraw {
            terminal.draw(|frame| application.render(frame))?;
            needs_redraw = false;
        }
        tokio::select! {
            managed_event = client.next() => {
                needs_redraw = true;
                match managed_event {
                    Some(event) => {
                        let was_recovering = application.is_recovering();
                        let transition = application
                            .handle_event(ApplicationEvent::Managed(event))?;
                        let is_recovering = application.is_recovering();
                        if !was_recovering && is_recovering {
                            reconnect_grace = Some(Box::pin(tokio::time::sleep(
                                RECONNECT_GRACE_PERIOD,
                            )));
                        } else if !is_recovering {
                            reconnect_grace = None;
                        }
                        match transition {
                            ApplicationTransition::Continue => {}
                            ApplicationTransition::SessionEnded => {
                                session_subscription = None;
                            }
                            ApplicationTransition::Exit => {
                                terminal.draw(|frame| application.render(frame))?;
                                return Ok(());
                            }
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
                            session_subscription = None;
                            if let Some((_, task)) = session_subscription_task.take() {
                                task.abort();
                            }
                        }
                    }
                    None => return Err(anyhow!("managed client stopped unexpectedly")),
                }
            }
            _ = wait_for_reconnect_grace(&mut reconnect_grace) => {
                needs_redraw = true;
                application.handle_event(ApplicationEvent::ReconnectGraceElapsed)?;
                reconnect_grace = None;
            }
            session_event = next_session_event(&mut session_subscription) => {
                needs_redraw = true;
                match session_event {
                    Some(Ok(event)) => {
                        application.handle_event(ApplicationEvent::Session(event))?;
                    }
                    Some(Err(error)) if !error.is_recoverable() => return Err(error.into()),
                    Some(Err(_)) | None => {
                        recover_session_subscription(
                            &mut application,
                            &mut session_subscription,
                            &mut session_subscription_task,
                            client.session_commands(),
                            &subscription_tx,
                        )?;
                    }
                }
            }
            connected = subscription_rx.recv() => {
                let Some(connected) = connected else {
                    return Err(anyhow!("Session subscription task channel stopped unexpectedly"));
                };
                if session_subscription_task
                    .as_ref()
                    .is_some_and(|(session_id, _)| *session_id == connected.session_id)
                {
                    session_subscription_task = None;
                }
                if application.session_id() == Some(connected.session_id) {
                    session_subscription = Some(connected.subscription);
                }
            }
            submission = submission_rx.recv() => {
                needs_redraw = true;
                let Some(submission) = submission else {
                    return Err(anyhow!("Prompt admission task channel stopped unexpectedly"));
                };
                match submission {
                    SubmissionResult::SessionCreated(created) => {
                        let session_id = created.session.id;
                        application.handle_event(ApplicationEvent::SessionCreated(*created))?;
                        if let Some((_, task)) = session_subscription_task.take() {
                            task.abort();
                        }
                        session_subscription_task = Some((
                            session_id,
                            spawn_session_subscription(
                                client.session_commands(),
                                session_id,
                                subscription_tx.clone(),
                            ),
                        ));
                    }
                    SubmissionResult::PromptAdmitted(prompt_id) => {
                        application.handle_event(
                            ApplicationEvent::PromptAdmissionSucceeded(prompt_id),
                        )?;
                    }
                    SubmissionResult::Failed { prompt_id, error } => {
                        application.handle_event(ApplicationEvent::PromptAdmissionFailed {
                            prompt_id,
                            error,
                        })?;
                    }
                    SubmissionResult::OperationSucceeded => {}
                    SubmissionResult::OperationFailed(error) => {
                        application.handle_event(ApplicationEvent::SessionOperationFailed(error))?;
                    }
                    SubmissionResult::SessionDeletionFailed { session_id, error } => {
                        application.handle_event(ApplicationEvent::SessionDeletionFailed {
                            session_id,
                            error,
                        })?;
                    }
                    SubmissionResult::LandingAgentSelectionConfirmed(selection) => {
                        let transition = application.handle_event(
                            ApplicationEvent::LandingAgentSelectionConfirmed(selection),
                        )?;
                        flush_landing_agent_selection(&client, transition, &submission_tx);
                    }
                    SubmissionResult::LandingAgentSelectionConfirmationFailed(error) => {
                        let transition = application.handle_event(
                            ApplicationEvent::LandingAgentSelectionConfirmationFailed(error),
                        )?;
                        flush_landing_agent_selection(&client, transition, &submission_tx);
                    }
                    SubmissionResult::AgentSelectionUpdated {
                        operation_id,
                        selection,
                    } => {
                        let transition = application.handle_event(
                            ApplicationEvent::AgentSelectionUpdated {
                                operation_id,
                                selection,
                            },
                        )?;
                        flush_agent_selection(&client, transition, &submission_tx);
                    }
                    SubmissionResult::AgentSelectionUpdateFailed {
                        operation_id,
                        error,
                    } => {
                        let transition = application.handle_event(
                            ApplicationEvent::AgentSelectionUpdateFailed {
                                operation_id,
                                error,
                            },
                        )?;
                        flush_agent_selection(&client, transition, &submission_tx);
                    }
                }
            }
            model = model_rx.recv() => {
                needs_redraw = true;
                let Some(model) = model else {
                    return Err(anyhow!("Model picker task channel stopped unexpectedly"));
                };
                match model {
                    ModelPickerResult::Listed { request, catalog } => {
                        application.handle_event(ApplicationEvent::ModelsListed {
                            request,
                            catalog,
                        })?;
                    }
                    ModelPickerResult::Refreshed { request, catalog } => {
                        finish_listing(&mut model_list_task, &request);
                        application.handle_event(ApplicationEvent::ModelsRefreshed {
                            request,
                            catalog,
                        })?;
                    }
                    ModelPickerResult::Failed { request, error } => {
                        finish_listing(&mut model_list_task, &request);
                        application.handle_event(ApplicationEvent::ModelListingFailed {
                            request,
                            error,
                        })?;
                    }
                }
            }
            picker = picker_rx.recv() => {
                needs_redraw = true;
                let Some(picker) = picker else {
                    return Err(anyhow!("Session picker task channel stopped unexpectedly"));
                };
                match picker {
                    SessionPickerResult::Listed { request, sessions } => {
                        finish_listing(&mut session_list_task, &request);
                        application.handle_event(ApplicationEvent::SessionsListed {
                            request,
                            sessions,
                        })?;
                    }
                    SessionPickerResult::ListingFailed { request, error } => {
                        finish_listing(&mut session_list_task, &request);
                        application.handle_event(ApplicationEvent::SessionListingFailed {
                            request,
                            error,
                        })?;
                    }
                    SessionPickerResult::Attached {
                        snapshot,
                        subscription,
                    } => {
                        session_attachment_task = None;
                        application.handle_event(ApplicationEvent::SessionAttached(*snapshot))?;
                        session_subscription = Some(subscription);
                        if let Some((_, task)) = session_subscription_task.take() {
                            task.abort();
                        }
                    }
                    SessionPickerResult::AttachmentFailed(error) => {
                        session_attachment_task = None;
                        let transition = application.handle_event(
                            ApplicationEvent::SessionAttachmentFailed(error),
                        )?;
                        if let ApplicationTransition::ListSessions(request) = transition {
                            replace_session_listing(
                                &mut session_list_task,
                                client.session_commands(),
                                request,
                                picker_tx.clone(),
                            );
                        }
                    }
                }
            }
            input_event = input.next() => {
                match input_event {
                    Some(Ok(event)) => process_input_event!(event),
                    Some(Err(error)) => return Err(error.into()),
                    None => return Ok(()),
                }
            }
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
            match pending_input {
                Some(Ok(event)) => process_input_event!(event),
                Some(Err(error)) => return Err(error.into()),
                None => return Ok(()),
            }
        }
    }
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
    results: tokio::sync::mpsc::UnboundedSender<ModelPickerResult>,
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
    results: &tokio::sync::mpsc::UnboundedSender<SubmissionResult>,
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
    results: &tokio::sync::mpsc::UnboundedSender<SubmissionResult>,
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
    results: tokio::sync::mpsc::UnboundedSender<SubmissionResult>,
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
    results: tokio::sync::mpsc::UnboundedSender<SubmissionResult>,
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

fn replace_session_listing(
    active: &mut Option<(SessionListRequest, tokio::task::JoinHandle<()>)>,
    commands: SessionCommandClient,
    request: SessionListRequest,
    results: tokio::sync::mpsc::UnboundedSender<SessionPickerResult>,
) {
    replace_listing(active, request, |request| {
        spawn_session_listing(commands, request, results)
    });
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
    results: tokio::sync::mpsc::UnboundedSender<SessionPickerResult>,
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
    results: tokio::sync::mpsc::UnboundedSender<SessionPickerResult>,
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
    results: tokio::sync::mpsc::UnboundedSender<SubmissionResult>,
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
    connected: tokio::sync::mpsc::UnboundedSender<ConnectedSessionSubscription>,
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

fn recover_session_subscription(
    application: &mut Application,
    subscription: &mut Option<SessionSubscription>,
    subscription_task: &mut Option<(SessionId, tokio::task::JoinHandle<()>)>,
    commands: SessionCommandClient,
    connected: &tokio::sync::mpsc::UnboundedSender<ConnectedSessionSubscription>,
) -> Result<()> {
    *subscription = None;
    let transition = application.handle_event(ApplicationEvent::SessionSubscriptionEnded)?;
    if let ApplicationTransition::SubscribeSession(session_id) = transition
        && subscription_task.is_none()
    {
        *subscription_task = Some((
            session_id,
            spawn_session_subscription(commands, session_id, connected.clone()),
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
