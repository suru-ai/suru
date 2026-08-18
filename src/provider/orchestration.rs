use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anyhow::Result;
use futures_util::StreamExt;
use tokio::sync::mpsc;

use super::{
    ProviderEvent, ProviderEventStream, ProviderRuntime, ProviderSession, ProviderSessionRequest,
    ProviderTurnInput,
};
use crate::protocol::{
    Message, MessageId, MessageRole, MessageStatus, PromptId, SessionChange, SessionId, TurnId,
};
use crate::sessions::{DeliveredTurnStatus, SessionStore};

#[derive(Clone)]
pub(crate) struct ProviderOrchestrator {
    runtime: Arc<dyn ProviderRuntime>,
    sessions: SessionStore,
    actors: Arc<Mutex<HashMap<SessionId, mpsc::UnboundedSender<ProviderCommand>>>>,
}

#[derive(Clone, Copy)]
struct ProviderCommand {
    prompt_id: PromptId,
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
            .send(ProviderCommand { prompt_id })
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
                        let _ = sessions.deliver_prompt(
                            session_id,
                            command.prompt_id,
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
                        command.prompt_id,
                        DeliveredTurnStatus::Failed {
                            message: format!("Provider startup failed: {error}"),
                        },
                    );
                    continue;
                }
                provider = Some(ConnectedProviderSession { session, events });
            }

            let delivered = match sessions.deliver_prompt(
                session_id,
                command.prompt_id,
                DeliveredTurnStatus::Active,
            ) {
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
                // Prompts admitted while startup was still pending can already be queued here.
                // Keep them pending until queued delivery and steering gain their own orchestration.
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
