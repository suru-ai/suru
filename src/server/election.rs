//! The Channel's election, and a Server's standing in it while it runs.
//!
//! A Server owns its Channel by holding the exclusive lock on the
//! `server.lock` file in the Channel's state directory, and is found through
//! the runtime descriptor beside it, which only the lock's holder writes
//! (ADR 0001). Both stand for the Server only while that directory does. A
//! Server whose state directory is removed holds its lock on a file nothing
//! can open any more, so it keeps no successor out, and has published a
//! descriptor no client will read. Nothing else would end it — a Server
//! outlives its clients by design — so it would run on for good, holding its
//! Providers, as test Servers whose temporary directories were deleted did.
//!
//! So a Server confirms, once it has won the lock, that the lock is still the
//! Channel's, and keeps watch while it runs. It stops — the way `suru server
//! stop` stops it — on either of two findings, and on nothing less:
//!
//! - **The lock's file has no name left.** Asked of the file the Server holds
//!   open rather than of its path, so it answers only for that very file: its
//!   last link is gone (Unix), or it is deleted or awaiting deletion
//!   (Windows). The state directory was removed with it in it, or the lock
//!   alone was; either way no other Server would find it.
//! - **The runtime descriptor names another instance.** Read whole and
//!   decoded, so a descriptor being written, quarantined, or missing for a
//!   moment counts for nothing. Only a lock holder writes one, and this
//!   Server's successor writes its own only once this one has let the lock
//!   go, so a descriptor naming another while this Server still serves means
//!   another Server was elected through another lock — in a state directory
//!   made afresh where this one's stood, say, once this one's was moved away.
//!
//! What is deliberately not a finding is anything that fails to answer. A
//! state directory that cannot be read for now — its permissions changed, a
//! home directory or network mount gone away or not yet back — fails to say
//! anything about the lock's path or the descriptor, and the open file still
//! has its name. A directory moved elsewhere, as moving it to the Trash does,
//! leaves the file named and the path unanswered too. Each leaves the Server
//! running, which is what it did before it kept any watch; a moved directory
//! ends it once a Server is elected in a new one and publishes itself there.
//! And once a Server is stopping — replaced, or stopped by a client — it no
//! longer watches at all, so a replacement it is handing over to is never
//! taken for a usurper.
//!
//! Each look runs on a thread nothing waits on, and holds the lock's file
//! only while it asks about it — never while it reads the descriptor — and
//! the election is let go explicitly as the Server finishes stopping, so a
//! look left waiting on a directory that does not answer neither holds up
//! the process ending nor keeps a successor from being elected.
//!
//! Nor does a Server make its state or data directory again in the moments
//! before it notices it is gone: whatever it makes within them, it makes
//! only beneath a root still there (`paths::create_dir_beneath`).
//!
//! An election is also where a Manual stop is made final (ADR 0002). Servers
//! that lose an election wait out the winner's hold, and one still waiting
//! when the winner is stopped with `suru server stop` would take the Channel
//! the moment it is let go, undoing a stop that had reported success. So a
//! Server stopped manually records its stop ([`LastStop`](crate::LastStop))
//! before it lets the lock go, and a Server once elected confirms that no
//! stop has been recorded since the one its launch followed, ending — having
//! made nothing — where one has. The stop a launch follows is read as the
//! launch is asked for — by the launcher before it launches the process, or
//! by `spawn` as it is called in process — never as the Server gets round to
//! starting, so a Server slow to reach the election is held to the stops
//! made after its launch just as one long waiting in it is; and a launch
//! made after the stop reads that stop, so nothing it launches is ended by
//! it.
//!
//! A stop that cannot be recorded — a full disk, a record that cannot be
//! replaced — is reported to the client that asked for it as a failed stop,
//! though the Server stops all the same, as it was asked to. Its lock is let
//! go as ever, so a Server launched before the stop and still waiting may be
//! elected and serve; the failure is reported so that is never mistaken for
//! a stop that took. A stop the operating system asks for by signal has no
//! one to report to, and is only logged.

use std::{
    fs::{File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Weak},
};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use tokio::{sync::watch, time::Duration};
use uuid::Uuid;

use super::{ServerConfig, ShutdownController};
use crate::protocol::{LifecycleState, RuntimeDescriptor, ServerShutdown, ShutdownReason};
use crate::provider::wait_for_shutdown;
use crate::runtime::StopRecord;

