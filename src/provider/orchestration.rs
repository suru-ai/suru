use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
};

use anyhow::Result;
use futures_util::StreamExt;
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::{Duration, timeout},
};

use super::{
    ProviderCommandStatus, ProviderEvent, ProviderEventStream, ProviderFileChangeStatus,
    ProviderRuntime, ProviderSession, ProviderSessionRequest, ProviderSteerInput,
    ProviderTurnInput, wait_for_shutdown,
};
use crate::protocol::{
    Activity, ActivityId, ActivityStatus, AgentIdentity, Message, MessageId, MessageRole,
    MessageStatus, PromptId, SessionChange, SessionId, TurnId, TurnStatus,
};
use crate::sessions::{
    DeliveredTurn, DeliveredTurnStatus, InterruptTurnError, ProviderTurnOutcome, SessionStore,
    UnfinishedProviderOutput,
};

#[derive(Clone)]
pub(crate) struct ProviderOrchestrator {
    runtime: Arc<dyn ProviderRuntime>,
    sessions: SessionStore,
    actors: Arc<Mutex<ProviderActors>>,
    shutdown: watch::Receiver<bool>,
    updates: ProviderUpdateGate,
    shutdown_complete: watch::Sender<bool>,
}

struct ProviderActors {
    shutting_down: bool,
    entries: HashMap<SessionId, ProviderActor>,
}

struct ProviderActor {
    commands: mpsc::UnboundedSender<ProviderCommand>,
    task: JoinHandle<()>,
}

#[derive(Clone)]
pub(crate) struct ProviderUpdateGate {
    accepting: Arc<RwLock<bool>>,
}

impl ProviderUpdateGate {
    pub(crate) fn new() -> Self {
        Self {
            accepting: Arc::new(RwLock::new(true)),
        }
    }

    pub(crate) fn stop(&self) {
        *self
            .accepting
            .write()
            .expect("Provider update gate lock is not poisoned") = false;
    }

    fn apply<T>(&self, update: impl FnOnce() -> T) -> Option<T> {
        let accepting = self
            .accepting
            .read()
            .expect("Provider update gate lock is not poisoned");
        (*accepting).then(update)
    }
}

enum ProviderCommand {
    StartPrompt {
        prompt_id: PromptId,
    },
    SteerPrompt,
    InterruptTurn {
        turn_id: TurnId,
        response: oneshot::Sender<Result<crate::protocol::Turn, InterruptTurnError>>,
    },
}

struct ConnectedProviderSession {
    identity: AgentIdentity,
    session: Arc<dyn ProviderSession>,
    events: ProviderEventStream,
}

struct ActiveProviderTurn {
    turn_id: TurnId,
    streaming_message_id: Option<MessageId>,
    interruption_acknowledged: bool,
    command_activities: HashMap<super::ProviderActivityId, ActivityId>,
    file_change_activities: HashMap<super::ProviderActivityId, ActivityId>,
}

impl ActiveProviderTurn {
    fn take_unfinished_output(&mut self) -> UnfinishedProviderOutput {
        UnfinishedProviderOutput {
            streaming_message_id: self.streaming_message_id.take(),
            active_command_ids: self
                .command_activities
                .drain()
                .map(|(_, activity_id)| activity_id)
                .collect(),
            active_file_change_ids: self
                .file_change_activities
                .drain()
                .map(|(_, activity_id)| activity_id)
                .collect(),
        }
    }
}

enum ProviderInput {
    Command(Option<ProviderCommand>),
    Event(Option<Result<ProviderEvent, super::ProviderError>>),
}

enum ProviderEventProjection {
    Continue,
    Terminal(Option<DeliveredTurn>),
}

impl ProviderOrchestrator {
    pub(crate) fn new(
        runtime: Arc<dyn ProviderRuntime>,
        sessions: SessionStore,
        shutdown: watch::Receiver<bool>,
        updates: ProviderUpdateGate,
    ) -> Self {
        let (shutdown_complete, _) = watch::channel(false);
        Self {
            runtime,
            sessions,
            actors: Arc::new(Mutex::new(ProviderActors {
                shutting_down: false,
                entries: HashMap::new(),
            })),
            shutdown,
            updates,
            shutdown_complete,
        }
    }

