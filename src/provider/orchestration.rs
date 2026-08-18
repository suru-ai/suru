use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anyhow::Result;
use futures_util::StreamExt;
use tokio::sync::{mpsc, oneshot};

use super::{
    ProviderEvent, ProviderEventStream, ProviderRuntime, ProviderSession, ProviderSessionRequest,
    ProviderTurnInput,
};
use crate::protocol::{
    Message, MessageId, MessageRole, MessageStatus, PromptId, SessionChange, SessionId, TurnId,
    TurnStatus,
};
use crate::sessions::{DeliveredTurnStatus, InterruptTurnError, SessionStore};

#[derive(Clone)]
pub(crate) struct ProviderOrchestrator {
    runtime: Arc<dyn ProviderRuntime>,
    sessions: SessionStore,
    actors: Arc<Mutex<HashMap<SessionId, mpsc::UnboundedSender<ProviderCommand>>>>,
}

enum ProviderCommand {
    StartPrompt {
        prompt_id: PromptId,
    },
    InterruptTurn {
        turn_id: TurnId,
        response: oneshot::Sender<Result<crate::protocol::Turn, InterruptTurnError>>,
    },
}

struct ConnectedProviderSession {
    session: Arc<dyn ProviderSession>,
    events: ProviderEventStream,
}

struct ActiveProviderTurn {
    turn_id: TurnId,
    streaming_message_id: Option<MessageId>,
    interruption_acknowledged: bool,
}

enum ProviderInput {
    Command(Option<ProviderCommand>),
    Event(Option<Result<ProviderEvent, super::ProviderError>>),
}

impl ProviderOrchestrator {
    pub(crate) fn new(runtime: Arc<dyn ProviderRuntime>, sessions: SessionStore) -> Self {
        Self {
            runtime,
            sessions,
            actors: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) fn open_session(
        &self,
        session_id: SessionId,
        workspace: PathBuf,
        prompt_id: PromptId,
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
            .send(ProviderCommand::StartPrompt { prompt_id })
            .expect("new Provider actor accepts its initial Prompt");
    }

    pub(crate) fn schedule_prompt(&self, session_id: SessionId, prompt_id: PromptId) -> Result<()> {
        let actor = self
            .actors
            .lock()
            .expect("Provider actor registry lock is not poisoned")
            .get(&session_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Session has no Provider actor"))?;
        actor
            .send(ProviderCommand::StartPrompt { prompt_id })
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
            .get(&session_id)
            .cloned()
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
        let _ = self
            .sessions
            .fail_turn(session_id, turn_id, None, message.to_owned());
        InterruptTurnError::ProviderFailure(message.to_owned())
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
            let ProviderCommand::StartPrompt { prompt_id } = command else {
                if let ProviderCommand::InterruptTurn { turn_id, response } = command {
                    let result = sessions.interrupt_target(session_id, turn_id);
                    let _ = response.send(result);
                }
                continue;
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
                        let _ = sessions.deliver_prompt(
                            session_id,
                            prompt_id,
                            DeliveredTurnStatus::Failed {
                                message: format!("Provider startup failed: {error}"),
                            },
                        );
                        continue;
                    }
                };
                let (identity, session, events) = connection.into_parts();
                if let Err(error) = sessions.bind_agent(session_id, identity) {
                    let _ = sessions.deliver_prompt(
                        session_id,
                        prompt_id,
                        DeliveredTurnStatus::Failed {
                            message: format!("Provider startup failed: {error}"),
                        },
                    );
                    continue;
                }
                provider = Some(ConnectedProviderSession { session, events });
            }

            let delivered =
                match sessions.deliver_prompt(session_id, prompt_id, DeliveredTurnStatus::Active) {
                    Ok(Some(delivered)) => delivered,
                    Ok(None) => continue,
                    Err(_) => continue,
                };
            let provider_session = provider
                .as_ref()
                .expect("Provider connection exists before Prompt delivery")
                .session
                .clone();
            if let Err(error) = provider_session
                .start_turn(ProviderTurnInput {
                    prompt: delivered.prompt.text,
                })
                .await
            {
                let _ = sessions.fail_turn(
                    session_id,
                    delivered.turn.id,
                    None,
                    format!("Provider execution failed: {error}"),
                );
                continue;
            }
            active = Some(ActiveProviderTurn {
                turn_id: delivered.turn.id,
                streaming_message_id: None,
                interruption_acknowledged: false,
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
                command = commands.recv() => ProviderInput::Command(command),
                event = events.next() => ProviderInput::Event(event),
            }
        };
        match input {
            ProviderInput::Command(None) => return,
            ProviderInput::Command(Some(ProviderCommand::StartPrompt { .. })) => {
                // Prompts admitted while startup was still pending can already be queued here.
                // Keep them pending until queued delivery and steering gain their own orchestration.
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
                match provider_session.interrupt_turn().await {
                    Ok(()) => {
                        current.interruption_acknowledged = true;
                        let _ = response.send(Ok(target));
                    }
                    Err(error) => {
                        let message = format!("Provider interruption failed: {error}");
                        let _ = sessions.fail_turn(
                            session_id,
                            current.turn_id,
                            current.streaming_message_id.take(),
                            message.clone(),
                        );
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
        ProviderEvent::TurnInterrupted => sessions
            .interrupt_provider_turn(
                session_id,
                active.turn_id,
                active.streaming_message_id.take(),
            )
            .map(|_| true),
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