/// The Channel's election lock, as one Server holds it open.
pub(super) struct ElectionLock {
    file: File,
}

impl ElectionLock {
    /// Opens the lock at `path`, making the file — never the directory it is
    /// in — where it is missing.
    pub(super) fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        Ok(Self { file })
    }

    /// Takes the lock, waiting up to `handoff` for a hold that is only
    /// draining. The lock belongs to an open file description, and a child
    /// being spawned shares every description its parent has open until its
    /// `exec` closes them. So a lock goes on being held after its owner lets
    /// go, for as long as any spawn begun in that owner's process takes to
    /// finish — which on macOS, where a program is assessed on its first run,
    /// can be a few hundred milliseconds. Only a lock still held once
    /// `handoff` has passed belongs to a server that is running.
    pub(super) async fn take(&self, handoff: Duration) -> io::Result<()> {
        const POLL_INTERVAL: Duration = Duration::from_millis(10);
        let deadline = tokio::time::Instant::now() + handoff;
        loop {
            match self.file.try_lock_exclusive() {
                Ok(()) => return Ok(()),
                Err(error)
                    if error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
                        && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep_until(
                        (tokio::time::Instant::now() + POLL_INTERVAL).min(deadline),
                    )
                    .await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Confirms that the lock this Server has just taken still elects it: that
    /// its file was not removed while the Server waited for it. A Server that
    /// lost an election waits out the winner's hold, and the winner may be
    /// stopped because its state directory is being removed; the lock the
    /// waiting Server then takes is on a file nothing can open, and opening
    /// its storage or publishing a descriptor would make that directory
    /// again. Such a Server ends here instead, having made nothing.
    pub(super) fn confirm(&self) -> Result<()> {
        if !is_named(&self.file).context("inspect the server election lock")? {
            bail!(
                "the channel's state directory or election lock was removed while this server \
                 waited to be elected"
            );
        }
        Ok(())
    }
}

/// `config`, following the Channel's last Manual stop as it is recorded
/// now, unless its launcher already told it the stop it followed. Called as
/// a launch is asked for, before anything else it does, so that a launch
/// slow to be polled, to look at its Providers, or to reach the election is
/// held to every stop made after it was asked for. Where the record cannot
/// be read the launch follows none it can tell a later stop from, and no
/// stop ends it.
pub(super) fn follow_last_stop(config: ServerConfig) -> ServerConfig {
    if config.launched_after_stop().is_some() {
        return config;
    }
    match config.last_stop() {
        Ok(last_stop) => config.launched_after(last_stop),
        Err(error) => {
            tracing::warn!("could not read the channel's last manual stop: {error}");
            config
        }
    }
}

/// Confirms that the Channel this Server has just been elected for was not
/// stopped by a Manual stop recorded since the one its launch followed. Only
/// the lock's holder records a stop, so once this Server holds the lock what
/// it reads here is settled. A record that cannot be read is no finding, as
/// nothing that fails to answer is in an election.
pub(super) fn confirm_not_stopped(config: &ServerConfig) -> Result<()> {
    let Some(followed) = config.launched_after_stop() else {
        return Ok(());
    };
    let now = match config.last_stop() {
        Ok(now) => now,
        Err(error) => {
            tracing::warn!("could not read the channel's last manual stop; serving: {error}");
            return Ok(());
        }
    };
    if let Some(stopped) = followed.superseded_by(now) {
        bail!(
            "the channel's server {stopped} was stopped after this server was launched, and a \
             manual stop is final for every server launched before it"
        );
    }
    Ok(())
}

/// Records the Manual stop of the Server `instance_id` as the Channel's last,
/// at `path`, replacing whatever stop was recorded before whole, so that
/// nothing reads it half written, and flushing it before it answers, so it
/// outlasts whatever becomes of the stopping Server once it lets its lock
/// go. Written into the state directory as it stands: one that is gone
/// is not made again, and with it went every Server that could have read it.
pub(super) fn record_stop(path: &Path, instance_id: Uuid) -> Result<()> {
    let mut contents =
        serde_json::to_vec(&StopRecord { instance_id }).context("encode the manual stop")?;
    contents.push(b'\n');
    super::publish_runtime_file(path, "manual stop record", &contents)
}

/// The Channel's election, held by a Server for as long as this lives: the
/// lock is let go the moment it is dropped, as the Server finishes stopping,
/// however many references to the lock's file are still open — those a look
/// at the Server's standing holds while it waits on a directory that does not
/// answer among them. Only this gives the election up, so a look that never
/// returns keeps a file open but never keeps a successor out.
///
/// It is shared, and is also how a stopping Server keeps to one rule: it
/// writes to the Channel's state directory only while it holds the election,
/// or not at all. Every write its stop makes there — its Manual stop's
/// record, the removal of its runtime descriptor — holds a reference to this
/// while it runs, so the election is let go only once the last of them is
/// done, however long after the Server stopped waiting on it. A write that
/// never finishes keeps the election until the process ends, and ends with
/// it, unlanded: no successor is elected while a write of this Server's may
/// still overwrite or remove what the successor writes.
pub(super) struct Holding(Arc<ElectionLock>);

impl Holding {
    pub(super) fn new(lock: Arc<ElectionLock>) -> Self {
        Self(lock)
    }
}

impl Drop for Holding {
    fn drop(&mut self) {
        if let Err(error) = FileExt::unlock(&self.0.file) {
            tracing::warn!("could not let the server election lock go: {error}");
        }
    }
}

/// What one look at a Server's standing found.
#[derive(Debug)]
struct Look {
    /// Whether the election lock's file still has a name, or why it did not
    /// answer.
    lock: io::Result<bool>,
    /// The instance the runtime descriptor names — none while there is no
    /// descriptor, as there briefly is not while one is being replaced — or
    /// why it could not be read whole and decoded. Not read once the lock is
    /// found removed.
    descriptor: Result<Option<Uuid>, String>,
}

impl Look {
    /// Takes one look, holding the lock's file only for as long as it takes
    /// to ask about it, and not at all while the descriptor is read. `None`
    /// once the Server has let the lock go, when there is nothing to look at.
    fn take(lock: &Weak<ElectionLock>, descriptor_path: &Path) -> Option<Self> {
        let named = is_named(&lock.upgrade()?.file);
        let descriptor = match named {
            Ok(false) => Ok(None),
            _ => published_instance(descriptor_path),
        };
        Some(Self {
            lock: named,
            descriptor,
        })
    }

    /// What the Server `instance_id` has lost its standing to, if this look
    /// found it lost, as the module describes. Anything that did not answer
    /// is no finding.
    fn loss(&self, instance_id: Uuid) -> Option<Loss> {
        if matches!(self.lock, Ok(false)) {
            return Some(Loss::Removed);
        }
        match self.descriptor {
            Ok(Some(published)) if published != instance_id => Some(Loss::Superseded(published)),
            _ => None,
        }
    }
}

/// Why a running Server no longer stands for its Channel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Loss {
    /// The election lock's file has no name left: removed with the state
    /// directory, or alone.
    Removed,
    /// The runtime descriptor names this other instance.
    Superseded(Uuid),
}

impl std::fmt::Display for Loss {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Removed => formatter
                .write_str("its state directory or election lock was removed from under it"),
            Self::Superseded(instance_id) => write!(
                formatter,
                "its runtime descriptor names another server instance ({instance_id})"
            ),
        }
    }
}

