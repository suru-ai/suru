//! The background writer that lands Session saves and the rest of the Server's durable state.
//!
//! Streaming paths never wait on it. A commit only notes, beside the Session where its history is
//! held, which rows it moved ([`super::Unsaved`]); nothing is encoded or sent. The writer holds no
//! copy of any Session: at each idle tick it takes what the held Sessions owe through
//! [`HeldSessions`] — encoded from borrows of the one copy of each, under the store's lock, into
//! rows of their own — and lands them once that lock is released, so SQLite I/O never sits in the
//! path of a Provider stream. Where a Turn boundary, a location, a Resume State, or a deletion must
//! wait for storage, the store takes the save itself under the lock it already holds and hands it
//! over with the command. An idle tick that follows work, or that finds the sweep interval passed,
//! also sweeps orphaned Attachments once every Session's joins have landed.
//!
//! The writer never waits on the store's lock, only tries it: a caller holding it may be waiting on
//! the writer.
//!
//! A save writes only what moved since the last one: the rows each committed change added or moved,
//! and the Session's own row. Only a Session storage has never held, or one storage is found out of
//! step with, is written whole.
//!
//! In-memory state is canonical while the Server runs (ADR 0006), so storage refusing a save — a full
//! disk — is storage falling behind rather than the Session failing: the save waits here and is tried
//! again until it lands. Only stopping the Server with a save still refused fails, and loses what
//! storage never took.