    pub(crate) fn open_session(
        &self,
        session_id: SessionId,
        workspace: PathBuf,
        prompt_id: PromptId,
    ) {
        let (commands_tx, commands_rx) = mpsc::unbounded_channel();
        let mut actors = self
            .actors
            .lock()
            .expect("Provider actor registry lock is not poisoned");
        if actors.shutting_down || *self.shutdown.borrow() {
            return;
        }
        let runtime = self.runtime.clone();
        let sessions = self.sessions.clone();
        let task = tokio::spawn(run_provider_session(
            runtime,
            sessions,
            session_id,
            workspace,
            commands_rx,
            self.shutdown.clone(),
            self.updates.clone(),
        ));
        actors.entries.insert(
            session_id,
            ProviderActor {
                commands: commands_tx.clone(),
                task,
            },
        );
        drop(actors);
        commands_tx
            .send(ProviderCommand::StartPrompt { prompt_id })
            .expect("new Provider actor accepts its initial Prompt");
    }

    pub(crate) fn schedule_prompt(&self, session_id: SessionId, prompt_id: PromptId) -> Result<()> {
        self.schedule(session_id, ProviderCommand::StartPrompt { prompt_id })
    }

    pub(crate) fn schedule_steer(&self, session_id: SessionId) -> Result<()> {
        self.schedule(session_id, ProviderCommand::SteerPrompt)
    }

    fn schedule(&self, session_id: SessionId, command: ProviderCommand) -> Result<()> {
        let actor = self
            .actors
            .lock()
            .expect("Provider actor registry lock is not poisoned")
            .entries
            .get(&session_id)
            .map(|actor| actor.commands.clone())
            .ok_or_else(|| anyhow::anyhow!("Session has no Provider actor"))?;
        actor
            .send(command)
            .map_err(|_| anyhow::anyhow!("Session Provider actor stopped unexpectedly"))
    }

    pub(crate) async fn interrupt_turn(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
    ) -> Result<crate::protocol::Turn, InterruptTurnError> {
        let target = self.sessions.interrupt_target(session_id, turn_id)?;
        if target.status == TurnStatus::Interrupted {
            return Ok(target);
        }
        let actor = self
            .actors
            .lock()
            .expect("Provider actor registry lock is not poisoned")
            .entries
            .get(&session_id)
            .map(|actor| actor.commands.clone())
            .ok_or_else(|| {
                self.fail_unavailable_interruption(
                    session_id,
                    turn_id,
                    "Provider interruption failed: the Session has no Provider actor.",
                )
            })?;
        let (response_tx, response_rx) = oneshot::channel();
        actor
            .send(ProviderCommand::InterruptTurn {
                turn_id,
                response: response_tx,
            })
            .map_err(|_| {
                self.fail_unavailable_interruption(
                    session_id,
                    turn_id,
                    "Provider interruption failed: the Provider Session stopped unexpectedly.",
                )
            })?;
        response_rx.await.map_err(|_| {
            self.fail_unavailable_interruption(
                session_id,
                turn_id,
                "Provider interruption failed: the Provider Session stopped unexpectedly.",
            )
        })?
    }

    fn fail_unavailable_interruption(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        message: &str,
    ) -> InterruptTurnError {
        let _ = self.updates.apply(|| {
            self.sessions.fail_turn(
                session_id,
                turn_id,
                UnfinishedProviderOutput::default(),
                message.to_owned(),
            )
        });
        InterruptTurnError::ProviderFailure(message.to_owned())
    }

    pub(crate) async fn shutdown(&self) {
        let actors = {
            let mut registry = self
                .actors
                .lock()
                .expect("Provider actor registry lock is not poisoned");
            if registry.shutting_down {
                None
            } else {
                registry.shutting_down = true;
                Some(
                    registry
                        .entries
                        .drain()
                        .map(|(_, actor)| actor)
                        .collect::<Vec<_>>(),
                )
            }
        };

        let Some(actors) = actors else {
            let mut complete = self.shutdown_complete.subscribe();
            while !*complete.borrow() && complete.changed().await.is_ok() {}
            return;
        };

        for actor in actors {
            let _ = actor.task.await;
        }
        let _ = timeout(Duration::from_secs(2), self.runtime.shutdown()).await;
        self.shutdown_complete.send_replace(true);
    }
}