/// Keeps watch every `interval`, for as long as the Server runs, on whether
/// it still stands for its Channel, and stops it once it does not. The watch
/// ends as soon as the Server begins stopping for any reason. A lock or
/// descriptor that does not answer is warned of once, as it begins not to,
/// and the Server serves on; a descriptor missing for a moment, as one being
/// replaced is, is not worth a warning.
pub(super) fn watch(
    lock: Weak<ElectionLock>,
    descriptor_path: PathBuf,
    instance_id: Uuid,
    interval: Duration,
    shutdown: ShutdownController,
    mut stopping: watch::Receiver<bool>,
) {
    let interval = interval.max(Duration::from_millis(1));
    tokio::spawn(async move {
        let mut lock_unanswered = false;
        let mut descriptor_unreadable = false;
        loop {
            tokio::select! {
                biased;
                () = wait_for_shutdown(&mut stopping) => return,
                () = tokio::time::sleep(interval) => {}
            }
            // A look at a directory on a mount that has gone away may wait
            // for as long as the mount does, so it is taken on a thread of its
            // own that nothing waits on: not the runtime as the process ends,
            // which waits out every blocking task it began, and not this
            // watch once the Server begins stopping.
            let spawned = super::off_thread("suru-standing", {
                let lock = lock.clone();
                let descriptor_path = descriptor_path.clone();
                move || Look::take(&lock, &descriptor_path)
            });
            let looked = match spawned {
                Ok(looked) => looked,
                Err(error) => {
                    tracing::warn!("could not look at this server's standing: {error}");
                    continue;
                }
            };
            let looked = tokio::select! {
                biased;
                () = wait_for_shutdown(&mut stopping) => return,
                looked = looked => looked,
            };
            let Ok(Some(look)) = looked else {
                return;
            };
            match &look.lock {
                Ok(_) if std::mem::take(&mut lock_unanswered) => {
                    tracing::info!("the server election lock answers again");
                }
                Err(error) if !std::mem::replace(&mut lock_unanswered, true) => {
                    tracing::warn!("the server election lock does not answer; serving on: {error}");
                }
                _ => {}
            }
            match &look.descriptor {
                Ok(_) if std::mem::take(&mut descriptor_unreadable) => {
                    tracing::info!("the runtime descriptor reads again");
                }
                Err(error) if !std::mem::replace(&mut descriptor_unreadable, true) => {
                    tracing::warn!("the runtime descriptor cannot be read; serving on: {error}");
                }
                _ => {}
            }
            let Some(loss) = look.loss(instance_id) else {
                continue;
            };
            if shutdown.lifecycle() == LifecycleState::Ready {
                tracing::warn!(%loss, "this server no longer stands for its channel; stopping");
                shutdown.stand_down(ServerShutdown {
                    instance_id,
                    reason: ShutdownReason::Manual,
                });
            }
            return;
        }
    });
}

