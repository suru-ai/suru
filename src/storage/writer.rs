//! The background writer that coalesces Session state into durable rows.
//!
//! Streaming paths hand work to [`StorageSink`] and move on. The writer thread owns the projected
//! copy of each accessed or newly created Session, marks it dirty, and flushes on Turn boundaries and idle ticks so SQLite
//! I/O never sits in the path of a Provider stream. An idle tick that follows work, or that finds the sweep interval
//! passed, also sweeps orphaned Attachments once the flush has landed every Session's joins.

use std::{
    collections::HashMap,
    sync::mpsc as std_mpsc,
    thread::{self, JoinHandle},
    time::Duration,
};

use crate::{
    model_catalog::RememberedProviderCatalog,
    protocol::{
        AgentSelection, SessionChange, SessionId, SessionStatus, SessionSummary, SessionUpdate,
        WorkspaceDescription, WorkspaceId,
    },
    session_projection::apply_update,
};

use super::{
    PersistedSession, StorageError, StorageRepository, StoredResumeState, StoredSidekickAct,
    WorkspaceWrite,
};

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
    LocationChanged {
        session: Box<crate::protocol::Session>,
        revision: crate::protocol::SessionRevision,
        durability: std_mpsc::SyncSender<Result<(), String>>,
    },
    /// A Session just created, and the acts of Sidekicks its creation
    /// follows — a Sidekick's beginning of it.
    Create {
        persisted: Box<PersistedSession>,
        acts: Vec<StoredSidekickAct>,
    },
    Hydrate(Box<PersistedSession>),
    /// Catalog-only metadata, such as whether a Session is set aside or viewed,
    /// that changed without an update to the open Session, and the acts of
    /// Sidekicks the change follows.
    SummaryChanged {
        summary: Box<SessionSummary>,
        acts: Vec<StoredSidekickAct>,
    },
    Update {
        summary: Box<SessionSummary>,
        update: SessionUpdate,
        durability: Option<std_mpsc::SyncSender<Result<(), String>>>,
        /// The acts of Sidekicks the update follows.
        acts: Vec<StoredSidekickAct>,
    },
    Delete {
        session_id: SessionId,
        durability: std_mpsc::SyncSender<Result<(), String>>,
    },
    SaveLandingAgentSelection(AgentSelection),
    SaveModelCatalog(RememberedProviderCatalog),
    SaveWorkspaceIcon {
        workspace_id: WorkspaceId,
        icon: String,
    },
    /// A user's own choice replacing whatever a Workspace's Icon table row
    /// already held — the durable half of the Session store's own
    /// `set_workspace_icon`, off its commit path the same way
    /// `SaveWorkspaceIcon` is for a derivation.
    ReplaceWorkspaceIcon {
        workspace_id: WorkspaceId,
        icon: String,
    },
    /// A derived Description filling a Workspace's absent one, on the same
    /// terms `SaveWorkspaceIcon` fills an absent Icon.
    SaveWorkspaceDescription {
        workspace_id: WorkspaceId,
        description: WorkspaceDescription,
    },
    /// A Description the user or a Sidekick set, or its clearing, replacing
    /// whatever the Workspace's row already held, on the same terms
    /// `ReplaceWorkspaceIcon` replaces an Icon.
    ReplaceWorkspaceDescription {
        workspace_id: WorkspaceId,
        description: Option<WorkspaceDescription>,
    },
    SaveResumeState {
        state: StoredResumeState,
        durability: std_mpsc::SyncSender<Result<(), String>>,
    },
    /// A Sidekick's latest act on a Session of this Server that no change
    /// to that Session carries.
    RecordSidekickAct(StoredSidekickAct),
    Shutdown,
}

