//! The background writer that coalesces Session state into durable rows.
//!
//! Streaming paths hand work to [`StorageSink`] and move on. The writer thread owns the projected
//! copy of every Session, marks it dirty, and flushes on Turn boundaries and idle ticks so SQLite
//! I/O never sits in the path of a Provider stream.

use std::{
    collections::HashMap,
    sync::mpsc as std_mpsc,
    thread::{self, JoinHandle},
    time::Duration,
};

use crate::{
    protocol::{
        AgentSelection, SessionChange, SessionId, SessionSnapshot, SessionStatus, SessionSummary,
        SessionUpdate, TurnStatus,
    },
    session_projection::apply_update,
};

use super::{PersistedSession, StorageError, StorageRepository, StoredResumeState};

const IDLE_FLUSH_DELAY: Duration = Duration::from_millis(100);

#[derive(Clone)]
pub(crate) struct StorageSink {
    commands: std_mpsc::Sender<WriterCommand>,
}

pub(crate) struct StorageWriter {
    commands: std_mpsc::Sender<WriterCommand>,
    task: JoinHandle<Result<(), StorageError>>,
}

enum WriterCommand {
    Create(Box<PersistedSession>),
    Update {
        summary: SessionSummary,
        update: SessionUpdate,
        durability: Option<std_mpsc::SyncSender<Result<(), String>>>,
    },
    Delete {
        session_id: SessionId,
        durability: std_mpsc::SyncSender<Result<(), String>>,
    },
    SaveLandingAgentSelection(AgentSelection),
    SaveResumeState {
        state: StoredResumeState,
        durability: std_mpsc::SyncSender<Result<(), String>>,
    },
    Shutdown,
}

struct WriterState {
    persisted: PersistedSession,
    dirty: bool,
}