/// The instance the runtime descriptor at `path` names, or none where there
/// is no descriptor; any other failure to read it whole and decode it is the
/// error. Opening it may wait as long as whatever is in its place does, so
/// only a look, which nothing waits on, reads it this way.
fn published_instance(path: &Path) -> Result<Option<Uuid>, String> {
    decode_instance(path, File::open(path))
}

/// The instance the runtime descriptor at `path` names, as
/// [`published_instance`] reads it, but read only where it is a regular
/// file, as every descriptor a Server publishes is, and opened without
/// waiting: anything else in its place — a FIFO, which waits for a writer
/// that may never come — is the error. A stopping Server reads its
/// descriptor this way, never waiting on what is not its own.
pub(super) fn published_regular_instance(path: &Path) -> Result<Option<Uuid>, String> {
    decode_instance(path, open_regular_file(path))
}

fn decode_instance(path: &Path, opened: io::Result<File>) -> Result<Option<Uuid>, String> {
    let file = match opened {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("open {path:?}: {error}")),
    };
    serde_json::from_reader::<_, RuntimeDescriptor>(io::BufReader::new(file))
        .map(|descriptor| Some(descriptor.identity.instance_id))
        .map_err(|error| format!("decode {path:?}: {error}"))
}

/// Opens the file at `path` for reading where it is a regular file, as every
/// runtime file a Server publishes is. Opening anything else may never
/// answer — a FIFO put in a descriptor's place waits for a writer that never
/// comes — so on Unix it is opened without waiting, and looked at before
/// anything reads it.
fn open_regular_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "it is not a regular file",
        ));
    }
    Ok(file)
}

/// Whether any name in a directory still leads to the open `file`.
#[cfg(unix)]
fn is_named(file: &File) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    Ok(file.metadata()?.nlink() > 0)
}