async fn run_provider_session(
    runtime: Arc<dyn ProviderRuntime>,
    sessions: SessionStore,
    session_id: SessionId,
    workspace: PathBuf,
    mut commands: mpsc::UnboundedReceiver<ProviderCommand>,
    mut shutdown: watch::Receiver<bool>,
    updates: ProviderUpdateGate,
) {
    let mut provider: Option<ConnectedProviderSession> = None;
    let mut active: Option<ActiveProviderTurn> = None;

    'actor: loop {
        if *shutdown.borrow() {
            break;
        }
        if active.is_none() {
            let input = if let Some(connected) = provider.as_mut() {
                tokio::select! {
                    biased;
                    _ = wait_for_shutdown(&mut shutdown) => break,
                    event = connected.events.next() => ProviderInput::Event(event),
                    command = commands.recv() => ProviderInput::Command(command),
                }
            } else {
                tokio::select! {
                    biased;
                    _ = wait_for_shutdown(&mut shutdown) => break,
                    command = commands.recv() => ProviderInput::Command(command),
                }
            };
            let command = match input {
                ProviderInput::Command(command) => command,
                ProviderInput::Event(Some(Ok(_))) => continue,
                ProviderInput::Event(Some(Err(_)) | None) => {
                    provider = None;
                    continue;
                }
            };
            let Some(command) = command else { break };
            let prompt_id = match command {
                ProviderCommand::StartPrompt { prompt_id } => prompt_id,
                ProviderCommand::InterruptTurn { turn_id, response } => {
                    let result = sessions.interrupt_target(session_id, turn_id);
                    let _ = response.send(result);
                    continue;
                }
                ProviderCommand::SteerPrompt => continue,
            };
            if provider.is_none() {
                let connection = tokio::select! {
                    biased;
                    _ = wait_for_shutdown(&mut shutdown) => break 'actor,
                    connection = runtime.start_session(ProviderSessionRequest {
                        session_id,
                        workspace: workspace.clone(),
                    }) => connection,
                };
                let connection = match connection {
                    Ok(connection) => connection,
                    Err(error) => {
                        let Some(_) = updates.apply(|| {
                            sessions.deliver_prompt(
                                session_id,
                                prompt_id,
                                None,
                                DeliveredTurnStatus::Failed {
                                    message: format!("Provider startup failed: {error}"),
                                },
                            )
                        }) else {
                            break;
                        };
                        continue;
                    }
                };
                let (identity, session, events) = connection.into_parts();
                let selection = identity.selection.clone();
                let Some(selected) =
                    updates.apply(|| sessions.initialize_agent_selection(session_id, selection))
                else {
                    let _ = timeout(Duration::from_secs(2), session.shutdown()).await;
                    break;
                };
                if let Err(error) = selected {
                    let _ = updates.apply(|| {
                        sessions.deliver_prompt(
                            session_id,
                            prompt_id,
                            None,
                            DeliveredTurnStatus::Failed {
                                message: format!("Provider startup failed: {error}"),
                            },
                        )
                    });
                    let _ = timeout(Duration::from_secs(2), session.shutdown()).await;
                    continue;
                }
                provider = Some(ConnectedProviderSession {
                    identity,
                    session,
                    events,
                });
            }

            let identity = provider
                .as_ref()
                .expect("Provider connection exists before Prompt delivery")
                .identity
                .clone();
            let Some(delivered) = updates.apply(|| {
                sessions.deliver_prompt(
                    session_id,
                    prompt_id,
                    Some(identity.agent.clone()),
                    DeliveredTurnStatus::Active,
                )
            }) else {
                break;
            };
            let delivered = match delivered {
                Ok(Some(delivered)) => delivered,
                Ok(None) => continue,
                Err(_) => continue,
            };
            let provider_session = provider
                .as_ref()
                .expect("Provider connection exists before Prompt delivery")
                .session
                .clone();
            let (turn_id, input) = provider_turn_start(delivered);
            let started = tokio::select! {
                biased;
                _ = wait_for_shutdown(&mut shutdown) => break 'actor,
                started = provider_session.start_turn(input) => started,
            };
            if let Err(error) = started {
                let session_lost = error.is_session_lost();
                project_turn_start_failure(&sessions, &updates, session_id, turn_id, &error);
                if session_lost {
                    provider = None;
                }
                continue;
            }
            active = Some(ActiveProviderTurn {
                turn_id,
                streaming_message_id: None,
                interruption_acknowledged: false,
                command_activities: HashMap::new(),
                file_change_activities: HashMap::new(),
            });
            continue;
        }

        let provider_session = provider
            .as_ref()
            .expect("an active Provider Turn has a Provider Session")
            .session
            .clone();
        let input = {
            let events = &mut provider
                .as_mut()
                .expect("an active Provider Turn has a Provider Session")
                .events;
            tokio::select! {
                // Preserve the Provider's terminal boundary when both it and a later command
                // became ready while an RPC was in flight.
                biased;
                _ = wait_for_shutdown(&mut shutdown) => break 'actor,
                event = events.next() => ProviderInput::Event(event),
                command = commands.recv() => ProviderInput::Command(command),
            }
        };
        match input {
            ProviderInput::Command(None) => break,
            ProviderInput::Command(Some(ProviderCommand::StartPrompt { .. })) => {
                // Prompts admitted while startup was still pending can already be queued here.
                // Keep them pending until queued delivery and steering gain their own orchestration.
            }
            ProviderInput::Command(Some(ProviderCommand::SteerPrompt)) => {
                let current = active
                    .as_ref()
                    .expect("Provider input is handled while a Turn is active");
                let prompt = match sessions.next_pending_steer(session_id, current.turn_id) {
                    Ok(Some(prompt)) => prompt,
                    Ok(None) | Err(_) => continue,
                };
                let steered = tokio::select! {
                    biased;
                    _ = wait_for_shutdown(&mut shutdown) => break 'actor,
                    steered = provider_session.steer_turn(ProviderSteerInput {
                        prompt: prompt.text.clone(),
                    }) => steered,
                };
                match steered {
                    Ok(()) => {
                        let _ = updates.apply(|| {
                            sessions.deliver_steer(session_id, current.turn_id, prompt.id)
                        });
                    }
                    Err(error) => {
                        let _ = updates.apply(|| {
                            sessions.report_steer_failure(
                                session_id,
                                current.turn_id,
                                prompt.id,
                                format!("Provider steering failed: {error}"),
                            )
                        });
                    }
                }
            }
            ProviderInput::Command(Some(ProviderCommand::InterruptTurn { turn_id, response })) => {
                let target = match sessions.interrupt_target(session_id, turn_id) {
                    Ok(target) => target,
                    Err(error) => {
                        let _ = response.send(Err(error));
                        continue;
                    }
                };
                if target.status == TurnStatus::Interrupted {
                    let _ = response.send(Ok(target));
                    continue;
                }
                let current = active
                    .as_mut()
                    .expect("Provider input is handled while a Turn is active");
                if current.turn_id != turn_id {
                    let _ = response.send(Err(InterruptTurnError::ProviderFailure(
                        "Provider interruption failed: the Provider owns a different active Turn."
                            .to_owned(),
                    )));
                    continue;
                }
                if current.interruption_acknowledged {
                    let _ = response.send(Ok(target));
                    continue;
                }
                let interrupted = tokio::select! {
                    biased;
                    _ = wait_for_shutdown(&mut shutdown) => break 'actor,
                    interrupted = provider_session.interrupt_turn() => interrupted,
                };
                match interrupted {
                    Ok(()) => {
                        current.interruption_acknowledged = true;
                        let _ = response.send(Ok(target));
                    }
                    Err(error) => {
                        let message = format!("Provider interruption failed: {error}");
                        let unfinished_output = current.take_unfinished_output();
                        let _ = updates.apply(|| {
                            sessions.fail_turn(
                                session_id,
                                current.turn_id,
                                unfinished_output,
                                message.clone(),
                            )
                        });
                        active = None;
                        provider = None;
                        let _ = response.send(Err(InterruptTurnError::ProviderFailure(message)));
                    }
                }
            }
            ProviderInput::Event(event) => {
                let Some(current) = active.as_mut() else {
                    continue;
                };
                match event {
                    Some(Ok(event)) => {
                        let identity = provider
                            .as_ref()
                            .expect("Provider connection exists while its Turn is active")
                            .identity
                            .clone();
                        if let ProviderEventProjection::Terminal(next_turn) = project_provider_event(
                            &sessions, &updates, session_id, current, &identity, event,
                        ) {
                            active = None;
                            if let Some(delivered) = next_turn {
                                let (turn_id, input) = provider_turn_start(delivered);
                                let started = tokio::select! {
                                    biased;
                                    _ = wait_for_shutdown(&mut shutdown) => break 'actor,
                                    started = provider_session.start_turn(input) => started,
                                };
                                if let Err(error) = started {
                                    let session_lost = error.is_session_lost();
                                    project_turn_start_failure(
                                        &sessions, &updates, session_id, turn_id, &error,
                                    );
                                    if session_lost {
                                        provider = None;
                                    }
                                } else {
                                    active = Some(ActiveProviderTurn {
                                        turn_id,
                                        streaming_message_id: None,
                                        interruption_acknowledged: false,
                                        command_activities: HashMap::new(),
                                        file_change_activities: HashMap::new(),
                                    });
                                }
                            }
                        }
                    }
                    Some(Err(error)) => {
                        let unfinished_output = current.take_unfinished_output();
                        let _ = updates.apply(|| {
                            sessions.fail_turn(
                                session_id,
                                current.turn_id,
                                unfinished_output,
                                format!("Provider execution failed: {error}"),
                            )
                        });
                        active = None;
                        provider = None;
                    }
                    None => {
                        let unfinished_output = current.take_unfinished_output();
                        let _ = updates.apply(|| {
                            sessions.fail_turn(
                                session_id,
                                current.turn_id,
                                unfinished_output,
                                "Provider execution failed: the Provider Session ended before the Turn completed."
                                    .to_owned(),
                            )
                        });
                        active = None;
                        provider = None;
                    }
                }
            }
        }
    }

    if let Some(connected) = provider {
        let _ = timeout(Duration::from_secs(2), connected.session.shutdown()).await;
    }
}