use std::{
    collections::{HashSet, VecDeque},
    ops::ControlFlow,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc as std_mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::{
    model_catalog::RememberedProviderCatalog,
    protocol::{
        AgentSelection, SessionChange, SessionId, SessionStatus, SessionUpdate,
        WorkspaceDescription, WorkspaceId,
    },
};

use super::{
    Landing, SessionSave, StorageError, StorageRepository, StoredResumeState, StoredSidekickAct,
    WorkspaceWrite,
};

/// How long the writer waits without a command before an idle tick, and how
/// long nothing held must have moved for that tick to take saves.
pub(crate) const IDLE_FLUSH_DELAY: Duration = Duration::from_millis(100);

/// How long the writer, stopping, waits between tries of the store's lock,
/// reading any command that arrives meanwhile.
const STOP_RETRY: Duration = Duration::from_millis(1);

/// How long the writer, holding a Session storage refused to save, waits
/// before an idle tick tries it again: a full disk is not written to at every
/// tick, and is found to have room within moments of having it.
pub(crate) const SAVE_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Where the writer reads the Sessions it saves: the store holding their
/// histories, which keeps what each owes storage beside it.
pub(crate) trait HeldSessions: Send + Sync {
    /// Takes the save every Session owes storage, encoded where it is held,
    /// with a Sidekick's Session before any whose save carries an act
    /// naming it, writing whole each of `whole` that is held. Never waits:
    /// `None` where the Sessions cannot be read without waiting, since
    /// whoever holds them may be waiting on the writer, or where a `quiet`
    /// is asked for and some Session owing a save moved within it.
    fn take_saves(&self, quiet: Option<Duration>, whole: &[SessionId]) -> Option<TakenSaves>;
}

/// What one take of the held Sessions' saves found.
#[derive(Default)]
pub(crate) struct TakenSaves {
    pub(crate) saves: Vec<SessionSave>,
    /// Why each Session that could not be encoded was not: it keeps owing
    /// its save, which nothing taken here carries.
    pub(crate) unencoded: Vec<StorageError>,
}

/// How a take of the held Sessions' saves went.
enum Take {
    /// Some holder's Sessions could not be read without waiting.
    Busy,
    /// Every Session owing a save was read, and those it could not encode
    /// said why.
    Read { unencoded: Vec<StorageError> },
}

#[derive(Clone)]
pub(crate) struct StorageSink {
    commands: std_mpsc::Sender<WriterCommand>,
    /// Set once the writer begins to stop, after which nothing more a commit
    /// notes would ever be saved.
    stopping: Arc<AtomicBool>,
}

pub(crate) struct StorageWriter {
    commands: std_mpsc::Sender<WriterCommand>,
    task: JoinHandle<Result<(), StorageError>>,
}

enum WriterCommand {
    /// Where the Sessions the writer saves are held, read at every idle tick
    /// and as the writer stops.
    Hold(Arc<dyn HeldSessions>),
    /// Saves taken where the Sessions are held, landed before the reply,
    /// which says whether storage has now taken everything handed over.
    Save {
        saves: Vec<SessionSave>,
        landed: std_mpsc::SyncSender<bool>,
    },
    /// Where a Session works now, landed after the save it owed, if any.
    LocationChanged {
        save: Option<SessionSave>,
        session: Box<crate::protocol::Session>,
        revision: crate::protocol::SessionRevision,
        durability: std_mpsc::SyncSender<Result<(), String>>,
    },
    /// A Session's deletion, after every save the held Sessions owed, so the
    /// deletion keeps an Attachment another Session has bound but not yet
    /// saved.
    Delete {
        saves: Vec<SessionSave>,
        session_id: SessionId,
        durability: std_mpsc::SyncSender<Result<Deletion, String>>,
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
    /// A Session's Resume State, landed after the save the Session owed, if
    /// any, since the state's row names the Session's.
    SaveResumeState {
        save: Option<SessionSave>,
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

/// Whether storage is refusing the saves the writer holds. A refused save
/// waits and is tried again, so the refusal is told once where it begins and
/// once where it ends rather than at every attempt.
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

/// The writer thread's own state. It holds no Session, only the saves
/// handed to it that storage has yet to take.
struct Writer {
    repository: StorageRepository,
    /// Where the Sessions it saves are held: the store's, and any other a
    /// test opens over the same writer.
    held: Vec<Arc<dyn HeldSessions>>,
    /// The saves handed over that storage has yet to take, in the order
    /// they were taken: a Session's saves build on one another, so a later
    /// one never lands before an earlier one, and one storage refuses holds
    /// back every one after it, which may name it.
    pending: VecDeque<SessionSave>,
    /// The Sessions storage was found out of step with, each written whole
    /// at the next save taken of it. A save of one taken before then builds
    /// on what storage does not hold, and is let go.
    rewrite: HashSet<SessionId>,
    /// The acts kept until each is written, however often writing one
    /// fails: those no change to their Session carried, and those whose
    /// Sidekick's Session storage held no row of when their save landed.
    unwritten_acts: Vec<StoredSidekickAct>,
    refusal: Refusal,
    /// How long the writer waits without a command before an idle tick, and
    /// how long nothing held must have moved for that tick to take saves.
    idle_flush_delay: Duration,
    /// Told to every sink once the writer begins to stop.
    stopping: Arc<AtomicBool>,
    /// Whether work arrived or landed since the writer last went idle: the
    /// idle flush ending each burst of work sweeps orphaned Attachments
    /// once, and a quiet tick after it sweeps only once the sweep interval
    /// has passed.
    worked: bool,
}

impl StorageWriter {
    pub(crate) fn spawn(repository: StorageRepository) -> (Self, StorageSink) {
        let (commands, receiver) = std_mpsc::channel();
        let stopping = Arc::new(AtomicBool::new(false));
        let mut writer = Writer {
            stopping: stopping.clone(),
            refusal: Refusal {
                retry_interval: repository.save_retry_interval,
                retry_at: None,
            },
            idle_flush_delay: repository.idle_flush_delay,
            repository,
            held: Vec::new(),
            pending: VecDeque::new(),
            rewrite: HashSet::new(),
            unwritten_acts: Vec::new(),
            worked: false,
        };
        let task = thread::spawn(move || {
            loop {
                match receiver.recv_timeout(writer.idle_flush_delay) {
                    Ok(command) => {
                        writer.worked = true;
                        if writer.handle(command).is_break() {
                            return writer.stop(&receiver);
                        }
                    }
                    Err(std_mpsc::RecvTimeoutError::Timeout) => writer.idle(),
                    Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                        return writer.stop(&receiver);
                    }
                }
            }
        });
        (
            Self {
                commands: commands.clone(),
                task,
            },
            StorageSink { commands, stopping },
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

impl Writer {
    /// Carries out one command, breaking only where it is to stop.
    fn handle(&mut self, command: WriterCommand) -> ControlFlow<()> {
        let repository = self.repository.clone();
        match command {
            WriterCommand::Hold(held) => self.held.push(held),
            WriterCommand::Save { saves, landed } => {
                self.hand_over(saves);
                // The saves stand where they were taken however this goes:
                // one storage refuses waits here and is tried again.
                let _ = self.land();
                let _ = landed.send(self.pending.is_empty() && self.rewrite.is_empty());
            }
            WriterCommand::LocationChanged {
                save,
                session,
                revision,
                durability,
            } => {
                // A location storage refuses is handed back to its caller,
                // which keeps the Session where it was.
                self.hand_over(save);
                let result = self
                    .land()
                    .and_then(|()| repository.save_location(&session, revision));
                let _ = durability.send(result.map_err(|error| error.to_string()));
            }
            WriterCommand::Delete {
                saves,
                session_id,
                durability,
            } => {
                self.hand_over(saves);
                let result = self.land().and_then(|()| {
                    // Another Session storage is out of step with may have
                    // bound an Attachment only this one's rows still join,
                    // which the deletion would reclaim before that Session's
                    // join lands; it is written whole first. This one's own
                    // rows go with it however storage holds them, so its own
                    // rewrite holds nothing up — but stays owed until the
                    // deletion lands, since one that fails leaves it held,
                    // and storage still short of it.
                    let owed = self
                        .rewrite
                        .iter()
                        .filter(|owed| **owed != session_id)
                        .copied()
                        .collect::<Vec<_>>();
                    if !owed.is_empty() {
                        return Ok(Deletion::OwedWhole(owed));
                    }
                    repository.delete_session(session_id)?;
                    self.rewrite.remove(&session_id);
                    // An act naming the Session either way has nothing left
                    // to stand for.
                    self.unwritten_acts
                        .retain(|act| act.sidekick != session_id && act.session_id != session_id);
                    Ok(Deletion::Deleted)
                });
                let _ = durability.send(result.map_err(|error| error.to_string()));
            }
            // Best-effort on the same terms as a remembered Model Catalog: a
            // selection that fails to persist costs only the next process
            // landing on an older one.
            WriterCommand::SaveLandingAgentSelection(selection) => {
                if let Err(error) = repository.save_landing_agent_selection(selection) {
                    tracing::warn!("could not save the landing Agent Selection: {error}");
                }
            }
            // A remembered catalog is a nicety the next process starts from;
            // failing to write one must not cost this process its Session
            // persistence.
            WriterCommand::SaveModelCatalog(remembered) => {
                if let Err(error) = repository.save_model_catalog(remembered) {
                    tracing::warn!("could not remember the Model Catalog: {error}");
                }
            }
            // Best-effort on the same terms as a remembered Model Catalog: a
            // Workspace Icon that fails to persist costs nothing beyond the
            // next Session in that Workspace deriving one again.
            WriterCommand::SaveWorkspaceIcon { workspace_id, icon } => {
                if let Err(error) =
                    repository.write_workspace_icon(workspace_id, icon, WorkspaceWrite::FillAbsence)
                {
                    tracing::warn!("could not save a Workspace Icon: {error}");
                }
            }
            // Best-effort on the same terms as a derived Workspace Icon: a
            // choice that fails to persist here still stands in memory for
            // the rest of this process, and only a restart would ever see the
            // table's stale row again.
            WriterCommand::ReplaceWorkspaceIcon { workspace_id, icon } => {
                if let Err(error) =
                    repository.write_workspace_icon(workspace_id, icon, WorkspaceWrite::Replace)
                {
                    tracing::warn!("could not save a chosen Workspace Icon: {error}");
                }
            }
            // Best-effort on the same terms as a Workspace's Icon, whichever
            // way its Description lands.
            WriterCommand::SaveWorkspaceDescription {
                workspace_id,
                description,
            } => {
                if let Err(error) = repository.write_workspace_description(
                    workspace_id,
                    Some(description),
                    WorkspaceWrite::FillAbsence,
                ) {
                    tracing::warn!("could not save a Workspace Description: {error}");
                }
            }
            WriterCommand::ReplaceWorkspaceDescription {
                workspace_id,
                description,
            } => {
                if let Err(error) = repository.write_workspace_description(
                    workspace_id,
                    description,
                    WorkspaceWrite::Replace,
                ) {
                    tracing::warn!("could not save a set Workspace Description: {error}");
                }
            }
            // Best-effort as the landing it follows is: a path that fails to
            // persist costs only listing the Workspace once no Session works
            // in it, after a restart.
            WriterCommand::RecordWorkspacePath { workspace_id, path } => {
                if let Err(error) = repository.write_workspace_path(workspace_id, path) {
                    tracing::warn!("could not save where a Workspace is presented: {error}");
                }
            }
            WriterCommand::SaveResumeState {
                save,
                state,
                durability,
            } => {
                self.hand_over(save);
                let result = self
                    .land()
                    .and_then(|()| repository.save_resume_state(&state));
                let _ = durability.send(result.map_err(|error| error.to_string()));
            }
            // Kept until it is written: it is tried at once, and again at
            // every idle flush until it lands.
            WriterCommand::RecordSidekickAct(act) => {
                self.unwritten_acts.push(act);
                self.write_unwritten_acts();
            }
            // An act on it still waiting to be written goes with it, so it is
            // not written after it was forgotten.
            WriterCommand::ForgetRemoteSidekickActs {
                sidekick,
                remote,
                session_id,
            } => {
                self.unwritten_acts.retain(|act| {
                    act.origin.remote_name() != Some(remote.as_str())
                        || act.session_id != session_id
                        || sidekick.is_some_and(|sidekick| act.sidekick != sidekick)
                });
                if let Err(error) =
                    repository.forget_remote_sidekick_acts(sidekick, &remote, session_id)
                {
                    tracing::warn!(
                        %session_id,
                        "the acts on a Remote's Session found deleted were not forgotten: {error}"
                    );
                }
            }
            WriterCommand::Shutdown => return ControlFlow::Break(()),
        }
        ControlFlow::Continue(())
    }

    /// Queues `saves` behind those storage has yet to take, letting go of
    /// each of a Session storage is out of step with until the save writing
    /// it whole arrives, which carries everything those would have.
    fn hand_over(&mut self, saves: impl IntoIterator<Item = SessionSave>) {
        for save in saves {
            if save.is_whole() {
                self.rewrite.remove(&save.session_id());
            }
            if self.rewrite.contains(&save.session_id()) {
                self.unwritten_acts.extend(save.into_acts());
            } else {
                self.pending.push_back(save);
            }
        }
    }

    /// Lands every save storage has yet to take, in order, stopping at the
    /// first it refuses, which waits with every one after it.
    fn land(&mut self) -> Result<(), StorageError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let landed = self.land_pending();
        self.refusal.note(&landed);
        landed
    }

    fn land_pending(&mut self) -> Result<(), StorageError> {
        let mut connection = self.repository.save_connection()?;
        while let Some(save) = self.pending.front() {
            match self.repository.land_save(&mut connection, save) {
                Landing::Landed(kept_back) => {
                    let save = self.pending.pop_front().expect("a save was landed");
                    self.landed_acts(save.into_acts(), kept_back);
                    self.worked = true;
                }
                Landing::OutOfStep(found) => {
                    let session_id = save.session_id();
                    tracing::warn!(
                        %session_id,
                        "storage is out of step with the Session, which is written whole at its \
                         next save: {found}"
                    );
                    self.rewrite.insert(session_id);
                    let (gone, kept) = std::mem::take(&mut self.pending)
                        .into_iter()
                        .partition::<Vec<_>, _>(|save| save.session_id() == session_id);
                    self.pending = kept.into();
                    self.unwritten_acts
                        .extend(gone.into_iter().flat_map(SessionSave::into_acts));
                }
                Landing::Refused(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Notes the acts a landed save carried: those it kept back wait to be
    /// written, and an older act waiting on the same Session than one that
    /// landed is let go, so it never writes over the later one.
    fn landed_acts(&mut self, carried: Vec<StoredSidekickAct>, kept_back: Vec<StoredSidekickAct>) {
        for act in carried.iter().filter(|act| !kept_back.contains(act)) {
            self.unwritten_acts.retain(|waiting| {
                waiting.sidekick != act.sidekick
                    || waiting.origin != act.origin
                    || waiting.session_id != act.session_id
                    || waiting.acted_at > act.acted_at
            });
        }
        self.unwritten_acts.extend(kept_back);
    }

    /// Takes the saves the held Sessions owe, once nothing has moved for
    /// `quiet` where one is asked for: none, while some holder's lock is
    /// busy.
    fn take_held(&mut self, quiet: Option<Duration>) -> Take {
        let whole = self.rewrite.iter().copied().collect::<Vec<_>>();
        let mut taken = Vec::new();
        let mut unencoded = Vec::new();
        let mut read = true;
        for held in &self.held {
            match held.take_saves(quiet, &whole) {
                Some(found) => {
                    taken.extend(found.saves);
                    unencoded.extend(found.unencoded);
                }
                None => read = false,
            }
        }
        // A Session asked for whole was written whole by whichever took it,
        // and once every holder has answered, one none took is no longer held
        // to be — unless nothing holds any Session, when nothing could.
        for save in &taken {
            self.rewrite.remove(&save.session_id());
        }
        if read && !self.held.is_empty() {
            self.rewrite.clear();
        }
        self.hand_over(taken);
        if read {
            Take::Read { unencoded }
        } else {
            Take::Busy
        }
    }

    /// An idle tick: what the held Sessions owe lands, then the acts waiting
    /// to be written, then — once every Session has landed its joins —
    /// orphaned Attachments are swept.
    fn idle(&mut self) {
        if self.refusal.holds_idle_flush() {
            return;
        }
        let taken = self.take_held(Some(self.idle_flush_delay));
        let landed = self.land();
        self.write_unwritten_acts();
        // Every Session held has landed its joins, so an Attachment none is
        // joined to is bound by no stored Prompt or Message. A Session that
        // could not be encoded, or that storage was found out of step with
        // and is yet to be written whole, has joins storage may lack. An
        // upload alone never reaches the writer, so a quiet Server sweeps by
        // the interval. A failed sweep leaves its orphans for the next one.
        if matches!(&taken, Take::Read { unencoded } if unencoded.is_empty())
            && landed.is_ok()
            && self.rewrite.is_empty()
            && (std::mem::take(&mut self.worked) || self.repository.attachment_sweep_due())
            && let Err(error) =
                super::attachment_table::sweep_orphaned_attachments(&self.repository)
        {
            tracing::warn!("could not sweep orphaned Attachments: {error}");
        }
    }

    /// Lands everything the held Sessions owe, and every act waiting, before
    /// the writer stops. The store's lock may be held by a caller waiting on
    /// the writer for a command it has yet to read, so the writer reads on
    /// while it waits for the lock rather than wait on it. A Session storage
    /// is found out of step with on the way is taken again whole, until none
    /// is left to be. Nothing is left to try a refused save again, so one
    /// still refused here — or a Session that could not be encoded, or one
    /// still owed whole — is the writer's own failure.
    fn stop(&mut self, receiver: &std_mpsc::Receiver<WriterCommand>) -> Result<(), StorageError> {
        // Told before the last take, which waits for the store's lock, so a
        // commit either lands before that take or is refused.
        self.stopping.store(true, Ordering::SeqCst);
        let mut unencoded = Vec::new();
        // A Session written whole is never out of step, so each Session is
        // taken at most twice: once as it stood, once whole.
        for _ in 0..3 {
            loop {
                match self.take_held(None) {
                    Take::Read { unencoded: found } => {
                        unencoded.extend(found);
                        break;
                    }
                    Take::Busy => match receiver.recv_timeout(STOP_RETRY) {
                        // Asked to stop again, it is stopping already.
                        Ok(command) => {
                            let _ = self.handle(command);
                        }
                        Err(std_mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                            thread::sleep(STOP_RETRY);
                        }
                    },
                }
            }
            self.land()?;
            if self.rewrite.is_empty() {
                break;
            }
        }
        self.write_unwritten_acts();
        if let Some(session_id) = self.rewrite.iter().next() {
            return Err(StorageError::Write {
                session_id: *session_id,
                message: "storage is out of step with the Session, which was never written \
                          whole"
                    .to_owned(),
            });
        }
        match unencoded.into_iter().next() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Writes every act waiting whose Sidekick's Session storage holds,
    /// keeping each whose write fails, or whose Sidekick's Session has yet to
    /// land, for the next attempt rather than losing it.
    fn write_unwritten_acts(&mut self) {
        let repository = &self.repository;
        self.unwritten_acts
            .retain(|act| match repository.record_sidekick_act(act) {
                Ok(written) => !written,
                Err(error) => {
                    tracing::warn!(
                        "a Sidekick's act is not written yet, and will be tried again: {error}"
                    );
                    true
                }
            });
    }
}

impl StorageSink {
    /// Refuses where the writer has begun to stop, so a change committed
    /// from here on, which nothing would ever save, is refused rather than
    /// standing in memory alone.
    pub(crate) fn running(&self) -> Result<(), StorageError> {
        if self.stopping.load(Ordering::SeqCst) {
            return Err(StorageError::WriterTask(
                "writer is no longer running".to_owned(),
            ));
        }
        Ok(())
    }

    /// Has the writer read the Sessions it saves where `held` holds them.
    pub(crate) fn hold(&self, held: Arc<dyn HeldSessions>) {
        let _ = self.commands.send(WriterCommand::Hold(held));
    }

    /// Lands `saves`, taken where their Sessions are held, answering once
    /// storage has been tried with them: whether it has now taken everything
    /// the writer was handed. One storage refuses waits and is tried again.
    pub(crate) fn save(&self, saves: Vec<SessionSave>) -> Result<bool, StorageError> {
        let (landed, receipt) = std_mpsc::sync_channel(0);
        self.commands
            .send(WriterCommand::Save { saves, landed })
            .map_err(|_| StorageError::WriterTask("writer is no longer running".to_owned()))?;
        receipt.recv().map_err(|_| {
            StorageError::WriterTask("writer stopped before confirming a save".to_owned())
        })
    }

    /// Stores a Session whole, as creating it does, with `acts`, the acts of
    /// Sidekicks its creation follows, without a store to hold it.
    #[cfg(test)]
    pub(crate) fn created(&self, persisted: super::PersistedSession, acts: Vec<StoredSidekickAct>) {
        let save = super::Unsaved::created(acts)
            .take(super::SessionRows {
                summary: &persisted.summary,
                snapshot: &persisted.snapshot,
                subagent_identity: persisted.subagent_identity.as_ref(),
                brokered: persisted.brokered,
            })
            .expect("a fixture encodes");
        self.save(save.into_iter().collect())
            .expect("the writer is running");
    }

    /// Persists where a Session works now, after `save`, what the Session
    /// owed storage before it moved.
    pub(crate) fn location_changed(
        &self,
        save: Option<SessionSave>,
        session: crate::protocol::Session,
        revision: crate::protocol::SessionRevision,
    ) -> Result<(), StorageError> {
        let (durability, receipt) = std_mpsc::sync_channel(0);
        self.commands
            .send(WriterCommand::LocationChanged {
                save,
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

    /// Saves a Session's Resume State after `save`, what the Session owed
    /// storage, whose row the state's names.
    pub(crate) fn save_resume_state(
        &self,
        save: Option<SessionSave>,
        state: StoredResumeState,
    ) -> Result<(), StorageError> {
        let (durability, receipt) = std_mpsc::sync_channel(0);
        self.commands
            .send(WriterCommand::SaveResumeState {
                save,
                state,
                durability,
            })
            .map_err(|_| StorageError::WriterTask("writer is no longer running".to_owned()))?;
        receipt
            .recv()
            .map_err(|_| {
                StorageError::WriterTask("writer stopped before confirming Resume State".to_owned())
            })?
            .map_err(StorageError::WriterTask)
    }

    /// Deletes a Session's rows once `saves`, what every held Session owed
    /// storage, have landed — unless storage is out of step with some
    /// Session still to be written whole, which the answer names, and which
    /// must be saved whole before the deletion is asked for again.
    pub(crate) fn deleted(
        &self,
        saves: Vec<SessionSave>,
        session_id: SessionId,
    ) -> Result<Deletion, StorageError> {
        let (durability, receipt) = std_mpsc::sync_channel(0);
        self.commands
            .send(WriterCommand::Delete {
                saves,
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

/// How a Session's deletion went, where storage could be reached.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Deletion {
    Deleted,
    /// Nothing was deleted: storage is out of step with these Sessions, each
    /// to be saved whole first, so the deletion reclaims no Attachment a
    /// join of theirs storage lacks still binds.
    OwedWhole(Vec<SessionId>),
}

/// Whether `update` ends a Turn, whose save its commit waits for: a Turn
/// once settled is never lost to a crash (ADR 0006).
pub(crate) fn is_turn_boundary(update: &SessionUpdate) -> bool {
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
            SessionStandingInputs, SessionSummary, SessionTimestamp, TranscriptItem, Turn, TurnId,
            TurnStatus, Workspace,
        },
        session_projection::{apply_update, land_update},
        storage::{PersistedSession, SessionRows, Unsaved},
    };

    /// One Session held the way the store holds one — its history, with what
    /// it owes storage beside it — which the writer reads as it reads the
    /// store. The test keeps its own copy of the Session to read storage
    /// back against.
    #[derive(Clone)]
    struct Held(Arc<std::sync::Mutex<(PersistedSession, Unsaved)>>);

    impl HeldSessions for Held {
        fn take_saves(&self, _quiet: Option<Duration>, whole: &[SessionId]) -> Option<TakenSaves> {
            let mut held = self.0.try_lock().ok()?;
            let (persisted, unsaved) = &mut *held;
            if whole.contains(&persisted.snapshot.session.id) {
                unsaved.rewrite_whole();
            }
            Some(TakenSaves {
                saves: take(persisted, unsaved).into_iter().collect(),
                unencoded: Vec::new(),
            })
        }
    }

    fn take(persisted: &PersistedSession, unsaved: &mut Unsaved) -> Option<SessionSave> {
        unsaved
            .take(SessionRows {
                summary: &persisted.summary,
                snapshot: &persisted.snapshot,
                subagent_identity: persisted.subagent_identity.as_ref(),
                brokered: persisted.brokered,
            })
            .unwrap()
    }

    /// A held Session that never goes quiet for an idle tick, so only a save
    /// taken unasked — a Turn boundary's, or the writer's as it stops —
    /// carries what it owes.
    struct Restless(Held);

    impl HeldSessions for Restless {
        fn take_saves(&self, quiet: Option<Duration>, whole: &[SessionId]) -> Option<TakenSaves> {
            match quiet {
                Some(_) => None,
                None => self.0.take_saves(None, whole),
            }
        }
    }

    impl Held {
        /// As [`Self::hydrated`], read by the writer only as it stops.
        fn restless(
            sink: &StorageSink,
            persisted: PersistedSession,
            recovered_activities: Vec<usize>,
        ) -> Self {
            let unsaved = Unsaved::hydrated(&persisted.snapshot, recovered_activities);
            let held = Self(Arc::new(std::sync::Mutex::new((persisted, unsaved))));
            sink.hold(Arc::new(Restless(held.clone())));
            held
        }

        /// A Session just read back from storage, as `persisted` stands but
        /// for the Activities at `recovered_activities`, which the reading
        /// moved: the writer reads it from here on.
        fn hydrated(
            sink: &StorageSink,
            persisted: PersistedSession,
            recovered_activities: Vec<usize>,
        ) -> Self {
            let unsaved = Unsaved::hydrated(&persisted.snapshot, recovered_activities);
            let held = Self(Arc::new(std::sync::Mutex::new((persisted, unsaved))));
            sink.hold(Arc::new(held.clone()));
            held
        }

        /// Commits `update` as the store does — in place, noting which rows
        /// it moved, with `summary` as the commit leaves it — waiting for its
        /// save where it ends a Turn.
        fn commit(&self, sink: &StorageSink, summary: &SessionSummary, update: &SessionUpdate) {
            let save = {
                let mut held = self.0.lock().unwrap();
                let (persisted, unsaved) = &mut *held;
                let landed = land_update(&mut persisted.snapshot, update).unwrap();
                persisted.summary = summary.clone();
                unsaved.note(&update.changes, &landed);
                is_turn_boundary(update)
                    .then(|| take(persisted, unsaved))
                    .flatten()
            };
            if let Some(save) = save {
                sink.save(vec![save]).unwrap();
            }
        }

        /// Saves what the Session owes storage at once, as an idle tick does.
        fn save(&self, sink: &StorageSink) -> bool {
            let save = {
                let mut held = self.0.lock().unwrap();
                let (persisted, unsaved) = &mut *held;
                take(persisted, unsaved)
            };
            sink.save(save.into_iter().collect()).unwrap()
        }
    }

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
        let (writer, sink) = StorageWriter::spawn(repository.clone());
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
        let (writer, sink) = StorageWriter::spawn(repository.clone());
        let holder = Held::hydrated(&sink, held.clone(), Vec::new());

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
        holder.commit(&sink, &held.summary, &appended);
        holder.save(&sink);
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
        holder.commit(&sink, &held.summary, &settled);
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
        let (writer, sink) = StorageWriter::spawn(repository.clone());
        let holder = Held::hydrated(&sink, held.clone(), Vec::new());

        let appended = update(
            &held.snapshot,
            vec![SessionChange::MessageContentAppended {
                message_id: streaming,
                content: " at the writer".to_owned(),
            }],
        );
        apply_update(&mut held.snapshot, &appended).unwrap();
        holder.commit(&sink, &held.summary, &appended);
        assert!(
            !holder.save(&sink),
            "a save built on a row storage lost does not land"
        );

        // The writer reads the Session whole at its next idle tick.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let reloaded = repository.session(session_id).await.unwrap().unwrap();
            if reloaded.snapshot == held.snapshot {
                break;
            }
            assert!(Instant::now() < deadline, "the Session is written whole");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // And deleting it still takes every row with it.
        sink.deleted(Vec::new(), session_id).unwrap();
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

    /// The writer stopping finds storage out of step with a Session and
    /// writes it whole before it stops, rather than stopping on a save it
    /// let go of.
    #[tokio::test]
    async fn a_session_found_out_of_step_as_the_writer_stops_is_written_whole_first() {
        let directory = tempfile::tempdir().unwrap();
        let repository = StorageRepository::open(directory.path()).await.unwrap();
        let mut held = stored(&repository, session(directory.path())).await;
        let session_id = held.snapshot.session.id;
        let streaming = held.snapshot.messages[2].id;
        super::super::connect(&repository.database_path)
            .unwrap()
            .batch_execute(&format!("DELETE FROM messages WHERE id = '{streaming}';"))
            .unwrap();
        let (writer, sink) = StorageWriter::spawn(repository.clone());
        let holder = Held::restless(&sink, held.clone(), Vec::new());

        let appended = update(
            &held.snapshot,
            vec![SessionChange::MessageContentAppended {
                message_id: streaming,
                content: " at the writer".to_owned(),
            }],
        );
        apply_update(&mut held.snapshot, &appended).unwrap();
        holder.commit(&sink, &held.summary, &appended);
        writer.shutdown().await.unwrap();

        let reloaded = repository.session(session_id).await.unwrap().unwrap();
        assert_eq!(reloaded.snapshot, held.snapshot);
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

        let (writer, sink) = StorageWriter::spawn(repository.clone());
        Held::hydrated(&sink, held.clone(), vec![position]);
        writer.shutdown().await.unwrap();
        let reloaded = repository.session(session_id).await.unwrap().unwrap();
        assert_eq!(
            stored_outcome(&reloaded),
            QuestionnaireOutcome::Pending,
            "reading a Session back writes nothing of its own"
        );

        let (writer, sink) = StorageWriter::spawn(repository.clone());
        let holder = Held::hydrated(&sink, held.clone(), vec![position]);
        let observed = update(
            &held.snapshot,
            vec![SessionChange::TurnOutputObserved {
                turn_id: working,
                observed_at: SessionTimestamp(50),
            }],
        );
        apply_update(&mut held.snapshot, &observed).unwrap();
        holder.commit(&sink, &held.summary, &observed);
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

    /// Commits `changes` to the Session `held` as the store would, noting
    /// beside `holder`'s copy which rows they moved, saves it, and checks
    /// storage now reads the Session back as `held` stands.
    async fn commit_and_check(
        repository: &StorageRepository,
        sink: &StorageSink,
        holder: &Held,
        held: &mut PersistedSession,
        changes: Vec<SessionChange>,
    ) {
        let committed = update(&held.snapshot, changes);
        apply_update(&mut held.snapshot, &committed).unwrap();
        held.summary.session = held.snapshot.session.clone();
        held.summary.title.clone_from(&held.snapshot.title);
        held.summary.icon.clone_from(&held.snapshot.icon);
        held.summary.updated_at = SessionTimestamp(held.snapshot.revision.0 * 10);
        holder.commit(sink, &held.summary, &committed);
        assert!(holder.save(sink), "storage takes the save");
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
        let (writer, sink) = StorageWriter::spawn(repository.clone());
        let holder = Held::hydrated(&sink, held.clone(), Vec::new());

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
        commit_and_check(&repository, &sink, &holder, &mut held, additions).await;

        // Each stored row moved by one kind of change alone.
        commit_and_check(
            &repository,
            &sink,
            &holder,
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
            &holder,
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
            &holder,
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
            &holder,
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
            &holder,
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
            &holder,
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
            &holder,
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