impl StorageWriter {
    pub(crate) fn spawn(
        repository: StorageRepository,
        restored: &[PersistedSession],
    ) -> (Self, StorageSink) {
        let (commands, receiver) = std_mpsc::channel();
        let mut sessions = restored
            .iter()
            .cloned()
            .map(|persisted| {
                (
                    persisted.snapshot.session.id,
                    WriterState {
                        persisted,
                        dirty: false,
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        let task = thread::spawn(move || {
            loop {
                match receiver.recv_timeout(IDLE_FLUSH_DELAY) {
                    Ok(WriterCommand::Create(persisted)) => {
                        let persisted = *persisted;
                        sessions.insert(
                            persisted.snapshot.session.id,
                            WriterState {
                                persisted,
                                dirty: true,
                            },
                        );
                    }
                    Ok(WriterCommand::Update {
                        summary,
                        update,
                        durability,
                    }) => {
                        let session_id = update.session_id;
                        let state = sessions.get_mut(&session_id).ok_or_else(|| {
                            StorageError::WriterTask(format!(
                                "received update for unknown Session {session_id}"
                            ))
                        })?;
                        apply_update(&mut state.persisted.snapshot, &update).map_err(|error| {
                            StorageError::WriterTask(format!(
                                "project update for Session {session_id}: {error:#}"
                            ))
                        })?;
                        state.persisted.summary = summary;
                        state.dirty = true;
                        if is_turn_boundary(&update) {
                            let result =
                                flush_sessions(&repository, &mut sessions, Some(session_id));
                            if let Some(durability) = durability {
                                let _ = durability
                                    .send(result.as_ref().map(|_| ()).map_err(ToString::to_string));
                            }
                            result?;
                        }
                    }
                    Ok(WriterCommand::Delete {
                        session_id,
                        durability,
                    }) => {
                        let result = repository.delete_session(session_id);
                        if result.is_ok() {
                            sessions.remove(&session_id);
                        }
                        let _ = durability
                            .send(result.as_ref().map(|_| ()).map_err(ToString::to_string));
                        result?;
                    }
                    Ok(WriterCommand::SaveLandingAgentSelection(selection)) => {
                        repository.save_landing_agent_selection(selection)?;
                    }
                    Ok(WriterCommand::SaveResumeState { state, durability }) => {
                        let result =
                            flush_sessions(&repository, &mut sessions, Some(state.session_id))
                                .and_then(|()| repository.save_resume_state(&state));
                        if result.is_ok()
                            && let Some(session) = sessions.get_mut(&state.session_id)
                        {
                            session
                                .persisted
                                .resume_states
                                .insert(state.provider, state.resume_state);
                        }
                        let _ = durability
                            .send(result.as_ref().map(|_| ()).map_err(ToString::to_string));
                        result?;
                    }
                    Ok(WriterCommand::Shutdown) | Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                        flush_sessions(&repository, &mut sessions, None)?;
                        break;
                    }
                    Err(std_mpsc::RecvTimeoutError::Timeout) => {
                        flush_sessions(&repository, &mut sessions, None)?;
                    }
                }
            }
            Ok(())
        });
        (
            Self {
                commands: commands.clone(),
                task,
            },
            StorageSink { commands },
        )
    }

    pub(crate) async fn shutdown(self) -> Result<(), StorageError> {
        let _ = self.commands.send(WriterCommand::Shutdown);
        tokio::task::spawn_blocking(move || self.task.join())
            .await
            .map_err(|error| StorageError::WriterTask(error.to_string()))?
            .map_err(|_| StorageError::WriterTask("writer thread panicked".to_owned()))?
    }
}

impl StorageSink {
    pub(crate) fn created(&self, summary: SessionSummary, snapshot: SessionSnapshot) {
        let _ = self
            .commands
            .send(WriterCommand::Create(Box::new(PersistedSession {
                summary,
                snapshot,
                resume_states: HashMap::new(),
            })));
    }

    pub(crate) fn updated(
        &self,
        summary: SessionSummary,
        update: &SessionUpdate,
    ) -> Result<(), StorageError> {
        // The sender is the only streaming-path work. Projection, coalescing, and SQLite I/O all
        // happen in the background writer.
        let (durability, receipt) = if is_turn_boundary(update) {
            let (sender, receiver) = std_mpsc::sync_channel(0);
            (Some(sender), Some(receiver))
        } else {
            (None, None)
        };
        self.commands
            .send(WriterCommand::Update {
                summary,
                update: update.clone(),
                durability,
            })
            .map_err(|_| StorageError::WriterTask("writer is no longer running".to_owned()))?;
        if let Some(receipt) = receipt {
            receipt
                .recv()
                .map_err(|_| {
                    StorageError::WriterTask(
                        "writer stopped before confirming a Turn boundary".to_owned(),
                    )
                })?
                .map_err(StorageError::WriterTask)?;
        }
        Ok(())
    }

    pub(crate) fn save_landing_agent_selection(&self, selection: AgentSelection) {
        let _ = self
            .commands
            .send(WriterCommand::SaveLandingAgentSelection(selection));
    }

    pub(crate) fn save_resume_state(&self, state: StoredResumeState) -> Result<(), StorageError> {
        let (durability, receipt) = std_mpsc::sync_channel(0);
        self.commands
            .send(WriterCommand::SaveResumeState { state, durability })
            .map_err(|_| StorageError::WriterTask("writer is no longer running".to_owned()))?;
        receipt
            .recv()
            .map_err(|_| {
                StorageError::WriterTask("writer stopped before confirming Resume State".to_owned())
            })?
            .map_err(StorageError::WriterTask)
    }

    pub(crate) fn deleted(&self, session_id: SessionId) -> Result<(), StorageError> {
        let (durability, receipt) = std_mpsc::sync_channel(0);
        self.commands
            .send(WriterCommand::Delete {
                session_id,
                durability,
            })
            .map_err(|_| StorageError::WriterTask("writer is no longer running".to_owned()))?;
        receipt
            .recv()
            .map_err(|_| {
                StorageError::WriterTask(
                    "writer stopped before confirming Session deletion".to_owned(),
                )
            })?
            .map_err(StorageError::WriterTask)
    }
}

fn flush_sessions(
    repository: &StorageRepository,
    sessions: &mut HashMap<SessionId, WriterState>,
    only: Option<SessionId>,
) -> Result<(), StorageError> {
    let dirty = sessions
        .iter()
        .filter(|(session_id, state)| {
            state.dirty && only.as_ref().is_none_or(|only| only == *session_id)
        })
        .map(|(_, state)| state.persisted.clone())
        .collect::<Vec<_>>();
    repository.save_sessions(dirty)?;
    for (session_id, state) in sessions {
        if state.dirty && only.as_ref().is_none_or(|only| only == session_id) {
            state.dirty = false;
        }
    }
    Ok(())
}

fn is_turn_boundary(update: &SessionUpdate) -> bool {
    update.changes.iter().any(|change| match change {
        SessionChange::TurnAdded { turn } => turn.status != TurnStatus::Active,
        SessionChange::TurnStatusChanged { status, .. } => *status != TurnStatus::Active,
        SessionChange::SessionStatusChanged { status } => *status == SessionStatus::Idle,
        _ => false,
    })
}