struct WriterState {
    persisted: PersistedSession,
    dirty: bool,
    /// The acts of Sidekicks on this Session that the changes not yet
    /// flushed follow, which land in the same transaction as those changes.
    acts: Vec<StoredSidekickAct>,
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
                        acts: Vec::new(),
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        // The acts no change to their Session carried, kept until each is
        // written, however often writing one fails.
        let mut unwritten_acts = Vec::<StoredSidekickAct>::new();
        let task = thread::spawn(move || {
            // Whether a command arrived since the writer last went idle: the
            // idle flush ending each burst of work sweeps orphaned
            // Attachments once, and a quiet tick after it sweeps only once
            // the sweep interval has passed.
            let mut worked = false;
            loop {
                let received = receiver.recv_timeout(IDLE_FLUSH_DELAY);
                worked |= received.is_ok();
                match received {
                    Ok(WriterCommand::Hydrate(persisted)) => {
                        sessions
                            .entry(persisted.snapshot.session.id)
                            .or_insert(WriterState {
                                persisted: *persisted,
                                dirty: false,
                                acts: Vec::new(),
                            });
                    }
                    Ok(WriterCommand::Create { persisted, acts }) => {
                        let persisted = *persisted;
                        sessions.insert(
                            persisted.snapshot.session.id,
                            WriterState {
                                persisted,
                                dirty: true,
                                acts,
                            },
                        );
                    }
                    Ok(WriterCommand::LocationChanged {
                        session,
                        revision,
                        durability,
                    }) => {
                        let result = if let Some(state) = sessions.get_mut(&session.id) {
                            state.persisted.snapshot.session = (*session).clone();
                            state.persisted.snapshot.revision = revision;
                            state.persisted.summary.session = (*session).clone();
                            state.dirty = true;
                            flush_sessions(&repository, &mut sessions, Some(session.id))
                        } else {
                            repository.save_location(&session, revision)
                        };
                        let _ = durability
                            .send(result.as_ref().map(|_| ()).map_err(ToString::to_string));
                        result?;
                    }
                    Ok(WriterCommand::SummaryChanged { summary, acts }) => {
                        // A Session the writer does not know is one already
                        // deleted; a Title that arrives for it has nothing left
                        // to land on and is dropped rather than resurrecting a
                        // row, and so is an act on it.
                        if let Some(state) = sessions.get_mut(&summary.session.id) {
                            state.persisted.summary = *summary;
                            state.acts.extend(acts);
                            state.dirty = true;
                        }
                    }
                    Ok(WriterCommand::Update {
                        summary,
                        update,
                        durability,
                        acts,
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
                        state.persisted.summary = *summary;
                        state.acts.extend(acts);
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
                        // Every Session's joins to its Attachments land first,
                        // so the deletion keeps an Attachment another Session
                        // has bound but not yet flushed.
                        let result = flush_sessions(&repository, &mut sessions, None)
                            .and_then(|()| repository.delete_session(session_id));
                        if result.is_ok() {
                            sessions.remove(&session_id);
                            // An act naming the Session either way has
                            // nothing left to stand for.
                            unwritten_acts.retain(|act| {
                                act.sidekick != session_id && act.session_id != session_id
                            });
                        }
                        let _ = durability
                            .send(result.as_ref().map(|_| ()).map_err(ToString::to_string));
                        result?;
                    }
                    Ok(WriterCommand::SaveLandingAgentSelection(selection)) => {
                        repository.save_landing_agent_selection(selection)?;
                    }
                    // A remembered catalog is a nicety the next process starts
                    // from; failing to write one must not cost this process its
                    // Session persistence.
                    Ok(WriterCommand::SaveModelCatalog(remembered)) => {
                        if let Err(error) = repository.save_model_catalog(remembered) {
                            tracing::warn!("could not remember the Model Catalog: {error}");
                        }
                    }
                    // Best-effort on the same terms as a remembered Model
                    // Catalog: a Workspace Icon that fails to persist costs
                    // nothing beyond the next Session in that Workspace
                    // deriving one again.
                    Ok(WriterCommand::SaveWorkspaceIcon { workspace_id, icon }) => {
                        if let Err(error) = repository.write_workspace_icon(
                            workspace_id,
                            icon,
                            WorkspaceWrite::FillAbsence,
                        ) {
                            tracing::warn!("could not save a Workspace Icon: {error}");
                        }
                    }
                    // Best-effort on the same terms as a derived Workspace
                    // Icon: a choice that fails to persist here still stands
                    // in memory for the rest of this process, and only a
                    // restart would ever see the table's stale row again.
                    Ok(WriterCommand::ReplaceWorkspaceIcon { workspace_id, icon }) => {
                        if let Err(error) = repository.write_workspace_icon(
                            workspace_id,
                            icon,
                            WorkspaceWrite::Replace,
                        ) {
                            tracing::warn!("could not save a chosen Workspace Icon: {error}");
                        }
                    }
                    // Best-effort on the same terms as a Workspace's Icon,
                    // whichever way its Description lands.
                    Ok(WriterCommand::SaveWorkspaceDescription {
                        workspace_id,
                        description,
                    }) => {
                        if let Err(error) = repository.write_workspace_description(
                            workspace_id,
                            Some(description),
                            WorkspaceWrite::FillAbsence,
                        ) {
                            tracing::warn!("could not save a Workspace Description: {error}");
                        }
                    }
                    Ok(WriterCommand::ReplaceWorkspaceDescription {
                        workspace_id,
                        description,
                    }) => {
                        if let Err(error) = repository.write_workspace_description(
                            workspace_id,
                            description,
                            WorkspaceWrite::Replace,
                        ) {
                            tracing::warn!("could not save a set Workspace Description: {error}");
                        }
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
                    // Kept until it is written: it is tried at once, and
                    // again at every idle flush until it lands.
                    Ok(WriterCommand::RecordSidekickAct(act)) => {
                        unwritten_acts.push(act);
                        write_unwritten_acts(&repository, &mut sessions, &mut unwritten_acts)?;
                    }
                    Ok(WriterCommand::Shutdown) | Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                        flush_sessions(&repository, &mut sessions, None)?;
                        write_unwritten_acts(&repository, &mut sessions, &mut unwritten_acts)?;
                        break;
                    }
                    Err(std_mpsc::RecvTimeoutError::Timeout) => {
                        flush_sessions(&repository, &mut sessions, None)?;
                        write_unwritten_acts(&repository, &mut sessions, &mut unwritten_acts)?;
                        // Every Session held here has landed its joins, so an
                        // Attachment none is joined to is bound by no stored
                        // Prompt or Message. An upload alone never reaches the
                        // writer, so a quiet Server sweeps by the interval. A
                        // failed sweep leaves its orphans for the next one.
                        if (std::mem::take(&mut worked) || repository.attachment_sweep_due())
                            && let Err(error) =
                                super::attachment_table::sweep_orphaned_attachments(&repository)
                        {
                            tracing::warn!("could not sweep orphaned Attachments: {error}");
                        }
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
    pub(crate) fn location_changed(
        &self,
        session: crate::protocol::Session,
        revision: crate::protocol::SessionRevision,
    ) -> Result<(), StorageError> {
        let (durability, receipt) = std_mpsc::sync_channel(0);
        self.commands
            .send(WriterCommand::LocationChanged {
                session: Box::new(session),
                revision,
                durability,
            })
            .map_err(|_| StorageError::WriterTask("writer is no longer running".to_owned()))?;
        receipt
            .recv()
            .map_err(|_| {
                StorageError::WriterTask("writer stopped before location persistence".to_owned())
            })?
            .map_err(StorageError::WriterTask)
    }

    pub(crate) fn hydrated(&self, persisted: PersistedSession) -> Result<(), StorageError> {
        self.commands
            .send(WriterCommand::Hydrate(Box::new(persisted)))
            .map_err(|_| StorageError::WriterTask("writer is no longer running".to_owned()))
    }

    /// Records a Session just created — a Subagent's child Session together
    /// with what its spawn fixed about it, the Provider's identity for a
    /// native one or that a brokered one is brokered, which lands in the same
    /// flush, as do `acts`, the acts of Sidekicks its creation follows.
    pub(crate) fn created(&self, persisted: PersistedSession, acts: Vec<StoredSidekickAct>) {
        let _ = self.commands.send(WriterCommand::Create {
            persisted: Box::new(persisted),
            acts,
        });
    }

    /// Records catalog-only metadata, and `acts`, the acts of Sidekicks its
    /// change follows, in the same flush. Fire-and-forget on the same terms
    /// as [`Self::created`]: the next idle flush lands it.
    pub(crate) fn summary_changed(&self, summary: SessionSummary, acts: Vec<StoredSidekickAct>) {
        let _ = self.commands.send(WriterCommand::SummaryChanged {
            summary: Box::new(summary),
            acts,
        });
    }

    /// Records an update to a Session, and `acts`, the acts of Sidekicks it
    /// follows, in the same flush.
    pub(crate) fn updated(
        &self,
        summary: SessionSummary,
        update: &SessionUpdate,
        acts: Vec<StoredSidekickAct>,
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
                summary: Box::new(summary),
                update: update.clone(),
                durability,
                acts,
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

    /// Remembers what a Provider just served, off the discovery's own path: a
    /// catalog is worth serving the moment it is known, not once it is stored.
    pub(crate) fn save_model_catalog(&self, remembered: RememberedProviderCatalog) {
        let _ = self
            .commands
            .send(WriterCommand::SaveModelCatalog(remembered));
    }

    /// Records a Workspace's Icon, off the Session store's own commit path:
    /// the in-memory guard against overwriting one is already applied by the
    /// time this fires, so this is purely the durable half landing behind it.
    pub(crate) fn save_workspace_icon(&self, workspace_id: WorkspaceId, icon: String) {
        let _ = self
            .commands
            .send(WriterCommand::SaveWorkspaceIcon { workspace_id, icon });
    }

    /// Records a user's own choice of a Workspace's Icon, replacing whatever
    /// the table already held for it — off the Session store's own commit
    /// path the same way [`Self::save_workspace_icon`] is, but for the
    /// unconditional half of that commit rather than the fill-an-absence one.
    pub(crate) fn replace_workspace_icon(&self, workspace_id: WorkspaceId, icon: String) {
        let _ = self
            .commands
            .send(WriterCommand::ReplaceWorkspaceIcon { workspace_id, icon });
    }

    /// Records a derived Workspace Description where the table holds none,
    /// off the Session store's own commit path the way
    /// [`Self::save_workspace_icon`] records a derived Icon.
    pub(crate) fn save_workspace_description(
        &self,
        workspace_id: WorkspaceId,
        description: WorkspaceDescription,
    ) {
        let _ = self.commands.send(WriterCommand::SaveWorkspaceDescription {
            workspace_id,
            description,
        });
    }

    /// Records a Workspace's Description as it was set, or its absence where
    /// it was cleared, replacing whatever the table held — the unconditional
    /// half, as [`Self::replace_workspace_icon`] is for an Icon.
    pub(crate) fn replace_workspace_description(
        &self,
        workspace_id: WorkspaceId,
        description: Option<WorkspaceDescription>,
    ) {
        let _ = self
            .commands
            .send(WriterCommand::ReplaceWorkspaceDescription {
                workspace_id,
                description,
            });
    }

    /// Records a Sidekick's latest act on a Session that no change to the
    /// Session carries, off the Session store's own path: the act already
    /// stands in memory by the time this fires, and the writer keeps trying
    /// until it is written.
    pub(crate) fn record_sidekick_act(&self, act: StoredSidekickAct) {
        let _ = self.commands.send(WriterCommand::RecordSidekickAct(act));
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
    let mut flushed = sessions
        .iter()
        .filter(|(session_id, state)| {
            state.dirty && only.as_ref().is_none_or(|only| only == *session_id)
        })
        .map(|(session_id, _)| *session_id)
        .collect::<Vec<_>>();
    // An act names its Sidekick's Session, which lands before it, in a
    // transaction of its own.
    let sidekicks = flushed
        .iter()
        .flat_map(|session_id| sessions[session_id].acts.iter().map(|act| act.sidekick))
        .filter(|sidekick| sessions.get(sidekick).is_some_and(|state| state.dirty))
        .collect::<Vec<_>>();
    flushed.retain(|session_id| !sidekicks.contains(session_id));
    let mut ordered = Vec::with_capacity(flushed.len() + sidekicks.len());
    for session_id in sidekicks.into_iter().chain(flushed) {
        if !ordered.contains(&session_id) {
            ordered.push(session_id);
        }
    }
    repository.save_sessions(
        ordered
            .iter()
            .map(|session_id| {
                let state = &sessions[session_id];
                (state.persisted.clone(), state.acts.clone())
            })
            .collect(),
    )?;
    for session_id in ordered {
        let state = sessions
            .get_mut(&session_id)
            .expect("a flushed Session is held");
        state.dirty = false;
        state.acts.clear();
    }
    Ok(())
}

/// Writes every act no change to its Session carried, once both Sessions it
/// names have landed, keeping each one whose write fails for the next
/// attempt rather than losing it. Only a failure to land a Session the act
/// names stops the writer, as any Session write does.
fn write_unwritten_acts(
    repository: &StorageRepository,
    sessions: &mut HashMap<SessionId, WriterState>,
    unwritten: &mut Vec<StoredSidekickAct>,
) -> Result<(), StorageError> {
    if unwritten.is_empty() {
        return Ok(());
    }
    for act in unwritten.iter() {
        flush_sessions(repository, sessions, Some(act.sidekick))?;
        flush_sessions(repository, sessions, Some(act.session_id))?;
    }
    unwritten.retain(|act| match repository.record_sidekick_act(act) {
        Ok(()) => false,
        Err(error) => {
            tracing::warn!("a Sidekick's act is not written yet, and will be tried again: {error}");
            true
        }
    });
    Ok(())
}

fn is_turn_boundary(update: &SessionUpdate) -> bool {
    update.changes.iter().any(|change| match change {
        SessionChange::TurnAdded { turn } => turn.status.is_terminal(),
        SessionChange::TurnStatusChanged { status, .. } => status.is_terminal(),
        SessionChange::SessionStatusChanged { status } => *status == SessionStatus::Idle,
        _ => false,
    })
}
