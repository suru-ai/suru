//! The background writer that coalesces Session state into durable rows.
//!
//! Streaming paths hand work to [`StorageSink`] and move on. The writer thread owns the projected
//! copy of each accessed or newly created Session, marks it dirty, and flushes on Turn boundaries and idle ticks so SQLite
//! I/O never sits in the path of a Provider stream. An idle tick that follows work, or that finds the sweep interval
//! passed, also sweeps orphaned Attachments once the flush has landed every Session's joins.
//!
//! A flush writes only what moved since the last one: the rows each committed change added or
//! moved, noted as the change arrives, and the Session's own row. Only a Session storage has
//! never held, or one storage is found out of step with, is written whole.
//!
//! In-memory state is canonical while the Server runs (ADR 0006), so storage refusing a save — a full disk — is
//! storage falling behind rather than the Session failing: the Session stays dirty here and is tried again until it
//! lands. Only stopping the Server with a save still refused fails, and loses what storage never took.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::mpsc as std_mpsc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::{
    model_catalog::RememberedProviderCatalog,
    protocol::{
        AgentSelection, Outlook, SessionChange, SessionId, SessionStatus, SessionSummary,
        SessionUpdate, WorkspaceDescription, WorkspaceId,
    },
    session_projection::land_update,
};

use super::{
    PersistedSession, SessionSave, StorageError, StorageRepository, StoredResumeState,
    StoredSidekickAct, UnsavedRows, WorkspaceWrite,
};

const IDLE_FLUSH_DELAY: Duration = Duration::from_millis(100);

/// How long the writer, holding a Session storage refused to save, waits
/// before an idle tick tries it again: a full disk is not written to at every
/// tick, and is found to have room within moments of having it.
pub(crate) const SAVE_RETRY_INTERVAL: Duration = Duration::from_secs(5);

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
    /// A Session read back from storage, and the positions of the
    /// Activities its reading moved without a change saying so, which storage
    /// still holds as they were.
    Hydrate {
        persisted: Box<PersistedSession>,
        recovered_activities: Vec<usize>,
    },
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
        /// Told once the save a Turn boundary asks for has been tried,
        /// however it went.
        durability: Option<std_mpsc::SyncSender<()>>,
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
    /// Where a Workspace whose Icon or Description just landed is presented,
    /// written into the row that landing made or kept.
    RecordWorkspacePath {
        workspace_id: WorkspaceId,
        path: PathBuf,
    },
    SaveResumeState {
        state: StoredResumeState,
        durability: std_mpsc::SyncSender<Result<(), String>>,
    },
    /// A Sidekick's latest act on a Session that no change to that Session
    /// carries — every act on a Remote's Session among them.
    RecordSidekickAct(StoredSidekickAct),
    /// Every Sidekick's act on a Remote's Session found deleted there — or
    /// the one Sidekick's alone, where it names one.
    ForgetRemoteSidekickActs {
        sidekick: Option<SessionId>,
        remote: String,
        session_id: SessionId,
    },
    Shutdown,
}

struct WriterState {
    persisted: PersistedSession,
    /// Whether the Session owes storage a save.
    dirty: bool,
    /// What that save writes. Rows a reading of the Session moved may wait
    /// here while it owes nothing, until something else it owes lands them.
    unsaved: UnsavedRows,
    /// The acts of Sidekicks on this Session that the changes not yet
    /// flushed follow, which land in the same transaction as those changes.
    acts: Vec<StoredSidekickAct>,
}

/// Whether storage is refusing the Sessions the writer holds. A refused
/// Session stays dirty and is tried again, so the refusal is told once where
/// it begins and once where it ends rather than at every attempt.
struct Refusal {
    retry_interval: Duration,
    /// When an idle tick may next try what storage refused, while it is
    /// refusing.
    retry_at: Option<Instant>,
}

impl Refusal {
    /// Notes how a save of held Sessions went.
    fn note(&mut self, saved: &Result<(), StorageError>) {
        match saved {
            Ok(()) => {
                if self.retry_at.take().is_some() {
                    tracing::info!("storage is taking Session saves again");
                }
            }
            Err(error) => {
                if self.retry_at.is_none() {
                    tracing::error!(
                        "storage is refusing Session saves, which are tried again until it takes \
                         them: {error}"
                    );
                }
                self.retry_at = Some(Instant::now() + self.retry_interval);
            }
        }
    }

    /// Whether an idle tick leaves what storage refused for a later one.
    fn holds_idle_flush(&self) -> bool {
        self.retry_at
            .is_some_and(|retry_at| Instant::now() < retry_at)
    }
}