fn provider_turn_start(delivered: DeliveredTurn) -> (TurnId, ProviderTurnInput) {
    let turn_id = delivered.turn.id;
    let selection = delivered
        .turn
        .agent
        .expect("a connected Provider delivers a selected Turn")
        .selection;
    (
        turn_id,
        ProviderTurnInput {
            prompt: delivered.prompt.text,
            selection,
        },
    )
}

fn project_turn_start_failure(
    sessions: &SessionStore,
    updates: &ProviderUpdateGate,
    session_id: SessionId,
    turn_id: TurnId,
    error: &super::ProviderError,
) {
    let selection_rejected = error.is_selection_rejected();
    let _ = updates.apply(|| {
        let message = format!("Provider execution failed: {error}");
        if selection_rejected {
            sessions.reject_agent_selection(
                session_id,
                turn_id,
                UnfinishedProviderOutput::default(),
                message,
            )
        } else {
            sessions.fail_turn(
                session_id,
                turn_id,
                UnfinishedProviderOutput::default(),
                message,
            )
        }
    });
}

fn project_provider_event(
    sessions: &SessionStore,
    updates: &ProviderUpdateGate,
    session_id: SessionId,
    active: &mut ActiveProviderTurn,
    next_agent: &AgentIdentity,
    event: ProviderEvent,
) -> ProviderEventProjection {
    let Some(projected) = updates.apply(|| {
        let projection = match event {
            ProviderEvent::AgentSelectionChanged { selection } => sessions
                .reconcile_effective_agent_selection(
                    session_id,
                    active.turn_id,
                    next_agent.agent.clone(),
                    selection,
                )
                .map(|_| ProviderEventProjection::Continue),
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
                        .map(|_| ProviderEventProjection::Continue)
                }
            }
            ProviderEvent::AgentMessageDelta { content } => {
                let Some(message_id) = active.streaming_message_id else {
                    return fail_invalid_provider_event(
                        sessions,
                        session_id,
                        active,
                        next_agent,
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
                    .map(|_| ProviderEventProjection::Continue)
            }
            ProviderEvent::AgentMessageCompleted => {
                let Some(message_id) = active.streaming_message_id.take() else {
                    return fail_invalid_provider_event(
                        sessions,
                        session_id,
                        active,
                        next_agent,
                        "Provider completed an Agent Message before starting one",
                    );
                };
                sessions
                    .publish_agent_output(
                        session_id,
                        SessionChange::MessageCompleted { message_id },
                    )
                    .map(|_| ProviderEventProjection::Continue)
            }
            ProviderEvent::CommandStarted {
                activity_id,
                command,
                cwd,
            } => {
                if active.command_activities.contains_key(&activity_id)
                    || active.file_change_activities.contains_key(&activity_id)
                {
                    Err(anyhow::anyhow!(
                        "Provider reused an active command Activity identity"
                    ))
                } else {
                    let command_activity_id = ActivityId::new();
                    sessions
                        .publish_agent_output(
                            session_id,
                            SessionChange::ActivityAdded {
                                activity: Activity::Command {
                                    id: command_activity_id,
                                    turn_id: active.turn_id,
                                    status: ActivityStatus::Active,
                                    command,
                                    cwd,
                                    output: String::new(),
                                    exit_status: None,
                                },
                            },
                        )
                        .map(|_| {
                            active
                                .command_activities
                                .insert(activity_id, command_activity_id);
                            ProviderEventProjection::Continue
                        })
                }
            }
            ProviderEvent::CommandOutputDelta {
                activity_id,
                content,
            } => {
                let Some(command_activity_id) =
                    active.command_activities.get(&activity_id).copied()
                else {
                    return fail_invalid_provider_event(
                        sessions,
                        session_id,
                        active,
                        next_agent,
                        "Provider sent command output before starting the Activity",
                    );
                };
                sessions
                    .publish_agent_output(
                        session_id,
                        SessionChange::CommandOutputAppended {
                            activity_id: command_activity_id,
                            content,
                        },
                    )
                    .map(|_| ProviderEventProjection::Continue)
            }
            ProviderEvent::CommandCompleted {
                activity_id,
                status,
                exit_status,
            } => {
                let Some(command_activity_id) =
                    active.command_activities.get(&activity_id).copied()
                else {
                    return fail_invalid_provider_event(
                        sessions,
                        session_id,
                        active,
                        next_agent,
                        "Provider completed a command before starting the Activity",
                    );
                };
                sessions
                    .publish_agent_output(
                        session_id,
                        SessionChange::CommandStatusChanged {
                            activity_id: command_activity_id,
                            status: match status {
                                ProviderCommandStatus::Completed => ActivityStatus::Completed,
                                ProviderCommandStatus::Failed => ActivityStatus::Failed,
                            },
                            exit_status,
                        },
                    )
                    .map(|_| {
                        active.command_activities.remove(&activity_id);
                        ProviderEventProjection::Continue
                    })
            }
            ProviderEvent::FileChangeStarted {
                activity_id,
                changes,
            } => {
                if active.command_activities.contains_key(&activity_id)
                    || active.file_change_activities.contains_key(&activity_id)
                {
                    Err(anyhow::anyhow!(
                        "Provider reused an active file-change Activity identity"
                    ))
                } else {
                    let file_change_activity_id = ActivityId::new();
                    sessions
                        .publish_agent_output(
                            session_id,
                            SessionChange::ActivityAdded {
                                activity: Activity::FileChange {
                                    id: file_change_activity_id,
                                    turn_id: active.turn_id,
                                    status: ActivityStatus::Active,
                                    changes,
                                },
                            },
                        )
                        .map(|_| {
                            active
                                .file_change_activities
                                .insert(activity_id, file_change_activity_id);
                            ProviderEventProjection::Continue
                        })
                }
            }
            ProviderEvent::FileChangeUpdated {
                activity_id,
                changes,
            } => {
                let Some(file_change_activity_id) =
                    active.file_change_activities.get(&activity_id).copied()
                else {
                    return fail_invalid_provider_event(
                        sessions,
                        session_id,
                        active,
                        next_agent,
                        "Provider updated file changes before starting the Activity",
                    );
                };
                sessions
                    .publish_agent_output(
                        session_id,
                        SessionChange::FileChangeUpdated {
                            activity_id: file_change_activity_id,
                            changes,
                        },
                    )
                    .map(|_| ProviderEventProjection::Continue)
            }
            ProviderEvent::FileChangeCompleted {
                activity_id,
                status,
            } => {
                let Some(file_change_activity_id) =
                    active.file_change_activities.get(&activity_id).copied()
                else {
                    return fail_invalid_provider_event(
                        sessions,
                        session_id,
                        active,
                        next_agent,
                        "Provider completed file changes before starting the Activity",
                    );
                };
                sessions
                    .publish_agent_output(
                        session_id,
                        SessionChange::FileChangeStatusChanged {
                            activity_id: file_change_activity_id,
                            status: match status {
                                ProviderFileChangeStatus::Completed => ActivityStatus::Completed,
                                ProviderFileChangeStatus::Failed => ActivityStatus::Failed,
                            },
                        },
                    )
                    .map(|_| {
                        active.file_change_activities.remove(&activity_id);
                        ProviderEventProjection::Continue
                    })
            }
            ProviderEvent::TurnCompleted => {
                if active.streaming_message_id.is_some()
                    || !active.command_activities.is_empty()
                    || !active.file_change_activities.is_empty()
                {
                    Err(anyhow::anyhow!(
                        "Provider completed the Turn before completing its streamed output"
                    ))
                } else {
                    sessions
                        .finish_provider_turn(
                            session_id,
                            active.turn_id,
                            next_agent.agent.clone(),
                            ProviderTurnOutcome::Completed,
                        )
                        .map(ProviderEventProjection::Terminal)
                }
            }
            ProviderEvent::TurnInterrupted => {
                let unfinished_output = active.take_unfinished_output();
                sessions
                    .finish_provider_turn(
                        session_id,
                        active.turn_id,
                        next_agent.agent.clone(),
                        ProviderTurnOutcome::Interrupted { unfinished_output },
                    )
                    .map(ProviderEventProjection::Terminal)
            }
            ProviderEvent::AgentSelectionRejected { message } => {
                let unfinished_output = active.take_unfinished_output();
                sessions
                    .reject_agent_selection(session_id, active.turn_id, unfinished_output, message)
                    .map(|_| ProviderEventProjection::Terminal(None))
            }
            ProviderEvent::TurnFailed { message } => {
                let unfinished_output = active.take_unfinished_output();
                sessions
                    .finish_provider_turn(
                        session_id,
                        active.turn_id,
                        next_agent.agent.clone(),
                        ProviderTurnOutcome::Failed {
                            unfinished_output,
                            message,
                        },
                    )
                    .map(ProviderEventProjection::Terminal)
            }
        };

        projection.unwrap_or_else(|error| {
            finish_invalid_provider_event(
                sessions,
                session_id,
                active,
                next_agent,
                format!("Provider execution failed: {error}"),
            )
        })
    }) else {
        return ProviderEventProjection::Terminal(None);
    };
    projected
}

fn fail_invalid_provider_event(
    sessions: &SessionStore,
    session_id: SessionId,
    active: &mut ActiveProviderTurn,
    next_agent: &AgentIdentity,
    message: &str,
) -> ProviderEventProjection {
    finish_invalid_provider_event(
        sessions,
        session_id,
        active,
        next_agent,
        format!("Provider execution failed: {message}"),
    )
}

fn finish_invalid_provider_event(
    sessions: &SessionStore,
    session_id: SessionId,
    active: &mut ActiveProviderTurn,
    next_agent: &AgentIdentity,
    message: String,
) -> ProviderEventProjection {
    let unfinished_output = active.take_unfinished_output();
    let next_turn = sessions
        .finish_provider_turn(
            session_id,
            active.turn_id,
            next_agent.agent.clone(),
            ProviderTurnOutcome::Failed {
                unfinished_output,
                message,
            },
        )
        .ok()
        .flatten();
    ProviderEventProjection::Terminal(next_turn)
}