/// Whether any name in a directory still leads to the open `file`. A file
/// deleted while open loses its name at once under the POSIX semantics
/// Windows deletes with today, and is left awaiting deletion under the older
/// ones; either way it is on its way out.
#[cfg(windows)]
fn is_named(file: &File) -> io::Result<bool> {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_STANDARD_INFO, FileStandardInfo, GetFileInformationByHandleEx,
    };

    let mut standard = FILE_STANDARD_INFO::default();
    let size = u32::try_from(std::mem::size_of::<FILE_STANDARD_INFO>())
        .expect("FILE_STANDARD_INFO fits a u32");
    // SAFETY: the handle stays open while `file` is borrowed, and `standard`
    // is writable storage of the size passed for the class asked for.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileStandardInfo,
            (&raw mut standard).cast(),
            size,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(!standard.DeletePending && standard.NumberOfLinks > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor_naming(instance_id: Uuid) -> RuntimeDescriptor {
        RuntimeDescriptor::new(
            "http://127.0.0.1:9".to_owned(),
            "token".to_owned(),
            crate::protocol::ServerIdentity {
                instance_id,
                pid: 1,
                protocol_version: crate::protocol::PROTOCOL_VERSION,
                build_identity: "test-build".to_owned(),
            },
        )
    }

    /// A lock whose file is removed while it is held — with its directory —
    /// confirms nothing and is found removed, while one left in place stands.
    #[tokio::test]
    async fn a_lock_whose_file_is_removed_no_longer_elects() {
        let state = tempfile::tempdir().expect("create state directory");
        let lock_path = state.path().join("server.lock");
        let descriptor_path = state.path().join("runtime.json");
        let instance_id = Uuid::new_v4();
        let lock = Arc::new(ElectionLock::open(&lock_path).expect("open the lock"));
        lock.take(Duration::ZERO).await.expect("take the lock");
        let look = || Look::take(&Arc::downgrade(&lock), &descriptor_path).expect("look");

        lock.confirm().expect("a lock in place elects its holder");
        assert_eq!(look().loss(instance_id), None);

        let state_path = state.path().to_owned();
        state.close().expect("remove the state directory");
        assert!(!state_path.exists());
        assert!(lock.confirm().is_err(), "a removed lock elects no one");
        assert_eq!(look().loss(instance_id), Some(Loss::Removed));
        assert!(!state_path.exists(), "inspecting the lock makes nothing");
    }

    /// Only a descriptor read whole and naming another instance is a loss:
    /// one missing, half written, or naming this instance is not — though
    /// one half written is reported as unreadable, and one missing is not.
    #[tokio::test]
    async fn only_a_descriptor_naming_another_instance_supersedes_the_server() {
        let state = tempfile::tempdir().expect("create state directory");
        let descriptor_path = state.path().join("runtime.json");
        let instance_id = Uuid::new_v4();
        let lock =
            Arc::new(ElectionLock::open(&state.path().join("server.lock")).expect("open the lock"));
        lock.take(Duration::ZERO).await.expect("take the lock");
        let look = || Look::take(&Arc::downgrade(&lock), &descriptor_path).expect("look");

        let missing = look();
        assert_eq!(
            missing.loss(instance_id),
            None,
            "a missing descriptor is no finding"
        );
        assert_eq!(missing.descriptor, Ok(None), "nor is it unreadable");
        let own = serde_json::to_vec(&descriptor_naming(instance_id)).expect("encode");
        std::fs::write(&descriptor_path, &own).expect("write own descriptor");
        assert_eq!(
            look().loss(instance_id),
            None,
            "its own descriptor is no finding"
        );

        let another = Uuid::new_v4();
        let other = serde_json::to_vec(&descriptor_naming(another)).expect("encode");
        std::fs::write(&descriptor_path, &other[..other.len() / 2]).expect("write half");
        let half = look();
        assert_eq!(
            half.loss(instance_id),
            None,
            "a half-written descriptor is no finding"
        );
        assert!(half.descriptor.is_err(), "but it is unreadable");

        std::fs::write(&descriptor_path, &other).expect("write another's descriptor");
        assert_eq!(look().loss(instance_id), Some(Loss::Superseded(another)));
    }

    /// Dropping the Holding lets the election go even while something —
    /// a look stuck on a directory that does not answer — keeps the lock's
    /// file open, so a successor is elected all the same.
    #[tokio::test]
    async fn letting_the_election_go_does_not_wait_for_a_look_holding_the_lock_open() {
        let state = tempfile::tempdir().expect("create state directory");
        let lock_path = state.path().join("server.lock");
        let lock = Arc::new(ElectionLock::open(&lock_path).expect("open the lock"));
        lock.take(Duration::ZERO).await.expect("take the lock");
        let stuck_look = lock.clone();
        let holding = Holding::new(lock);

        let successor = ElectionLock::open(&lock_path).expect("open the successor's lock");
        assert!(
            successor.take(Duration::ZERO).await.is_err(),
            "the election is held"
        );
        drop(holding);
        successor
            .take(Duration::ZERO)
            .await
            .expect("the successor is elected while the look still holds the file");
        drop(stuck_look);
    }
}