impl StorageWriter {
    pub(crate) fn spawn(
        repository: StorageRepository,
        restored: &[PersistedSession],
    ) -> (Self, StorageSink) {
        let (commands, receiver) = std_mpsc::channel();
        // What storage holds of a Session handed over at the start is not
        // the writer's to know, so its first save writes it whole.
        let mut sessions = restored
            .iter()
            .cloned()
            .map(|persisted| {
                (
                    persisted.snapshot.session.id,
                    WriterState {
                        persisted,
                        dirty: false,
                        unsaved: UnsavedRows::whole(),
                        acts: Vec::new(),
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        // The acts no change to their Session carried, kept until each is
        // written, however often writing one fails.
        let mut unwritten_acts = Vec::<StoredSidekickAct>::new();
        let mut refusal = Refusal {
            retry_interval: repository.save_retry_interval,
            retry_at: None,
        };
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
                    Ok(WriterCommand::Hydrate {
                        persisted,
                        recovered_activities,
                    }) => {
                        sessions
                            .entry(persisted.snapshot.session.id)
                            .or_insert_with(|| {
                                let mut unsaved = UnsavedRows::stored(&persisted.snapshot);
                                unsaved.moved_activities(recovered_activities);
                                WriterState {
                                    persisted: *persisted,
                                    dirty: false,
                                    unsaved,
                                    acts: Vec::new(),
                                }
                            });
                    }
                    Ok(WriterCommand::Create { persisted, acts }) => {
                        let persisted = *persisted;
                        sessions.insert(
                            persisted.snapshot.session.id,
                            WriterState {
                                persisted,
                                dirty: true,
                                unsaved: UnsavedRows::whole(),
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
                            let held = (
                                state.persisted.snapshot.session.clone(),
                                state.persisted.snapshot.revision,
                                state.persisted.summary.session.clone(),
                                state.dirty,
                            );
                            state.persisted.snapshot.session = (*session).clone();
                            state.persisted.snapshot.revision = revision;
                            state.persisted.summary.session = (*session).clone();
                            state.dirty = true;
                            let flushed = flush_sessions(
                                &repository,
                                &mut sessions,
                                Some(session.id),
                                &mut refusal,
                            );
                            // A location storage refuses is handed back to
                            // its caller, which keeps the Session where it
                            // was. So does the writer, or the Session's next
                            // update would not follow what is held here.
                            if flushed.is_err()
                                && let Some(state) = sessions.get_mut(&session.id)
                            {
                                (
                                    state.persisted.snapshot.session,
                                    state.persisted.snapshot.revision,
                                    state.persisted.summary.session,
                                    state.dirty,
                                ) = held;
                            }
                            flushed
                        } else {
                            repository.save_location(&session, revision)
                        };
                        let _ = durability.send(result.map_err(|error| error.to_string()));
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
                        let landed = land_update(&mut state.persisted.snapshot, &update).map_err(
                            |error| {
                                StorageError::WriterTask(format!(
                                    "project update for Session {session_id}: {error:#}"
                                ))
                            },
                        )?;
                        state.unsaved.note(&update.changes, &landed);
                        state.persisted.summary = *summary;
                        state.acts.extend(acts);
                        state.dirty = true;
                        if is_turn_boundary(&update) {
                            // The update stands in memory however its save
                            // goes: a Session storage refuses stays dirty
                            // and is tried again.
                            let _ = flush_sessions(
                                &repository,
                                &mut sessions,
                                Some(session_id),
                                &mut refusal,
                            );
                            if let Some(durability) = durability {
                                let _ = durability.send(());
                            }
                        }
                    }
                    Ok(WriterCommand::Delete {
                        session_id,
                        durability,
                    }) => {
                        // Every Session's joins to its Attachments land first,
                        // so the deletion keeps an Attachment another Session
                        // has bound but not yet flushed.
                        let result = flush_sessions(&repository, &mut sessions, None, &mut refusal)
                            .and_then(|()| repository.delete_session(session_id));
                        if result.is_ok() {
                            sessions.remove(&session_id);
                            // An act naming the Session either way has
                            // nothing left to stand for.
                            unwritten_acts.retain(|act| {
                                act.sidekick != session_id && act.session_id != session_id
                            });
                        }
                        let _ = durability.send(result.map_err(|error| error.to_string()));
                    }
                    // Best-effort on the same terms as a remembered Model
                    // Catalog: a selection that fails to persist costs only
                    // the next process landing on an older one.
                    Ok(WriterCommand::SaveLandingAgentSelection(selection)) => {
                        if let Err(error) = repository.save_landing_agent_selection(selection) {
                            tracing::warn!("could not save the landing Agent Selection: {error}");
                        }
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
                    // Best-effort as the landing it follows is: a path that
                    // fails to persist costs only listing the Workspace once
                    // no Session works in it, after a restart.
                    Ok(WriterCommand::RecordWorkspacePath { workspace_id, path }) => {
                        if let Err(error) = repository.write_workspace_path(workspace_id, path) {
                            tracing::warn!(
                                "could not save where a Workspace is presented: {error}"
                            );
                        }
                    }
                    Ok(WriterCommand::SaveResumeState { state, durability }) => {
                        let result = flush_sessions(
                            &repository,
                            &mut sessions,
                            Some(state.session_id),
                            &mut refusal,
                        )
                        .and_then(|()| repository.save_resume_state(&state));
                        if result.is_ok()
                            && let Some(session) = sessions.get_mut(&state.session_id)
                        {
                            session
                                .persisted
                                .resume_states
                                .insert(state.provider, state.resume_state);
                        }
                        let _ = durability.send(result.map_err(|error| error.to_string()));
                    }
                    // Kept until it is written: it is tried at once, and
                    // again at every idle flush until it lands.
                    Ok(WriterCommand::RecordSidekickAct(act)) => {
                        unwritten_acts.push(act);
                        let _ = write_unwritten_acts(
                            &repository,
                            &mut sessions,
                            &mut unwritten_acts,
                            &mut refusal,
                        );
                    }
                    // An act on it still waiting to be written goes with it,
                    // so it is not written after it was forgotten.
                    Ok(WriterCommand::ForgetRemoteSidekickActs {
                        sidekick,
                        remote,
                        session_id,
                    }) => {
                        unwritten_acts.retain(|act| {
                            act.origin.remote_name() != Some(remote.as_str())
                                || act.session_id != session_id
                                || sidekick.is_some_and(|sidekick| act.sidekick != sidekick)
                        });
                        if let Err(error) =
                            repository.forget_remote_sidekick_acts(sidekick, &remote, session_id)
                        {
                            tracing::warn!(
                                %session_id,
                                "the acts on a Remote's Session found deleted were not forgotten: \
                                 {error}"
                            );
                        }
                    }
                    // Nothing is left to try a refused save again, so one
                    // still refused here is the writer's own failure.
                    Ok(WriterCommand::Shutdown) | Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                        flush_sessions(&repository, &mut sessions, None, &mut refusal)?;
                        write_unwritten_acts(
                            &repository,
                            &mut sessions,
                            &mut unwritten_acts,
                            &mut refusal,
                        )?;
                        break;
                    }
                    Err(std_mpsc::RecvTimeoutError::Timeout) => {
                        if refusal.holds_idle_flush() {
                            continue;
                        }
                        let flushed =
                            flush_sessions(&repository, &mut sessions, None, &mut refusal)
                                .and_then(|()| {
                                    write_unwritten_acts(
                                        &repository,
                                        &mut sessions,
                                        &mut unwritten_acts,
                                        &mut refusal,
                                    )
                                });
                        // Every Session held here has landed its joins, so an
                        // Attachment none is joined to is bound by no stored
                        // Prompt or Message. An upload alone never reaches the
                        // writer, so a quiet Server sweeps by the interval. A
                        // failed sweep leaves its orphans for the next one.
                        if flushed.is_ok()
                            && (std::mem::take(&mut worked) || repository.attachment_sweep_due())
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

    /// Hands over a Session just read back from storage, as clean: storage
    /// holds it, save for the Activities at `recovered_activities`, which the
    /// reading moved without a change saying so. Those land with whatever
    /// the Session next owes storage, and never on their own (ADR 0022).
    pub(crate) fn hydrated(
        &self,
        persisted: PersistedSession,
        recovered_activities: Vec<usize>,
    ) -> Result<(), StorageError> {
        self.commands
            .send(WriterCommand::Hydrate {
                persisted: Box::new(persisted),
                recovered_activities,
            })
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
    /// follows, in the same flush. One ending a Turn waits for its save to be
    /// tried, and stands whether or not storage took it: the writer keeps a
    /// Session storage refuses and tries it again.
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
            receipt.recv().map_err(|_| {
                StorageError::WriterTask(
                    "writer stopped before confirming a Turn boundary".to_owned(),
                )
            })?;
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

    /// Records where a Workspace is presented into the row its Icon or
    /// Description just landed in, off the Session store's own commit path
    /// and behind that landing's own write.
    pub(crate) fn record_workspace_path(&self, workspace_id: WorkspaceId, path: PathBuf) {
        let _ = self
            .commands
            .send(WriterCommand::RecordWorkspacePath { workspace_id, path });
    }

    /// Records a Sidekick's latest act on a Session that no change to the
    /// Session carries, off the Session store's own path: the act already
    /// stands in memory by the time this fires, and the writer keeps trying
    /// until it is written.
    pub(crate) fn record_sidekick_act(&self, act: StoredSidekickAct) {
        let _ = self.commands.send(WriterCommand::RecordSidekickAct(act));
    }

    /// Forgets every Sidekick's act on the Session `session_id` of the
    /// Remote `remote`, which the Remote no longer holds.
    pub(crate) fn forget_remote_sidekick_acts(&self, remote: String, session_id: SessionId) {
        let _ = self.commands.send(WriterCommand::ForgetRemoteSidekickActs {
            sidekick: None,
            remote,
            session_id,
        });
    }

    /// Forgets the act of the Sidekick of `sidekick` alone on the Session
    /// `session_id` of the Remote `remote`.
    pub(crate) fn forget_sidekick_act(
        &self,
        sidekick: SessionId,
        remote: String,
        session_id: SessionId,
    ) {
        let _ = self.commands.send(WriterCommand::ForgetRemoteSidekickActs {
            sidekick: Some(sidekick),
            remote,
            session_id,
        });
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

/// Saves what storage lacks of every dirty Session, or of `only` that one and
/// the Sidekicks' Sessions its acts name, each in a transaction of its own.
/// Those storage refuses stay dirty for the next attempt, and so does every
/// one after the first refused, which may name it.
fn flush_sessions(
    repository: &StorageRepository,
    sessions: &mut HashMap<SessionId, WriterState>,
    only: Option<SessionId>,
    refusal: &mut Refusal,
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
    if ordered.is_empty() {
        return Ok(());
    }
    let saves = ordered
        .iter()
        .map(|session_id| {
            let state = &sessions[session_id];
            SessionSave {
                persisted: &state.persisted,
                unsaved: &state.unsaved,
                acts: &state.acts,
            }
        })
        .collect::<Vec<_>>();
    let (landed, saved) = repository.save_sessions(&saves);
    for session_id in &ordered[..landed] {
        let state = sessions
            .get_mut(session_id)
            .expect("a flushed Session is held");
        state.dirty = false;
        state.unsaved.saved(&state.persisted.snapshot);
        state.acts.clear();
    }
    refusal.note(&saved);
    saved
}

/// Writes every act no change to its Session carried, once both Sessions it
/// names have landed, keeping each one whose write fails for the next
/// attempt rather than losing it. A Session an act names that storage refuses
/// keeps every act waiting with it.
fn write_unwritten_acts(
    repository: &StorageRepository,
    sessions: &mut HashMap<SessionId, WriterState>,
    unwritten: &mut Vec<StoredSidekickAct>,
    refusal: &mut Refusal,
) -> Result<(), StorageError> {
    if unwritten.is_empty() {
        return Ok(());
    }
    for act in unwritten.iter() {
        flush_sessions(repository, sessions, Some(act.sidekick), refusal)?;
        // A Remote's Session is never stored here.
        if act.origin == Outlook::Local {
            flush_sessions(repository, sessions, Some(act.session_id), refusal)?;
        }
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use diesel::{QueryableByName, RunQueryDsl, connection::SimpleConnection, sql_types::Text};

    use super::*;
    use crate::{
        protocol::{
            Activity, ActivityId, ActivityStatus, Message, MessageId, MessageRole, MessageStatus,
            ModelAvailability, Prompt, PromptDelivery, PromptId, PromptOrder, PromptStatus,
            ProviderId, QuestionnaireOutcome, Session, SessionRevision, SessionSnapshot,
            SessionStandingInputs, SessionTimestamp, TranscriptItem, Turn, TurnId, TurnStatus,
            Workspace,
        },
        provider::ProviderResumeState,
        session_projection::apply_update,
    };

    /// One write a trigger saw land on a table of a Session's history.
    #[derive(Debug, Eq, PartialEq, QueryableByName)]
    struct Written {
        #[diesel(sql_type = Text)]
        kind: String,
        #[diesel(sql_type = Text)]
        op: String,
        #[diesel(sql_type = Text)]
        id: String,
    }

    fn written(kind: &str, op: &str, id: impl ToString) -> Written {
        Written {
            kind: kind.to_owned(),
            op: op.to_owned(),
            id: id.to_string(),
        }
    }

    /// Has every write to a table of a Session's history logged from here on.
    fn log_history_writes(repository: &StorageRepository) {
        let mut sql = "CREATE TABLE write_log (\
             seq INTEGER PRIMARY KEY AUTOINCREMENT, kind TEXT NOT NULL, \
             op TEXT NOT NULL, id TEXT NOT NULL);"
            .to_owned();
        for table in ["prompts", "turns", "messages", "activities"] {
            for (op, row) in [("insert", "NEW"), ("update", "NEW"), ("delete", "OLD")] {
                sql.push_str(&format!(
                    "CREATE TRIGGER log_{table}_{op} AFTER {op} ON {table} BEGIN \
                     INSERT INTO write_log (kind, op, id) VALUES ('{table}', '{op}', {row}.id); \
                     END;"
                ));
            }
        }
        super::super::connect(&repository.database_path)
            .unwrap()
            .batch_execute(&sql)
            .unwrap();
    }

    /// The writes logged since last asked, in the order they landed.
    fn take_history_writes(repository: &StorageRepository) -> Vec<Written> {
        let mut connection = super::super::connect(&repository.database_path).unwrap();
        let writes = diesel::sql_query("SELECT kind, op, id FROM write_log ORDER BY seq")
            .load::<Written>(&mut connection)
            .unwrap();
        connection.batch_execute("DELETE FROM write_log;").unwrap();
        writes
    }

    /// Saving a Resume State lands every change to its Session before it,
    /// which flushes the Session at once.
    fn flush(sink: &StorageSink, session_id: SessionId) {
        sink.save_resume_state(StoredResumeState {
            session_id,
            provider: ProviderId::new("codex"),
            resume_state: ProviderResumeState::new(serde_json::json!({ "thread": "t" })),
        })
        .unwrap();
    }

    fn update(snapshot: &SessionSnapshot, changes: Vec<SessionChange>) -> SessionUpdate {
        SessionUpdate {
            session_id: snapshot.session.id,
            revision: SessionRevision(snapshot.revision.0 + 1),
            changes,
        }
    }

    fn turn(prompt_id: Option<PromptId>, status: TurnStatus) -> Turn {
        Turn {
            id: TurnId::new(),
            prompt_id,
            compaction_requested: false,
            agent: None,
            status,
            started_at: Some(SessionTimestamp(10)),
            settled_at: status.is_terminal().then_some(SessionTimestamp(20)),
            last_output_at: None,
            usage: None,
            cost: None,
            cost_basis: None,
            cost_details: None,
        }
    }

    fn message(
        turn_id: TurnId,
        role: MessageRole,
        status: MessageStatus,
        content: &str,
    ) -> Message {
        Message {
            id: MessageId::new(),
            turn_id,
            role,
            status,
            content: content.to_owned(),
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
            truncated: false,
            author: None,
        }
    }

    fn command(turn_id: TurnId, status: ActivityStatus, output: &str) -> Activity {
        Activity::Command {
            id: ActivityId::new(),
            turn_id,
            status,
            command: "cargo test".to_owned(),
            cwd: None,
            output: output.to_owned(),
            output_truncated: false,
            exit_status: (status != ActivityStatus::Active).then_some(0),
        }
    }

    fn transcript(snapshot: &mut SessionSnapshot, item: TranscriptItem) {
        snapshot.transcript.push(item);
    }

    /// A Session one settled Turn in and one still working: a Prompt, two
    /// Turns, three Messages — the last still streaming — and two commands
    /// between them.
    fn session(workspace: &Path) -> PersistedSession {
        let session = Session {
            checkout: None,
            context_fill: None,
            id: SessionId::new(),
            execution_directory: crate::protocol::ExecutionDirectory {
                path: workspace.to_owned(),
            },
            workspace: Workspace::directory(workspace.to_owned()),
            agent_selection: None,
            agent_selection_availability: ModelAvailability::Unavailable,
            approval_posture: None,
            status: SessionStatus::Active,
            working_since: None,
            monitoring_since: None,
            parent: None,
            begun_by: None,
        };
        let prompt = Prompt {
            id: PromptId::new(),
            text: "Map the storage writer".to_owned(),
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
            delivery: PromptDelivery::Queue,
            admission_order: PromptOrder(1),
            status: PromptStatus::Delivered,
            withdrawal: None,
            author: None,
            taken: None,
        };
        let settled = turn(Some(prompt.id), TurnStatus::Completed);
        let working = turn(None, TurnStatus::Active);
        let mut snapshot = SessionSnapshot {
            title: "writer fixture".to_owned(),
            icon: None,
            session: session.clone(),
            revision: SessionRevision(3),
            prompts: vec![prompt],
            turns: vec![settled.clone(), working.clone()],
            messages: Vec::new(),
            activities: Vec::new(),
            transcript: Vec::new(),
            subagent_interventions: Vec::new(),
            pending_approvals: Vec::new(),
            submitting_approvals: Vec::new(),
            pending_approvals_revision: SessionRevision(0),
            watches: Vec::new(),
            waiting_on_subagents: None,
            subagent_usage: None,
            total_cost: None,
            own_cost: None,
            attachments: Vec::new(),
        };
        for (entry, item) in [
            (
                Some(message(
                    settled.id,
                    MessageRole::User,
                    MessageStatus::Completed,
                    "Map the storage writer",
                )),
                None,
            ),
            (
                None,
                Some(command(settled.id, ActivityStatus::Completed, "ok")),
            ),
            (
                Some(message(
                    settled.id,
                    MessageRole::Agent,
                    MessageStatus::Completed,
                    "It rewrites everything.",
                )),
                None,
            ),
            (
                Some(message(
                    working.id,
                    MessageRole::Agent,
                    MessageStatus::Streaming,
                    "Looking",
                )),
                None,
            ),
            (None, Some(command(working.id, ActivityStatus::Active, ""))),
        ] {
            if let Some(message) = entry {
                transcript(
                    &mut snapshot,
                    TranscriptItem::Message {
                        message_id: message.id,
                    },
                );
                snapshot.messages.push(message);
            }
            if let Some(activity) = item {
                transcript(
                    &mut snapshot,
                    TranscriptItem::Activity {
                        activity_id: activity.id(),
                    },
                );
                snapshot.activities.push(activity);
            }
        }
        PersistedSession::created(
            SessionSummary {
                checkout_state: None,
                session,
                title: snapshot.title.clone(),
                icon: None,
                settled_at: None,
                standing_inputs: SessionStandingInputs::default(),
                total_usage: None,
                own_cost: None,
                remote_subsessions: Vec::new(),
                created_at: SessionTimestamp(1),
                updated_at: SessionTimestamp(2),
            },
            snapshot,
        )
    }

    /// Stores `persisted` whole, the way creating it does, and reads it
    /// back as hydrating it would.
    async fn stored(
        repository: &StorageRepository,
        persisted: PersistedSession,
    ) -> PersistedSession {
        let session_id = persisted.snapshot.session.id;
        let (writer, sink) = StorageWriter::spawn(repository.clone(), &[]);
        sink.created(persisted, Vec::new());
        writer.shutdown().await.unwrap();
        repository.session(session_id).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn a_flush_writes_only_the_rows_that_moved_since_the_last_one() {
        let directory = tempfile::tempdir().unwrap();
        let repository = StorageRepository::open(directory.path()).await.unwrap();
        let mut held = stored(&repository, session(directory.path())).await;
        let session_id = held.snapshot.session.id;
        log_history_writes(&repository);
        let (writer, sink) = StorageWriter::spawn(repository.clone(), &[]);
        sink.hydrated(held.clone(), Vec::new()).unwrap();

        // More of the Message still streaming moves that one row alone.
        let streaming = held.snapshot.messages[2].id;
        let appended = update(
            &held.snapshot,
            vec![SessionChange::MessageContentAppended {
                message_id: streaming,
                content: " at the writer".to_owned(),
            }],
        );
        apply_update(&mut held.snapshot, &appended).unwrap();
        held.summary.updated_at = SessionTimestamp(30);
        sink.updated(held.summary.clone(), &appended, Vec::new())
            .unwrap();
        flush(&sink, session_id);
        assert_eq!(
            take_history_writes(&repository),
            vec![written("messages", "update", streaming)]
        );

        // Rows added land after those already stored, in their own order,
        // beside the rows they move, and nothing the last flush wrote is
        // written again.
        let working = held.snapshot.turns[1].id;
        let running = held.snapshot.activities[1].id();
        let added_command = command(working, ActivityStatus::Active, "");
        let added_message = message(working, MessageRole::Agent, MessageStatus::Streaming, "");
        let (added_command_id, added_message_id) = (added_command.id(), added_message.id);
        let settled = update(
            &held.snapshot,
            vec![
                SessionChange::ActivityAdded {
                    activity: added_command,
                },
                SessionChange::CommandOutputAppended {
                    activity_id: running,
                    content: "test result: ok".to_owned(),
                },
                SessionChange::MessageAdded {
                    message: added_message,
                },
                SessionChange::TurnStatusChanged {
                    turn_id: working,
                    status: TurnStatus::Completed,
                    settled_at: Some(SessionTimestamp(40)),
                },
            ],
        );
        apply_update(&mut held.snapshot, &settled).unwrap();
        held.summary.updated_at = SessionTimestamp(40);
        // A Turn's settling waits for its save.
        sink.updated(held.summary.clone(), &settled, Vec::new())
            .unwrap();
        let mut writes = take_history_writes(&repository);
        writes.sort_by(|left, right| (&left.kind, &left.op).cmp(&(&right.kind, &right.op)));
        assert_eq!(
            writes,
            vec![
                written("activities", "insert", added_command_id),
                written("activities", "update", running),
                written("messages", "insert", added_message_id),
                written("turns", "update", working),
            ]
        );

        writer.shutdown().await.unwrap();
        let reloaded = repository.session(session_id).await.unwrap().unwrap();
        assert_eq!(reloaded.snapshot, held.snapshot);
        assert_eq!(reloaded.summary.updated_at, SessionTimestamp(40));
        assert_eq!(
            take_history_writes(&repository),
            Vec::new(),
            "stopping the writer owes storage nothing more"
        );
    }

    #[tokio::test]
    async fn a_session_storage_is_out_of_step_with_is_written_whole() {
        let directory = tempfile::tempdir().unwrap();
        let repository = StorageRepository::open(directory.path()).await.unwrap();
        let mut held = stored(&repository, session(directory.path())).await;
        let session_id = held.snapshot.session.id;
        let streaming = held.snapshot.messages[2].id;
        // Storage loses a row the writer saved.
        super::super::connect(&repository.database_path)
            .unwrap()
            .batch_execute(&format!("DELETE FROM messages WHERE id = '{streaming}';"))
            .unwrap();
        let (writer, sink) = StorageWriter::spawn(repository.clone(), &[]);
        sink.hydrated(held.clone(), Vec::new()).unwrap();

        let appended = update(
            &held.snapshot,
            vec![SessionChange::MessageContentAppended {
                message_id: streaming,
                content: " at the writer".to_owned(),
            }],
        );
        apply_update(&mut held.snapshot, &appended).unwrap();
        sink.updated(held.summary.clone(), &appended, Vec::new())
            .unwrap();
        flush(&sink, session_id);

        let reloaded = repository.session(session_id).await.unwrap().unwrap();
        assert_eq!(reloaded.snapshot, held.snapshot);

        // And deleting it still takes every row with it.
        sink.deleted(session_id).unwrap();
        writer.shutdown().await.unwrap();
        assert!(repository.session(session_id).await.unwrap().is_none());
        let mut connection = super::super::connect(&repository.database_path).unwrap();
        for table in ["prompts", "turns", "messages", "activities"] {
            let rows = diesel::sql_query(format!("SELECT COUNT(*) AS value FROM {table}"))
                .get_result::<super::super::CountRow>(&mut connection)
                .unwrap();
            assert_eq!(rows.value, 0, "{table} keeps no row of a deleted Session");
        }
    }

    #[tokio::test]
    async fn what_a_reading_recovered_lands_with_the_next_save_and_never_on_its_own() {
        let directory = tempfile::tempdir().unwrap();
        let repository = StorageRepository::open(directory.path()).await.unwrap();
        let mut persisted = session(directory.path());
        let working = persisted.snapshot.turns[1].id;
        let asked = ActivityId::new();
        persisted.snapshot.activities.push(Activity::Questionnaire {
            id: asked,
            turn_id: working,
            questionnaire: crate::protocol::Questionnaire {
                id: crate::protocol::QuestionnaireId::new(),
                questions: Vec::new(),
            },
            outcome: QuestionnaireOutcome::Pending,
            answer: None,
            author: None,
            asked_at: None,
            settled_at: None,
        });
        persisted
            .snapshot
            .transcript
            .push(TranscriptItem::Activity { activity_id: asked });
        let mut held = stored(&repository, persisted).await;
        let session_id = held.snapshot.session.id;
        let position = held.snapshot.activities.len() - 1;
        let Activity::Questionnaire { outcome, .. } = &mut held.snapshot.activities[position]
        else {
            unreachable!("the fixture's last Activity is its Questionnaire");
        };
        *outcome = QuestionnaireOutcome::Unavailable;
        let stored_outcome =
            |persisted: &PersistedSession| match &persisted.snapshot.activities[position] {
                Activity::Questionnaire { outcome, .. } => *outcome,
                _ => unreachable!("the Questionnaire keeps its place"),
            };

        let (writer, sink) = StorageWriter::spawn(repository.clone(), &[]);
        sink.hydrated(held.clone(), vec![position]).unwrap();
        writer.shutdown().await.unwrap();
        let reloaded = repository.session(session_id).await.unwrap().unwrap();
        assert_eq!(
            stored_outcome(&reloaded),
            QuestionnaireOutcome::Pending,
            "reading a Session back writes nothing of its own"
        );

        let (writer, sink) = StorageWriter::spawn(repository.clone(), &[]);
        sink.hydrated(held.clone(), vec![position]).unwrap();
        let observed = update(
            &held.snapshot,
            vec![SessionChange::TurnOutputObserved {
                turn_id: working,
                observed_at: SessionTimestamp(50),
            }],
        );
        apply_update(&mut held.snapshot, &observed).unwrap();
        sink.updated(held.summary.clone(), &observed, Vec::new())
            .unwrap();
        writer.shutdown().await.unwrap();
        let reloaded = repository.session(session_id).await.unwrap().unwrap();
        assert_eq!(stored_outcome(&reloaded), QuestionnaireOutcome::Unavailable);
        assert_eq!(reloaded.snapshot, held.snapshot);
    }

    /// The Session as storage reads it back: what is stored of the Session
    /// held in memory, without the Approvals standing open in it, which are
    /// derived again at every read.
    fn stored_reading(snapshot: &SessionSnapshot) -> SessionSnapshot {
        SessionSnapshot {
            pending_approvals: Vec::new(),
            submitting_approvals: Vec::new(),
            pending_approvals_revision: SessionRevision(0),
            ..snapshot.clone()
        }
    }

    /// Commits `changes` to the Session `held`, hands them to the writer as
    /// the store would, flushes, and checks storage now reads the Session
    /// back as `held` stands.
    async fn commit_and_check(
        repository: &StorageRepository,
        sink: &StorageSink,
        held: &mut PersistedSession,
        changes: Vec<SessionChange>,
    ) {
        let committed = update(&held.snapshot, changes);
        apply_update(&mut held.snapshot, &committed).unwrap();
        held.summary.session = held.snapshot.session.clone();
        held.summary.title.clone_from(&held.snapshot.title);
        held.summary.icon.clone_from(&held.snapshot.icon);
        held.summary.updated_at = SessionTimestamp(held.snapshot.revision.0 * 10);
        sink.updated(held.summary.clone(), &committed, Vec::new())
            .unwrap();
        flush(sink, held.snapshot.session.id);
        let reloaded = repository
            .session(held.snapshot.session.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reloaded.snapshot,
            stored_reading(&held.snapshot),
            "storage reads back the Session as revision {} left it",
            held.snapshot.revision.0
        );
    }

    /// Every change kind that moves a stored row is noted against that row.
    /// A row's payload carries its whole entity, so a change whose kind went
    /// unnoted is masked by any other change to the same entity in the same
    /// flush: each kind is therefore made, in some flushed batch, the only
    /// change to touch its entity, and storage is read back after every
    /// flush, so a kind left unnoted fails where it was left.
    #[tokio::test]
    async fn every_change_to_a_stored_row_lands_through_incremental_saves() {
        let directory = tempfile::tempdir().unwrap();
        let repository = StorageRepository::open(directory.path()).await.unwrap();
        let mut persisted = session(directory.path());
        // A Subagent's Session, so its Turn may observe the Subagent's Agent.
        persisted.snapshot.session.parent = Some(SessionId::new());
        persisted.summary.session.parent = persisted.snapshot.session.parent;
        let mut held = stored(&repository, persisted).await;
        let session_id = held.snapshot.session.id;
        let (writer, sink) = StorageWriter::spawn(repository.clone(), &[]);
        sink.hydrated(held.clone(), Vec::new()).unwrap();

        let delivered = held.snapshot.prompts[0].id;
        let (settled, working) = (held.snapshot.turns[0].id, held.snapshot.turns[1].id);
        let streaming = held.snapshot.messages[2].id;
        let running = held.snapshot.activities[1].id();
        let pending = |order| Prompt {
            id: PromptId::new(),
            text: format!("Prompt {order}"),
            skill_invocations: Vec::new(),
            attachments: Vec::new(),
            delivery: PromptDelivery::Queue,
            admission_order: PromptOrder(order),
            status: PromptStatus::Pending,
            withdrawal: None,
            author: None,
            taken: None,
        };
        let (promoted, delivering, withdrawn) = (pending(2), pending(3), pending(4));
        let (promoted, delivering, withdrawn) = (
            (promoted.id, promoted),
            (delivering.id, delivering),
            (withdrawn.id, withdrawn),
        );
        let approval = || Activity::Approval {
            id: ActivityId::new(),
            turn_id: working,
            approval: crate::protocol::Approval {
                id: crate::protocol::ApprovalId::new(),
                subject: crate::protocol::ApprovalSubject::Network {
                    host_or_url: "crates.io".to_owned(),
                },
                reason: Some("fetch a crate".to_owned()),
            },
            tool_activity_id: None,
            detail_truncated: false,
            outcome: crate::protocol::ApprovalOutcome::Pending,
            decision: None,
            follow_up_error: None,
            asked_at: Some(SessionTimestamp(11)),
        };
        let questionnaire = || Activity::Questionnaire {
            id: ActivityId::new(),
            turn_id: working,
            questionnaire: crate::protocol::Questionnaire {
                id: crate::protocol::QuestionnaireId::new(),
                questions: Vec::new(),
            },
            outcome: QuestionnaireOutcome::Pending,
            answer: None,
            author: None,
            asked_at: Some(SessionTimestamp(12)),
            settled_at: None,
        };
        let added = vec![
            approval(),
            approval(),
            questionnaire(),
            questionnaire(),
            Activity::FileChange {
                id: ActivityId::new(),
                turn_id: working,
                status: ActivityStatus::Active,
                changes: vec![crate::protocol::FileChange::Add {
                    path: "src/lib.rs".into(),
                }],
            },
            Activity::ToolCall {
                id: ActivityId::new(),
                turn_id: working,
                status: ActivityStatus::Active,
                name: "search".to_owned(),
                server: Some("docs".to_owned()),
                input: String::new(),
                input_truncated: false,
                output: String::new(),
                output_truncated: false,
                omitted_parts: 0,
            },
            Activity::Reasoning {
                id: ActivityId::new(),
                turn_id: working,
                status: ActivityStatus::Active,
                title: None,
                content: String::new(),
                content_truncated: false,
                duration_ms: None,
            },
            Activity::Subagent {
                id: ActivityId::new(),
                turn_id: working,
                status: ActivityStatus::Active,
                name: "explorer".to_owned(),
                description: "map the seams".to_owned(),
                model: None,
                session_id: SessionId::new(),
                brokered: false,
                duration_ms: None,
                delegated_at: Some(SessionTimestamp(13)),
            },
            Activity::Compaction {
                id: ActivityId::new(),
                turn_id: working,
                status: ActivityStatus::Active,
                trigger: crate::protocol::CompactionTrigger::Automatic,
                instructions: None,
                before_tokens: Some(9_000),
                after_tokens: None,
                error: None,
                summary: None,
                summary_truncated: false,
            },
            Activity::Subsession {
                id: ActivityId::new(),
                turn_id: working,
                session_id: SessionId::new(),
                origin: None,
                origin_fingerprint: None,
                title: "New Session".to_owned(),
                prompt: "Tidy the listing".to_owned(),
            },
            command(working, ActivityStatus::Active, ""),
            Activity::Status {
                id: ActivityId::new(),
                turn_id: working,
                text: "Reconnecting".to_owned(),
            },
            Activity::Error {
                id: ActivityId::new(),
                turn_id: working,
                text: "The tool failed".to_owned(),
            },
            Activity::WatchOutcome {
                id: ActivityId::new(),
                turn_id: working,
                status: crate::protocol::WatchOutcomeStatus::Completed,
                description: "the build".to_owned(),
                summary: Some("it passed".to_owned()),
            },
        ];
        let ids = added.iter().map(Activity::id).collect::<Vec<_>>();
        let [
            accepted,
            decided,
            asked,
            answered,
            file_change,
            tool_call,
            reasoning,
            subagent,
            compaction,
            subsession,
            truncated,
            ..,
        ] = ids[..]
        else {
            unreachable!("thirteen Activities were added");
        };
        let replying = message(working, MessageRole::Agent, MessageStatus::Streaming, "");
        let replying_id = replying.id;
        let agent = |model: &str| crate::protocol::AgentIdentity {
            agent: crate::protocol::AgentId::new("default"),
            selection: crate::protocol::AgentSelection {
                provider: ProviderId::new("codex"),
                model: crate::protocol::ModelId::new(model),
                options: Vec::new(),
            },
        };

        // Every kind of row added, and the stored Turn observing its Agent.
        let mut additions = vec![
            SessionChange::PromptAdded { prompt: promoted.1 },
            SessionChange::PromptAdded {
                prompt: delivering.1,
            },
            SessionChange::PromptAdded {
                prompt: withdrawn.1,
            },
            SessionChange::MessageAdded { message: replying },
            SessionChange::SubagentAgentChanged {
                turn_id: working,
                agent: agent("gpt-5"),
            },
        ];
        additions.extend(
            added
                .into_iter()
                .map(|activity| SessionChange::ActivityAdded { activity }),
        );
        commit_and_check(&repository, &sink, &mut held, additions).await;

        // Each stored row moved by one kind of change alone.
        commit_and_check(
            &repository,
            &sink,
            &mut held,
            vec![
                SessionChange::PromptTaken {
                    prompt_id: delivered,
                    taking: crate::protocol::PromptTaking {
                        turn_id: settled,
                        taken_at: Some(SessionTimestamp(14)),
                    },
                },
                SessionChange::PromptDeliveryChanged {
                    prompt_id: promoted.0,
                    delivery: PromptDelivery::Steer,
                },
                SessionChange::PromptStatusChanged {
                    prompt_id: delivering.0,
                    status: PromptStatus::Delivered,
                },
                SessionChange::PromptWithdrawn {
                    prompt_id: withdrawn.0,
                    withdrawal: crate::protocol::PromptWithdrawal::CompactionUnfinished {
                        turn_id: working,
                    },
                },
                SessionChange::TurnAgentChanged {
                    turn_id: working,
                    agent: agent("gpt-5-mini"),
                },
                SessionChange::MessageContentAppended {
                    message_id: streaming,
                    content: " further".to_owned(),
                },
                SessionChange::MessageTruncated {
                    message_id: replying_id,
                },
                SessionChange::DecisionAccepted {
                    activity_id: accepted,
                },
                SessionChange::ApprovalSettled {
                    activity_id: decided,
                    outcome: crate::protocol::ApprovalOutcome::Decided,
                    decision: Some(crate::protocol::Decision::Accept),
                },
                SessionChange::QuestionnaireAccepted { activity_id: asked },
                SessionChange::QuestionnaireSettled {
                    activity_id: answered,
                    outcome: QuestionnaireOutcome::Answered,
                    answer: Some(crate::protocol::Answer {
                        questions: Vec::new(),
                    }),
                    author: None,
                    settled_at: Some(SessionTimestamp(15)),
                },
                SessionChange::CommandOutputAppended {
                    activity_id: running,
                    content: "running 4 tests".to_owned(),
                },
                SessionChange::CommandOutputTruncated {
                    activity_id: truncated,
                },
                SessionChange::FileChangeUpdated {
                    activity_id: file_change,
                    changes: vec![crate::protocol::FileChange::Update {
                        path: "src/lib.rs".into(),
                        moved_to: Some("src/main.rs".into()),
                    }],
                },
                SessionChange::ToolCallInputChanged {
                    activity_id: tool_call,
                    input: "{\"query\":\"sqlite upsert\"}".to_owned(),
                    input_truncated: false,
                },
                SessionChange::ReasoningTitleChanged {
                    activity_id: reasoning,
                    title: "Weighing the seam".to_owned(),
                },
                SessionChange::SubagentDescriptionChanged {
                    activity_id: subagent,
                    description: "map every seam".to_owned(),
                },
                SessionChange::CompactionSettled {
                    activity_id: compaction,
                    status: ActivityStatus::Completed,
                    before_tokens: Some(9_000),
                    after_tokens: None,
                    error: None,
                    summary: Some("What came before".to_owned()),
                    summary_truncated: false,
                },
                SessionChange::SubsessionTitleChanged {
                    activity_id: subsession,
                    title: "Tidy the listing".to_owned(),
                },
            ],
        )
        .await;

        commit_and_check(
            &repository,
            &sink,
            &mut held,
            vec![
                SessionChange::TurnUsageChanged {
                    turn_id: working,
                    usage: crate::protocol::Usage {
                        output_tokens: Some(42),
                        ..Default::default()
                    },
                    cost: None,
                    cost_basis: None,
                    cost_coverage: None,
                    cost_is_partial: false,
                    cost_recorded_at: None,
                },
                SessionChange::MessageCompleted {
                    message_id: streaming,
                },
                SessionChange::ApprovalFollowUpFailed {
                    activity_id: decided,
                    error: "the Provider went away".to_owned(),
                },
                SessionChange::ApprovalSettled {
                    activity_id: accepted,
                    outcome: crate::protocol::ApprovalOutcome::Decided,
                    decision: Some(crate::protocol::Decision::Decline),
                },
                SessionChange::QuestionnaireSettled {
                    activity_id: asked,
                    outcome: QuestionnaireOutcome::Answered,
                    answer: Some(crate::protocol::Answer {
                        questions: Vec::new(),
                    }),
                    author: None,
                    settled_at: Some(SessionTimestamp(16)),
                },
                SessionChange::CommandStatusChanged {
                    activity_id: running,
                    status: ActivityStatus::Completed,
                    exit_status: Some(0),
                },
                SessionChange::FileChangeStatusChanged {
                    activity_id: file_change,
                    status: ActivityStatus::Completed,
                },
                SessionChange::ToolCallOutputAppended {
                    activity_id: tool_call,
                    content: "three results".to_owned(),
                },
                SessionChange::ReasoningContentAppended {
                    activity_id: reasoning,
                    content: "The writer notes positions.".to_owned(),
                },
                SessionChange::SubagentModelChanged {
                    activity_id: subagent,
                    model: crate::protocol::ModelId::new("gpt-5-mini"),
                },
                SessionChange::CompactionAfterMeasured {
                    activity_id: compaction,
                    after_tokens: 2_000,
                },
            ],
        )
        .await;

        commit_and_check(
            &repository,
            &sink,
            &mut held,
            vec![
                SessionChange::TurnOutputObserved {
                    turn_id: working,
                    observed_at: SessionTimestamp(17),
                },
                SessionChange::ToolCallOutputTruncated {
                    activity_id: tool_call,
                },
                SessionChange::ReasoningContentTruncated {
                    activity_id: reasoning,
                },
                SessionChange::SubagentStatusChanged {
                    activity_id: subagent,
                    status: ActivityStatus::Completed,
                    duration_ms: Some(1_200),
                },
                SessionChange::CommandStatusChanged {
                    activity_id: truncated,
                    status: ActivityStatus::Failed,
                    exit_status: Some(1),
                },
                SessionChange::MessageCompleted {
                    message_id: replying_id,
                },
            ],
        )
        .await;

        commit_and_check(
            &repository,
            &sink,
            &mut held,
            vec![
                SessionChange::ToolCallStatusChanged {
                    activity_id: tool_call,
                    status: ActivityStatus::Completed,
                    omitted_parts: 1,
                },
                SessionChange::ReasoningStatusChanged {
                    activity_id: reasoning,
                    status: ActivityStatus::Completed,
                    duration_ms: Some(800),
                },
            ],
        )
        .await;

        commit_and_check(
            &repository,
            &sink,
            &mut held,
            vec![SessionChange::TurnStatusChanged {
                turn_id: working,
                status: TurnStatus::Completed,
                settled_at: Some(SessionTimestamp(18)),
            }],
        )
        .await;

        // A Turn begun after all that, its rows added after those stored.
        let next = turn(Some(delivering.0), TurnStatus::Active);
        let next_id = next.id;
        let next_command = command(next_id, ActivityStatus::Active, "");
        let next_command_id = next_command.id();
        commit_and_check(
            &repository,
            &sink,
            &mut held,
            vec![
                SessionChange::TitleChanged {
                    title: "Incremental saves".to_owned(),
                    icon: Some("database".to_owned()),
                },
                SessionChange::TurnAdded { turn: next },
                SessionChange::MessageAdded {
                    message: message(
                        next_id,
                        MessageRole::User,
                        MessageStatus::Completed,
                        "Prompt 3",
                    ),
                },
                SessionChange::ActivityAdded {
                    activity: next_command,
                },
            ],
        )
        .await;
        commit_and_check(
            &repository,
            &sink,
            &mut held,
            vec![
                SessionChange::CommandOutputAppended {
                    activity_id: next_command_id,
                    content: "compiling".to_owned(),
                },
                SessionChange::TurnStatusChanged {
                    turn_id: next_id,
                    status: TurnStatus::Failed,
                    settled_at: Some(SessionTimestamp(19)),
                },
            ],
        )
        .await;

        writer.shutdown().await.unwrap();
        let reloaded = repository.session(session_id).await.unwrap().unwrap();
        let fresh = tempfile::tempdir().unwrap();
        let fresh = StorageRepository::open(fresh.path()).await.unwrap();
        let whole = stored(&fresh, held.clone()).await;
        assert_eq!(
            reloaded.snapshot, whole.snapshot,
            "saving as it went stores what saving the Session whole does"
        );
        assert_eq!(reloaded.summary, whole.summary);
        assert_eq!(reloaded.snapshot, stored_reading(&held.snapshot));
    }
}
