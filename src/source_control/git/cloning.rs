//! Cloning a Repository URL into a new directory: Git's ordinary clone —
//! neither shallow nor recursing into submodules, nor reaching for a
//! transport that runs a command of the URL's own — run through the runner
//! every other Git command is, with nothing it may ask anyone.
//!
//! Git makes the clone out of sight, within a hidden staging directory
//! beside its destination, which discovery passes over and nobody mistakes
//! for a Workspace. That directory is made by the Clone itself before Git
//! runs, recorded as its Server run's before it is made and forgotten once
//! it is known to be gone — not found where the directory holding it can
//! be read, since beneath one that cannot, as on a drive no longer mounted,
//! it may yet come back — and from the moment it is made it has an owner
//! that removes it unless the clone within is named: a name something
//! already holds is passed over for another, and nothing the Clone removes
//! was ever anyone else's. It holds a marker naming its Server run, locked
//! for as long as the Clone lives, so that what a Server that died
//! mid-Clone left behind is told from what a live one is making, and
//! removed at the next start: that, and only that — a directory the Server
//! recorded, still marked as the run that recorded it — never one merely
//! named or marked as a Clone names and marks its own. The clone takes its
//! destination's name only once it is whole, by the platform's own rename
//! that refuses to replace anything — another Clone of the same name among
//! it — so a Root never holds a half-made Workspace, even while one is
//! being made.
//!
//! The Clone's timeout bounds making the clone. Once Git has made it whole,
//! one task sees naming it through, or — where it cannot be named —
//! removing it, and nothing else touches it meanwhile, whatever becomes of
//! whoever awaited the Clone: the Clone answers at once with that landing,
//! for whoever asked for it to wait on as long as they choose. What a Clone
//! that fails leaves is removed before it answers, as long as its deadline
//! allows; what one stopped at its deadline, or abandoned while Git ran,
//! leaves is removed on its own. All a Server run's Clones have under way
//! on their own is counted, so a stopping Server waits on it a while before
//! it lets go.
use std::{
    io::{Read as _, Write as _},
    sync::Arc,
};

use fs2::FileExt as _;

use super::*;
use crate::source_control::{CloneFailure, CloneLanding, CloneStaging, StagingRecord};

/// How many more times removing or naming what a Clone made is tried once it
/// has failed, and how long apart: a Git stopped at its deadline may still be
/// letting go of what it held, and on Windows a file just written may be
/// held a moment longer by whatever reads it, so what Git made can be
/// neither removed nor renamed until then.
const RETRIES: u32 = 30;
const RETRY: Duration = Duration::from_millis(100);

/// How many hidden names a Clone tries before giving up on making a
/// directory of its own to be made in.
const RESERVATIONS: usize = 8;

/// The configuration a Clone's Git is run with whatever the user's says: no
/// `ext::` transport, which runs a command a Repository URL spells out, for
/// a URL a Sidekick was handed.
const NO_COMMAND_TRANSPORT: [&str; 2] = ["-c", "protocol.ext.allow=never"];

/// The transports a Clone's Git may use, whatever the environment it was
/// started in allows: `GIT_ALLOW_PROTOCOL` stands over every
/// `protocol.*.allow`, so it is set, not left to what was inherited.
const ALLOWED_TRANSPORTS: &str = "file:git:http:https:ssh";

/// The marker within a staging directory, naming the Server run whose
/// Clone made it and locked while that Clone lives.
const MARKER: &str = "suru-clone";

/// Where within its staging directory Git makes a clone.
const CLONED: &str = "clone";

/// How a whole clone takes its destination's name: as [`rename_new`] does,
/// unless a host or a test watches it.
pub(super) type Rename = Arc<dyn Fn(&Path, &Path) -> std::io::Result<()> + Send + Sync>;

/// What the Clones made through one adapter share: how a whole clone takes
/// its name, and what they have under way on their own.
#[derive(Clone)]
pub(super) struct Cloning {
    pub(super) rename: Rename,
    landings: Landings,
}

impl Default for Cloning {
    fn default() -> Self {
        Self {
            rename: Arc::new(rename_new),
            landings: Landings::default(),
        }
    }
}

impl Cloning {
    /// Waits until nothing a Clone left to see through on its own is under
    /// way.
    pub(super) async fn settled(&self) {
        self.landings.settled().await;
    }
}

/// Removes what Clones of a Server run no longer alive recorded in
/// `staging`'s records and left behind, on a thread of its own; see
/// [`sweep`].
pub(super) async fn sweep_abandoned(staging: &CloneStaging) {
    let staging = staging.clone();
    let _ = tokio::task::spawn_blocking(move || sweep(&staging)).await;
}

/// What a Server run's Clones have under way on their own — naming a whole
/// clone, removing one — counted, so a stopping Server can wait until none
/// is.
#[derive(Clone)]
struct Landings(Arc<tokio::sync::watch::Sender<usize>>);

impl Default for Landings {
    fn default() -> Self {
        Self(Arc::new(tokio::sync::watch::channel(0).0))
    }
}

impl Landings {
    /// One more under way, until the answer is dropped.
    fn begin(&self) -> Underway {
        self.0.send_modify(|underway| *underway += 1);
        Underway(self.clone())
    }

    async fn settled(&self) {
        let _ = self.0.subscribe().wait_for(|underway| *underway == 0).await;
    }
}

/// One piece of a Clone's work under way on its own.
struct Underway(Landings);

impl Drop for Underway {
    fn drop(&mut self) {
        self.0.0.send_modify(|underway| *underway -= 1);
    }
}

impl GitSourceControl {
    /// Clones `url` into `destination`, recording what it makes in
    /// `staging`'s records, as [`SourceControl::clone_repository`]
    /// describes.
    pub(super) async fn clone_into(
        &self,
        url: &str,
        destination: &Path,
        branch: Option<&str>,
        timeout: Duration,
        staging: &CloneStaging,
    ) -> Result<CloneLanding, CloneFailure> {
        let deadline = tokio::time::Instant::now() + timeout;
        let (Some(parent), Some(name)) = (
            destination.parent(),
            destination.file_name().and_then(|name| name.to_str()),
        ) else {
            return Err(CloneFailure::Failed(format!(
                "`{}` names no directory to Clone into",
                destination.display()
            )));
        };
        let staged = match tokio::time::timeout_at(
            deadline,
            Staged::reserve(destination, name, random_suffix, &self.cloning, staging),
        )
        .await
        {
            Ok(staged) => staged?,
            Err(_) => return Err(CloneFailure::TimedOut),
        };
        let into = format!(
            "{}/{CLONED}",
            staged
                .path()
                .file_name()
                .and_then(|hidden| hidden.to_str())
                .expect("a staging directory is named as it was made")
        );
        let Ok(ssh) = tokio::time::timeout_at(deadline, self.unasking_ssh(parent)).await else {
            return Err(CloneFailure::TimedOut);
        };
        let environment = unasking_environment(ssh.as_deref());
        let branch = branch.map(|branch| format!("--branch={branch}"));
        let mut args = NO_COMMAND_TRANSPORT.to_vec();
        args.extend(["clone", "--quiet"]);
        args.extend(branch.as_deref());
        // Everything after `--` is the URL and the directory, however they
        // begin, so no URL is read as an option.
        args.extend(["--", url, &into]);
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let failure = match self.run(parent, &args, left, None, &environment).await {
            Ok(output) if output.status.success() => {
                // Whole now: named, or removed where it cannot be, by a task
                // of its own that nothing cuts short, whose landing is the
                // answer.
                let underway = self.cloning.landings.begin();
                let rename = self.cloning.rename.clone();
                let destination = destination.to_owned();
                let landing = tokio::spawn(async move {
                    let _underway = underway;
                    staged.land(destination.clone(), rename).await?;
                    Ok(cloned_workspace(&destination))
                });
                return Ok(CloneLanding::new(async move {
                    landing.await.unwrap_or_else(|error| {
                        Err(CloneFailure::Failed(format!(
                            "Giving the clone its name failed: {error}"
                        )))
                    })
                }));
            }
            Ok(output) => CloneFailure::Failed(said(&output)),
            // What Git began goes with its staging, removed on its own, so
            // the refusal comes at the deadline.
            Err(Unrun::TimedOut) => return Err(CloneFailure::TimedOut),
            Err(Unrun::Failed(reason)) => CloneFailure::Failed(reason),
        };
        let staging = staged.path().to_owned();
        match tokio::time::timeout_at(deadline, staged.remove()).await {
            Ok(Ok(())) | Err(_) => Err(failure),
            Ok(Err(error)) => Err(left_behind(failure, &staging, &error)),
        }
    }

    /// The ssh command a Clone has Git run, where it must name one for Git
    /// to ask nothing; see [`unasking_ssh`]. Reads what the user names: in
    /// the environment, which a Clone keeps, and in Git's configuration.
    async fn unasking_ssh(&self, directory: &Path) -> Option<String> {
        let named = |name: &str| {
            let inherited = std::env::var(name).ok();
            let given = self
                .environment
                .iter()
                .rev()
                .find(|(given, _)| given == name)
                .and_then(|(_, value)| value.to_str().map(str::to_owned));
            given.or(inherited).filter(|value| !value.trim().is_empty())
        };
        let configured = self
            .text(
                directory,
                &[
                    "config",
                    "--get-regexp",
                    r"^(core\.sshcommand|ssh\.variant)$",
                ],
            )
            .await
            .unwrap_or_default();
        let configured = |key: &str| {
            configured
                .lines()
                .filter_map(|line| line.split_once(' '))
                .filter(|(named, _)| *named == key)
                .map(|(_, value)| value.to_owned())
                .next_back()
        };
        unasking_ssh(&SshNamed {
            command: named("GIT_SSH_COMMAND").or_else(|| configured("core.sshcommand")),
            program: named("GIT_SSH"),
            variant: named("GIT_SSH_VARIANT").or_else(|| configured("ssh.variant")),
        })
    }
}

/// The Workspace a Git clone at `destination` is, as the Clone knows it
/// without reading it: a Repository whose main root is `destination` and
/// whose metadata is the `.git` within it, read as discovery reads it.
fn cloned_workspace(destination: &Path) -> Workspace {
    let metadata = destination.join(".git");
    repository_workspace(&Repository {
        id: RepositoryId::from_metadata("git", &metadata),
        system: "git".to_owned(),
        metadata_directory: metadata,
        location: RepositoryLocation::Main {
            root: destination.to_owned(),
        },
        availability: SourceControlAvailability::Available,
        capabilities: SourceControlCapabilities::discovery_only(),
    })
}

/// What a Clone that failed as `failure` says once removing what it left at
/// `staging` failed as `error`.
fn left_behind(failure: CloneFailure, staging: &Path, error: &std::io::Error) -> CloneFailure {
    tracing::warn!(
        staging = %staging.display(),
        "what a Clone left behind could not be removed: {error}"
    );
    let why = match failure {
        CloneFailure::Failed(said) => said,
        CloneFailure::Occupied => "Something came to stand at its destination".to_owned(),
        _ => "It did not finish in time".to_owned(),
    };
    CloneFailure::LeftBehind(format!(
        "{why}; removing what it left at `{}` failed: {error}",
        staging.display()
    ))
}

/// Eight hex digits, enough to keep two Clones' hidden names apart.
fn random_suffix() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_owned()
}

/// The hidden name a Clone of `name` is made under, told apart by `suffix`.
fn staging_name(name: &str, suffix: &str) -> String {
    format!(".{name}.suru-clone-{suffix}")
}

/// Whether `name` is one a Clone names its staging by: `.`, a name, and
/// `.suru-clone-` with eight lowercase hex digits.
fn is_staging_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('.') else {
        return false;
    };
    let Some((named, suffix)) = rest.rsplit_once(".suru-clone-") else {
        return false;
    };
    !named.is_empty()
        && suffix.len() == 8
        && suffix
            .chars()
            .all(|digit| digit.is_ascii_digit() || ('a'..='f').contains(&digit))
}

/// The hidden directory a Clone is made in, which this Clone made and so
/// alone may remove. It stays the Clone's to remove until the clone within
/// is named or it is gone: dropped before then — as where whoever awaited
/// it goes away, Git with it, or its deadline passes — it is removed on its
/// own, counted among what Clones have under way, or at once where no
/// runtime is there to run that. Once it is known to be gone its record is
/// forgotten; one that cannot be removed, or told gone, stays recorded, for
/// the next start to remove.
struct Staged {
    staging: Option<PathBuf>,
    /// The marker within it, held open and locked while the Clone lives.
    marker: Option<std::fs::File>,
    landings: Landings,
    /// Where it is recorded, until it is gone.
    records: CloneStaging,
}

impl Staged {
    /// Makes a hidden directory beside `destination` for a Clone of `name`
    /// to be made in, named by the first of `suffix`'s answers that nothing
    /// holds yet, unless something already stands at `destination`, and
    /// records and marks it as `staging`'s run's. Made on a thread of its
    /// own, so a Root that does not answer holds up no one, and owned from
    /// the moment it is made: what is handed over is the guard itself, so
    /// one made after whoever asked for it went away is removed as its
    /// guard is dropped.
    async fn reserve(
        destination: &Path,
        name: &str,
        mut suffix: impl FnMut() -> String + Send + 'static,
        cloning: &Cloning,
        staging: &CloneStaging,
    ) -> Result<Self, CloneFailure> {
        let (reserved, reservation) = tokio::sync::oneshot::channel();
        let (destination, name) = (destination.to_owned(), name.to_owned());
        let (landings, records) = (cloning.landings.clone(), staging.clone());
        tokio::task::spawn_blocking(move || {
            let made =
                reserve_now(&destination, &name, &mut suffix, &records).map(|(staging, marker)| {
                    Self {
                        staging: Some(staging),
                        marker: Some(marker),
                        landings,
                        records,
                    }
                });
            // A guard nobody receives is dropped with the send's answer.
            let _ = reserved.send(made);
        });
        reservation.await.unwrap_or_else(|_| {
            Err(CloneFailure::Failed(
                "Making a directory to Clone into failed".to_owned(),
            ))
        })
    }

    fn path(&self) -> &Path {
        self.staging
            .as_deref()
            .expect("a staged Clone is held until named or removed")
    }

    /// Gives the whole clone within its name, `destination`, by `rename`,
    /// never over whatever has come to stand there since — or, where it
    /// cannot, removes it, only once the rename has answered — and then
    /// lets the staging go. Run on a task of its own, so nothing cuts
    /// either short.
    async fn land(self, destination: PathBuf, rename: Rename) -> Result<(), CloneFailure> {
        let (from, to) = (self.path().join(CLONED), destination.clone());
        let named = persistently(
            move || rename(&from, &to),
            |error| error.kind() == std::io::ErrorKind::AlreadyExists,
        )
        .await;
        let staging = self.path().to_owned();
        let failure = match named {
            Ok(()) => {
                if let Err(error) = self.remove().await {
                    tracing::warn!(
                        staging = %staging.display(),
                        "a Clone landed, but its staging could not be removed: {error}"
                    );
                }
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                CloneFailure::Occupied
            }
            Err(error) => CloneFailure::Failed(format!(
                "Could not give the clone its name, `{}`: {error}",
                destination.display()
            )),
        };
        match self.remove().await {
            Ok(()) => Err(failure),
            Err(error) => Err(left_behind(failure, &staging, &error)),
        }
    }

    /// Removes what is staged, and forgets its record once it is gone,
    /// holding it as the Clone's until removing it has finished, so a
    /// removal abandoned midway is finished on its own.
    async fn remove(mut self) -> std::io::Result<()> {
        self.marker = None;
        let removed = dismantle(self.path(), &self.records).await;
        self.staging = None;
        removed
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        self.marker = None;
        let Some(staging) = self.staging.take() else {
            return;
        };
        let records = self.records.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                let underway = self.landings.begin();
                runtime.spawn(async move {
                    let _underway = underway;
                    if let Err(error) = dismantle(&staging, &records).await {
                        tracing::warn!(
                            staging = %staging.display(),
                            "what an abandoned Clone left behind could not be removed: {error}"
                        );
                    }
                });
            }
            Err(_) => match remove_if_there(&staging) {
                Ok(()) => forget(&records, &staging),
                Err(error) => tracing::warn!(
                    staging = %staging.display(),
                    "what an abandoned Clone left behind could not be removed: {error}"
                ),
            },
        }
    }
}

/// Makes the directory a Clone of `name` to `destination` is made in, as
/// [`Staged::reserve`] describes — recorded in `staging`'s records before
/// it is made, so a Server that dies at any moment after leaves nothing its
/// next start does not know of — and its marker naming `staging`'s run,
/// locked; answers where it is and the marker. A name passed over leaves no
/// record.
fn reserve_now(
    destination: &Path,
    name: &str,
    suffix: &mut impl FnMut() -> String,
    staging: &CloneStaging,
) -> Result<(PathBuf, std::fs::File), CloneFailure> {
    if std::fs::symlink_metadata(destination).is_ok() {
        return Err(CloneFailure::Occupied);
    }
    let Some(parent) = destination.parent() else {
        return Err(CloneFailure::Failed(format!(
            "`{}` names no directory to Clone into",
            destination.display()
        )));
    };
    let unmade = |error: &dyn std::fmt::Display| {
        CloneFailure::Failed(format!(
            "Could not make a directory to Clone into beside `{}`: {error}",
            destination.display()
        ))
    };
    for _ in 0..RESERVATIONS {
        let directory = parent.join(staging_name(name, &suffix()));
        if std::fs::symlink_metadata(&directory).is_ok() {
            continue;
        }
        let record = StagingRecord {
            directory: directory.clone(),
            run: staging.run.clone(),
        };
        staging
            .records
            .record(&record)
            .map_err(|error| unmade(&format!("recording it failed: {error}")))?;
        match std::fs::create_dir(&directory) {
            Ok(()) => {
                return match mark(&directory, &staging.run) {
                    Ok(marker) => Ok((directory, marker)),
                    Err(error) => {
                        if remove_if_there(&directory).is_ok() {
                            forget(staging, &directory);
                        }
                        Err(unmade(&error))
                    }
                };
            }
            Err(error) => {
                // Nothing there is this Clone's.
                forget(staging, &directory);
                if error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(unmade(&error));
                }
            }
        }
    }
    Err(unmade(&"every name tried was taken"))
}

/// Marks `staging` as a Clone of the Server run `run`'s: a marker naming
/// the run, locked — waiting out a sweep looking at it — before the run is
/// written, so a marker naming a run is always one its Clone held.
fn mark(staging: &Path, run: &str) -> std::io::Result<std::fs::File> {
    let mut marker = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(staging.join(MARKER))?;
    marker.lock_exclusive()?;
    marker.write_all(run.as_bytes())?;
    marker.flush()?;
    Ok(marker)
}

/// Forgets `staging` in `records`, once nothing of a Clone is left there.
/// Blocks.
fn forget(records: &CloneStaging, staging: &Path) {
    if let Err(error) = records.records.forget(staging) {
        tracing::warn!(
            staging = %staging.display(),
            "what a Clone made is gone, but its record could not be forgotten: {error}"
        );
    }
}

/// Removes every directory a Clone of a Server run no longer alive recorded
/// in `staging`'s records and left behind, and nothing else, forgetting
/// each record once nothing of a Clone stands at it: a directory not
/// recorded is never touched, however it is named or marked, and one
/// recorded is removed only where [`left`] finds it abandoned. This very
/// run's records are its own Clones', under way, and left alone.
fn sweep(staging: &CloneStaging) {
    let recorded = match staging.records.recorded() {
        Ok(recorded) => recorded,
        Err(error) => {
            tracing::warn!(
                "what Clones of a Server no longer running made could not be read: {error}"
            );
            return;
        }
    };
    for record in recorded {
        if record.run == staging.run {
            continue;
        }
        let directory = &record.directory;
        match left(&record) {
            Left::Abandoned => match remove_tree(directory) {
                Ok(()) => {
                    tracing::info!(
                        staging = %directory.display(),
                        "removed what a Clone of a Server no longer running left behind"
                    );
                    forget(staging, directory);
                }
                Err(error) => tracing::warn!(
                    staging = %directory.display(),
                    "what a Clone of a Server no longer running left behind could not be \
                     removed: {error}"
                ),
            },
            Left::Nothing => forget(staging, directory),
            Left::Unknown => {}
        }
    }
}

/// What a sweep finds of a directory a Clone recorded.
#[derive(Debug, Eq, PartialEq)]
enum Left {
    /// The Clone's staging, still marked as the run that recorded it, which
    /// no live Clone holds: to be removed.
    Abandoned,
    /// Nothing of the Clone's: the directory is gone, or what stands there
    /// is no staging of that run's — not named as a Clone names its
    /// staging, no directory, or not marked as that run's, which is left as
    /// it is. The record is done with either way.
    Nothing,
    /// What cannot be told yet: the directory holding it cannot be read, as
    /// on a drive not mounted, or a live Clone holds its marker. The record
    /// stands, for the next start.
    Unknown,
}

/// What the Clone recorded as `record` left behind, as [`Left`] tells it:
/// abandoned only where the directory recorded is named as a Clone names its
/// staging and holds a marker no live Clone holds locked, naming the run
/// that recorded it.
fn left(record: &StagingRecord) -> Left {
    let staging = &record.directory;
    if !staging
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(is_staging_name)
    {
        return Left::Nothing;
    }
    match std::fs::symlink_metadata(staging) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Left::Nothing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return match holder_read(staging) {
                Ok(()) => Left::Nothing,
                Err(_) => Left::Unknown,
            };
        }
        Err(_) => return Left::Unknown,
    }
    let marker = staging.join(MARKER);
    match std::fs::symlink_metadata(&marker) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Left::Nothing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Left::Nothing,
        Err(_) => return Left::Unknown,
    }
    let Ok(mut marker) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&marker)
    else {
        return Left::Unknown;
    };
    if marker.try_lock_exclusive().is_err() {
        return Left::Unknown;
    }
    let mut named = String::new();
    let read = marker.read_to_string(&mut named);
    let _ = fs2::FileExt::unlock(&marker);
    match read {
        Ok(_) if named.trim() == record.run => Left::Abandoned,
        Ok(_) => Left::Nothing,
        Err(_) => Left::Unknown,
    }
}

/// Renames the directory `from` to `to` where nothing stands at `to`, and
/// otherwise fails as `AlreadyExists`, replacing nothing: by the platform's
/// own rename that refuses to replace anything, in one step — Linux's
/// `renameat2` with `RENAME_NOREPLACE`, macOS's `renamex_np` with
/// `RENAME_EXCL`, Windows' `MoveFileExW` asked to replace nothing. Only
/// where neither the platform nor the filesystem offers such a rename does
/// it fall back on [`rename_reserving`].
pub(super) fn rename_new(from: &Path, to: &Path) -> std::io::Result<()> {
    let renamed = match rename_exclusive(from, to) {
        Some(renamed) => renamed,
        #[cfg(unix)]
        None => rename_reserving(from, to),
        #[cfg(not(unix))]
        None => unreachable!("Windows always renames exclusively"),
    };
    renamed.map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists
            || std::fs::symlink_metadata(to).is_ok()
        {
            std::io::ErrorKind::AlreadyExists.into()
        } else {
            error
        }
    })
}

/// `from` and `to` as the C strings a rename takes; `None` where a path
/// holds a NUL, which no rename can name.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn c_paths(from: &Path, to: &Path) -> Option<(std::ffi::CString, std::ffi::CString)> {
    use std::os::unix::ffi::OsStrExt;

    Some((
        std::ffi::CString::new(from.as_os_str().as_bytes()).ok()?,
        std::ffi::CString::new(to.as_os_str().as_bytes()).ok()?,
    ))
}

/// Renames `from` to `to` by `renameat2` with `RENAME_NOREPLACE`, made as a
/// system call so no C library of a particular age is needed; `None` where
/// the kernel or the filesystem does not offer it.
#[cfg(target_os = "linux")]
fn rename_exclusive(from: &Path, to: &Path) -> Option<std::io::Result<()>> {
    let (from, to) = c_paths(from, to)?;
    // SAFETY: both paths are NUL-terminated and outlive the call, which only
    // reads them.
    let renamed = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if renamed == 0 {
        return Some(Ok(()));
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ENOSYS | libc::EINVAL) => None,
        _ => Some(Err(error)),
    }
}

/// Renames `from` to `to` by `renamex_np` with `RENAME_EXCL`; `None` where
/// the filesystem does not offer it.
#[cfg(target_os = "macos")]
fn rename_exclusive(from: &Path, to: &Path) -> Option<std::io::Result<()>> {
    let (from, to) = c_paths(from, to)?;
    // SAFETY: both paths are NUL-terminated and outlive the call, which only
    // reads them.
    let renamed = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    if renamed == 0 {
        return Some(Ok(()));
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ENOTSUP | libc::EINVAL) => None,
        _ => Some(Err(error)),
    }
}

/// No rename refusing to replace is known on this platform, so every one
/// falls back on [`rename_reserving`].
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn rename_exclusive(_from: &Path, _to: &Path) -> Option<std::io::Result<()>> {
    None
}

/// Renames `from` to `to` by `MoveFileExW` asked to replace nothing, which
/// refuses an existing `to`.
#[cfg(windows)]
fn rename_exclusive(from: &Path, to: &Path) -> Option<std::io::Result<()>> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;

    let wide = |path: &Path| {
        path.as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>()
    };
    let (wide_from, wide_to) = (wide(from), wide(to));
    // SAFETY: both paths are NUL-terminated and outlive the call.
    let moved = unsafe { MoveFileExW(wide_from.as_ptr(), wide_to.as_ptr(), 0) };
    Some(if moved != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    })
}

/// Which directory `path` is, by the device and inode it lies at, without
/// following a link.
#[cfg(unix)]
fn identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;

    std::fs::symlink_metadata(path)
        .ok()
        .filter(std::fs::Metadata::is_dir)
        .map(|metadata| (metadata.dev(), metadata.ino()))
}

/// The fallback where no rename refusing to replace is to be had, on Unix
/// alone, since Windows always has one. POSIX `rename` replaces an empty
/// directory, so `to` is first made as one — which fails where anything
/// stands there — and it is that directory, this call's own, the rename
/// replaces: checked to be the very one made, by its device and inode,
/// immediately before renaming, and `to` checked to be `from` straight
/// after; either check failing refuses as something standing there. One
/// something was put in meanwhile is left as it is, and so is one put in
/// its place before the check. All that can be lost is in the moment
/// between the check and the rename: an empty directory another made at
/// `to` in that moment, having removed this one, which the rename
/// replaces. Only the exclusive renames rule that out.
#[cfg(unix)]
fn rename_reserving(from: &Path, to: &Path) -> std::io::Result<()> {
    rename_reserving_with(from, to, || {})
}

/// [`rename_reserving`], with `meanwhile` run between making `to` and
/// checking it is the directory made, as another process might act.
#[cfg(unix)]
fn rename_reserving_with(from: &Path, to: &Path, meanwhile: impl FnOnce()) -> std::io::Result<()> {
    let standing = || std::io::Error::from(std::io::ErrorKind::AlreadyExists);
    let moving =
        identity(from).ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
    std::fs::create_dir(to)?;
    let reserved = identity(to);
    meanwhile();
    if reserved.is_none() || identity(to) != reserved {
        return Err(standing());
    }
    if let Err(error) = std::fs::rename(from, to) {
        if identity(to) != reserved || std::fs::remove_dir(to).is_err() {
            return Err(standing());
        }
        return Err(error);
    }
    if identity(to) != Some(moving) {
        return Err(standing());
    }
    Ok(())
}

/// What the user names of the ssh Git runs, each where they name it at all:
/// the command line it runs (`GIT_SSH_COMMAND`, else `core.sshCommand`),
/// the program it runs otherwise (`GIT_SSH`), and what kind of ssh it is
/// (`GIT_SSH_VARIANT`, else `ssh.variant`).
#[derive(Debug, Default)]
struct SshNamed {
    command: Option<String>,
    program: Option<String>,
    variant: Option<String>,
}

/// The kinds of ssh Git tells apart, as it reads them, by what each takes to
/// ask nothing.
#[derive(Debug, Eq, PartialEq)]
enum SshVariant {
    /// OpenSSH, or a program that takes what it does, which takes
    /// `-o BatchMode=yes`.
    OpenSsh,
    /// PuTTY's plink or TortoisePlink, which take `-batch`.
    Plink,
    /// One the user says takes nothing but where to connect and what to
    /// run: no option reaches it.
    Simple,
}

impl SshVariant {
    /// The variant `named` names, as `GIT_SSH_VARIANT` or `ssh.variant`
    /// does; `None` for `auto`, which leaves it to the program's name.
    fn named(named: &str) -> Option<Self> {
        match named.trim().to_ascii_lowercase().as_str() {
            "auto" => None,
            "plink" | "putty" | "tortoiseplink" => Some(Self::Plink),
            "simple" => Some(Self::Simple),
            _ => Some(Self::OpenSsh),
        }
    }

    /// The variant of the program at `program`, read by its name, whatever
    /// directory it is in, as Git reads it: plink and TortoisePlink by
    /// theirs, and every other as OpenSSH. Git probes a program it does not
    /// know by name with `-G`, passing it OpenSSH's options where it answers
    /// as OpenSSH does; this takes such a program to be one, as an ssh
    /// wrapper is, so it is told to ask nothing too. One that is not says so
    /// by `ssh.variant=simple`, which Git honours as well.
    fn of_program(program: &str) -> Self {
        let name = program
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(program)
            .to_ascii_lowercase();
        match name.strip_suffix(".exe").unwrap_or(&name) {
            "plink" | "tortoiseplink" => Self::Plink,
            _ => Self::OpenSsh,
        }
    }

    /// What this variant is told so it asks nothing, where anything is.
    fn unasking(&self) -> Option<&'static str> {
        match self {
            Self::OpenSsh => Some("-o BatchMode=yes"),
            Self::Plink => Some("-batch"),
            Self::Simple => None,
        }
    }
}

/// The `GIT_SSH_COMMAND` a Clone has Git run so that its ssh asks nothing —
/// no passphrase, no password, no new host to trust — and a key or host it
/// would have to ask about fails the Clone at once: the very ssh Git would
/// run, as `named` names it or `ssh` where it names none, told what its
/// variant takes to ask nothing — OpenSSH `-o BatchMode=yes`, plink and
/// TortoisePlink `-batch` — first, straight after the program, ahead of
/// every option the user's command gives itself: OpenSSH keeps the first
/// value it is given for an option, so this one stands over a
/// `-o BatchMode=no` of theirs. `None` only where the user says their ssh
/// takes no option at all, which Git then runs as they name it.
fn unasking_ssh(named: &SshNamed) -> Option<String> {
    let overridden = named.variant.as_deref().and_then(SshVariant::named);
    let (command, program, program_ends) = match (&named.command, &named.program) {
        (Some(command), _) => {
            let (program, ends) = program_of(command);
            (command.clone(), program, ends)
        }
        (None, Some(program)) => {
            let quoted = sh_quoted(program);
            let ends = quoted.len();
            (quoted, program.clone(), ends)
        }
        (None, None) => ("ssh".to_owned(), "ssh".to_owned(), "ssh".len()),
    };
    let variant = overridden.unwrap_or_else(|| SshVariant::of_program(&program));
    let unasking = variant.unasking()?;
    Some(format!(
        "{} {unasking}{}",
        &command[..program_ends],
        &command[program_ends..]
    ))
}

/// The program the shell command line `command` runs — its first word that
/// assigns no variable, its quotes taken away as Git reads the program a
/// command line runs — and where in `command` that word ends.
fn program_of(command: &str) -> (String, usize) {
    let mut from = 0;
    loop {
        let (word, ends) = word_at(command, from);
        let assigns = word.split_once('=').is_some_and(|(name, _)| {
            name.starts_with(|first: char| first == '_' || first.is_ascii_alphabetic())
                && name
                    .chars()
                    .all(|character| character == '_' || character.is_ascii_alphanumeric())
        });
        if !assigns || ends >= command.len() {
            return (word, ends);
        }
        from = ends;
    }
}

/// The word of the shell command line `command` beginning at the first
/// character past `from` that is not space, its quotes taken away, and
/// where in `command` it ends.
fn word_at(command: &str, from: usize) -> (String, usize) {
    let start = command[from..]
        .find(|character: char| !character.is_whitespace())
        .map_or(command.len(), |start| from + start);
    let mut word = String::new();
    let mut quote = None;
    let mut characters = command[start..].char_indices();
    while let Some((at, character)) = characters.next() {
        match (quote, character) {
            (None, character) if character.is_whitespace() => return (word, start + at),
            (None, '\'' | '"') => quote = Some(character),
            (Some(open), character) if character == open => quote = None,
            (None | Some('"'), '\\') => word.extend(characters.next().map(|(_, next)| next)),
            (_, character) => word.push(character),
        }
    }
    (word, command.len())
}

/// `text` quoted for the shell Git runs `GIT_SSH_COMMAND` through.
fn sh_quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// What a Clone's Git is run with, beside what every Git command is, so that
/// nothing it runs asks anyone anything: a credential it cannot find fails it
/// at once rather than leaving it waiting on a prompt nobody sees, and so
/// does a key or host its ssh would have to ask about, where `ssh` is the
/// command that ssh is run as (see [`unasking_ssh`]).
fn unasking_environment(ssh: Option<&str>) -> Vec<(&'static str, &str)> {
    let mut environment = vec![
        // An empty askpass is none, whatever `core.askPass` or `SSH_ASKPASS`
        // names, so Git falls back on the terminal prompt that
        // `GIT_TERMINAL_PROMPT=0` already refuses.
        ("GIT_ASKPASS", ""),
        // Nor does ssh ask through a program of its own,
        ("SSH_ASKPASS_REQUIRE", "never"),
        // nor Git Credential Manager through a window of its own.
        ("GCM_INTERACTIVE", "never"),
        // No transport outside these, whatever was inherited.
        ("GIT_ALLOW_PROTOCOL", ALLOWED_TRANSPORTS),
    ];
    environment.extend(ssh.map(|ssh| ("GIT_SSH_COMMAND", ssh)));
    environment
}

/// What Git said of a command that failed, in its own words.
fn said(output: &Output) -> String {
    let words = |bytes: &[u8]| String::from_utf8_lossy(bytes).trim().to_owned();
    match (words(&output.stderr), words(&output.stdout)) {
        (stderr, _) if !stderr.is_empty() => stderr,
        (_, stdout) if !stdout.is_empty() => stdout,
        _ => format!("Git exited with {}", output.status),
    }
}

/// Removes what a Clone that did not finish left at `staging`; nothing
/// there is nothing to remove, where that can be told at all — where it
/// cannot, as [`Untold`] says, it is not tried again.
async fn remove_partial(staging: &Path) -> std::io::Result<()> {
    let staging = staging.to_owned();
    persistently(
        move || remove_if_there(&staging),
        |error| error.get_ref().is_some_and(|said| said.is::<Untold>()),
    )
    .await
}

/// Removes the staging directory `staging` as [`remove_partial`] does, and
/// once it is known to be gone forgets it in `records`; one that cannot be
/// removed, or told gone, stays recorded, for the next start to remove.
async fn dismantle(staging: &Path, records: &CloneStaging) -> std::io::Result<()> {
    remove_partial(staging).await?;
    let (staging, records) = (staging.to_owned(), records.clone());
    let _ = tokio::task::spawn_blocking(move || forget(&records, &staging)).await;
    Ok(())
}

/// Removes the directory tree at `path`, where anything stands there. One
/// not found is gone only where the directory holding it can be read:
/// beneath one that cannot — on a drive no longer mounted, or moved away —
/// it is not found either, yet may come back with it, so removing it fails
/// as [`Untold`].
fn remove_if_there(path: &Path) -> std::io::Result<()> {
    match remove_tree(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => match holder_read(path) {
            Err(unread) => Err(std::io::Error::new(unread.kind(), Untold(unread))),
            Ok(()) => match std::fs::symlink_metadata(path) {
                Err(gone) if gone.kind() == std::io::ErrorKind::NotFound => Ok(()),
                // Still there: what went missing was within it.
                _ => Err(error),
            },
        },
        removed => removed,
    }
}

/// Why a staging directory not found is not taken for gone: the directory
/// holding it cannot be read, as this error says, so it may yet come back.
#[derive(Debug)]
struct Untold(std::io::Error);

impl std::fmt::Display for Untold {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "the directory holding it cannot be read, so it stays recorded for the Server's \
             next start to remove: {}",
            self.0
        )
    }
}

impl std::error::Error for Untold {}

/// Reads the directory holding `path`, so that `path` not found there is
/// known to be gone.
fn holder_read(path: &Path) -> std::io::Result<()> {
    let holder = path
        .parent()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
    std::fs::read_dir(holder).map(drop)
}

/// Runs `act` on a thread of its own until it succeeds, trying it again a
/// few times, a moment apart, where it fails — but not where it fails as
/// `settled` says no try can change.
async fn persistently(
    act: impl Fn() -> std::io::Result<()> + Clone + Send + 'static,
    settled: impl Fn(&std::io::Error) -> bool,
) -> std::io::Result<()> {
    let mut retries = 0;
    loop {
        let attempt = act.clone();
        let acted = tokio::task::spawn_blocking(attempt)
            .await
            .unwrap_or_else(|error| Err(std::io::Error::other(error)));
        match acted {
            Ok(()) => return Ok(()),
            Err(error) if retries == RETRIES || settled(&error) => return Err(error),
            Err(_) => {
                retries += 1;
                tokio::time::sleep(RETRY).await;
            }
        }
    }
}

/// Removes the directory tree at `path`. Git marks the objects it writes
/// read-only, which Windows refuses to delete, so there each is made
/// writable first.
fn remove_tree(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    make_writable(path)?;
    std::fs::remove_dir_all(path)
}

/// Clears the read-only attribute of `path` and of everything within it,
/// never following a link out of it.
#[cfg(windows)]
fn make_writable(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path)? {
            make_writable(&entry?.path())?;
        }
    }
    let mut permissions = metadata.permissions();
    if permissions.readonly() {
        // On Windows read-only is an attribute and nothing more: clearing it
        // grants no one anything.
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        std::fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex as StdMutex, mpsc};

    use super::*;

    /// How long a test waits for what it knows will happen.
    const SURELY: Duration = Duration::from_secs(30);

    /// Waits until nothing stands at `path`, as a removal on a task of its
    /// own leaves it.
    async fn gone(path: &Path) {
        tokio::time::timeout(SURELY, async {
            while std::fs::symlink_metadata(path).is_ok() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{} is removed", path.display()));
    }

    /// Answers each of `suffixes` in turn.
    fn suffixes(suffixes: &[&str]) -> impl FnMut() -> String + Send + 'static {
        let mut suffixes = suffixes
            .iter()
            .map(|suffix| (*suffix).to_owned())
            .collect::<Vec<_>>()
            .into_iter();
        move || {
            suffixes
                .next()
                .expect("a suffix is asked for no more often than given")
        }
    }

    /// Runs Git in `directory` as a test's fixture does.
    fn git(directory: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(directory)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Suru Test")
            .env("GIT_AUTHOR_EMAIL", "suru@example.invalid")
            .env("GIT_COMMITTER_NAME", "Suru Test")
            .env("GIT_COMMITTER_EMAIL", "suru@example.invalid")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A bare Repository within `within` holding one commit with a README;
    /// answers its path.
    fn origin(within: &Path) -> PathBuf {
        let work = within.join("atlas-work");
        std::fs::create_dir(&work).unwrap();
        git(&work, &["init", "-b", "main"]);
        std::fs::write(work.join("README.md"), "# atlas\n").unwrap();
        git(&work, &["add", "README.md"]);
        git(
            &work,
            &["-c", "commit.gpgsign=false", "commit", "-m", "initial"],
        );
        let bare = within.join("atlas.git");
        git(
            within,
            &[
                "clone",
                "--bare",
                "--quiet",
                work.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        bare
    }

    /// A rename that says when it has begun and waits for the test to let
    /// it go on, then renames as `then` does.
    struct Held {
        begun: mpsc::Receiver<()>,
        release: mpsc::Sender<()>,
    }

    fn held(then: fn(&Path, &Path) -> std::io::Result<()>) -> (Held, Rename) {
        let (begun_tx, begun) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let (begun_tx, released) = (StdMutex::new(begun_tx), StdMutex::new(released));
        let rename: Rename = Arc::new(move |from: &Path, to: &Path| {
            let _ = begun_tx.lock().unwrap().send(());
            let _ = released.lock().unwrap().recv();
            then(from, to)
        });
        (Held { begun, release }, rename)
    }

    impl Held {
        /// Waits until the rename has begun.
        async fn begun(&self) {
            tokio::time::timeout(SURELY, async {
                while self.begun.try_recv().is_err() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("the clone comes to take its name");
        }
    }

    /// Git, its clones named by `rename`.
    fn renaming(rename: Rename) -> GitSourceControl {
        GitSourceControl {
            cloning: Cloning {
                rename,
                ..Cloning::default()
            },
            ..GitSourceControl::default()
        }
    }

    /// A run named `run`, recording in memory alone.
    fn run_named(run: &str) -> CloneStaging {
        CloneStaging {
            run: run.to_owned(),
            ..CloneStaging::in_memory()
        }
    }

    /// The directories `staging`'s records hold, sorted.
    fn recorded(staging: &CloneStaging) -> Vec<PathBuf> {
        let mut recorded = staging
            .records
            .recorded()
            .unwrap()
            .into_iter()
            .map(|record| record.directory)
            .collect::<Vec<_>>();
        recorded.sort();
        recorded
    }

    /// Waits until `staging`'s records hold nothing, as a removal on a task
    /// of its own leaves them once it is done.
    async fn all_forgotten(staging: &CloneStaging) {
        tokio::time::timeout(SURELY, async {
            while !recorded(staging).is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("everything recorded is forgotten: {:?}", recorded(staging)));
    }

    /// The hidden directories directly within `directory`.
    fn staging_within(directory: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.contains(".suru-clone-"))
            })
            .collect()
    }

    #[test]
    fn a_clone_asks_no_one_anything() {
        let environment = unasking_environment(Some("ssh -o BatchMode=yes"));
        assert!(environment.contains(&("GIT_ASKPASS", "")));
        assert!(environment.contains(&("SSH_ASKPASS_REQUIRE", "never")));
        assert!(environment.contains(&("GCM_INTERACTIVE", "never")));
        assert!(environment.contains(&("GIT_ALLOW_PROTOCOL", "file:git:http:https:ssh")));
        assert!(environment.contains(&("GIT_SSH_COMMAND", "ssh -o BatchMode=yes")));
        assert!(
            unasking_environment(None)
                .iter()
                .all(|(name, _)| *name != "GIT_SSH_COMMAND"),
            "an ssh nothing can be told to is left as the user names it"
        );
        assert_eq!(NO_COMMAND_TRANSPORT, ["-c", "protocol.ext.allow=never"]);
    }

    /// Whatever ssh the user names is the one Git runs, told what its kind
    /// takes to ask nothing, straight after the program and ahead of every
    /// option of theirs, so it stands over one of theirs saying otherwise; a
    /// program of a name Git does not know is taken for OpenSSH, and only
    /// one the user says takes no option is left as they name it.
    #[test]
    fn the_ssh_a_clone_runs_is_the_users_own_told_first_to_ask_nothing() {
        let named = |command: Option<&str>, program: Option<&str>, variant: Option<&str>| {
            unasking_ssh(&SshNamed {
                command: command.map(str::to_owned),
                program: program.map(str::to_owned),
                variant: variant.map(str::to_owned),
            })
        };
        for (command, program, variant, run) in [
            (None, None, None, Some("ssh -o BatchMode=yes")),
            (
                Some("ssh -i ~/.ssh/locked_key"),
                None,
                None,
                Some("ssh -o BatchMode=yes -i ~/.ssh/locked_key"),
            ),
            (
                Some("ssh -o BatchMode=no -i locked_key"),
                None,
                None,
                Some("ssh -o BatchMode=yes -o BatchMode=no -i locked_key"),
            ),
            (Some("  ssh"), None, None, Some("  ssh -o BatchMode=yes")),
            (
                Some("'/opt/my ssh/bin/ssh' -F config"),
                Some("/usr/bin/plink"),
                None,
                Some("'/opt/my ssh/bin/ssh' -o BatchMode=yes -F config"),
            ),
            (
                Some(r#""C:\\Program Files\\OpenSSH\\ssh.exe" -i key"#),
                None,
                None,
                Some(r#""C:\\Program Files\\OpenSSH\\ssh.exe" -o BatchMode=yes -i key"#),
            ),
            (
                Some("GIT_TRACE=1 LANG='en US' ssh -i key"),
                None,
                None,
                Some("GIT_TRACE=1 LANG='en US' ssh -o BatchMode=yes -i key"),
            ),
            (
                Some("plink.exe -load work"),
                None,
                None,
                Some("plink.exe -batch -load work"),
            ),
            (
                None,
                Some(r"C:\Program Files\PuTTY\plink.exe"),
                None,
                Some(r"'C:\Program Files\PuTTY\plink.exe' -batch"),
            ),
            (
                None,
                Some("/opt/it's/ssh"),
                None,
                Some(r"'/opt/it'\''s/ssh' -o BatchMode=yes"),
            ),
            (
                None,
                Some("TortoisePlink.exe"),
                None,
                Some("'TortoisePlink.exe' -batch"),
            ),
            (
                Some("my-ssh-wrapper --vpn"),
                None,
                None,
                Some("my-ssh-wrapper -o BatchMode=yes --vpn"),
            ),
            (
                None,
                Some("/usr/local/bin/vpn-wrapper"),
                Some("auto"),
                Some("'/usr/local/bin/vpn-wrapper' -o BatchMode=yes"),
            ),
            (Some("my-ssh-wrapper --vpn"), None, Some("simple"), None),
            (
                Some("ssh -i key"),
                None,
                Some("ssh"),
                Some("ssh -o BatchMode=yes -i key"),
            ),
            (Some("ssh"), None, Some("putty"), Some("ssh -batch")),
        ] {
            assert_eq!(
                named(command, program, variant).as_deref(),
                run,
                "{command:?}, {program:?}, {variant:?}"
            );
        }
    }

    /// The hidden name a Clone is made under is its destination's name and
    /// a suffix of its own, nothing of the Repository URL it Clones; and
    /// the Log shows a Clone's arguments without the credential a URL among
    /// them carries.
    #[test]
    fn neither_the_hidden_name_nor_the_logged_arguments_carry_a_credential() {
        let url = "https://octocat:ghp_SECRET@example.invalid/org/atlas.git";
        let hidden = staging_name("atlas", "0123abcd");
        assert_eq!(hidden, ".atlas.suru-clone-0123abcd");
        let logged = format!(
            "{:?}",
            Shown(&["clone", "--quiet", "--branch=main", "--", url, &hidden])
        );
        assert!(!logged.contains("SECRET"), "{logged}");
        assert!(
            logged.contains("https://***@example.invalid/org/atlas.git")
                && logged.contains(&hidden),
            "{logged}"
        );
    }

    /// A Clone is made only in a directory it made itself, recorded as its
    /// run's while it stands and forgotten once it is gone: a hidden name
    /// something already holds is passed over, unrecorded, and what holds it
    /// is never touched — not when the Clone is made, nor when what it made
    /// is removed.
    #[tokio::test]
    async fn a_clone_is_made_only_in_a_directory_of_its_own() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("atlas");
        let taken = temporary.path().join(staging_name("atlas", "taken"));
        std::fs::create_dir(&taken).unwrap();
        std::fs::write(taken.join("notes.txt"), "Mine.").unwrap();
        let run = run_named("this run");

        let staged = Staged::reserve(
            &destination,
            "atlas",
            suffixes(&["taken", "free"]),
            &Cloning::default(),
            &run,
        )
        .await
        .unwrap();
        let staging = temporary.path().join(staging_name("atlas", "free"));
        assert_eq!(staged.path(), staging);
        assert_eq!(
            std::fs::read_dir(&staging)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>(),
            [MARKER],
            "a staging directory holds its marker and nothing else until Git clones into it"
        );
        assert_eq!(
            run.records.recorded().unwrap(),
            [StagingRecord {
                directory: staging.clone(),
                run: "this run".to_owned(),
            }],
            "the directory made is recorded as its run's, and the name passed over is not"
        );
        staged.remove().await.unwrap();
        assert!(!staging.exists());
        assert!(recorded(&run).is_empty(), "and forgotten once removed");
        assert_eq!(
            std::fs::read_to_string(taken.join("notes.txt")).unwrap(),
            "Mine.",
            "what another put under a hidden name is left as it was"
        );

        let staged = Staged::reserve(
            &destination,
            "atlas",
            suffixes(&["free"]),
            &Cloning::default(),
            &run,
        )
        .await
        .unwrap();
        drop(staged);
        gone(&staging).await;
        all_forgotten(&run).await;
        assert!(taken.join("notes.txt").exists());

        std::fs::create_dir(&destination).unwrap();
        assert!(matches!(
            Staged::reserve(
                &destination,
                "atlas",
                suffixes(&[]),
                &Cloning::default(),
                &run
            )
            .await,
            Err(CloneFailure::Occupied)
        ));
        assert!(recorded(&run).is_empty());
    }

    /// A staging directory whose record cannot be written is never made, so
    /// none is ever made that the next start would not know of.
    #[tokio::test]
    async fn a_staging_directory_that_cannot_be_recorded_is_never_made() {
        struct Unrecording;
        impl crate::source_control::StagingRecords for Unrecording {
            fn record(&self, _record: &StagingRecord) -> Result<(), String> {
                Err("the records cannot be written".to_owned())
            }
            fn forget(&self, _directory: &Path) -> Result<(), String> {
                Ok(())
            }
            fn recorded(&self) -> Result<Vec<StagingRecord>, String> {
                Ok(Vec::new())
            }
        }
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("atlas");
        let unrecording = CloneStaging::new(Arc::new(Unrecording));

        let Err(CloneFailure::Failed(said)) = Staged::reserve(
            &destination,
            "atlas",
            suffixes(&["unmade"]),
            &Cloning::default(),
            &unrecording,
        )
        .await
        else {
            panic!("a directory that cannot be recorded refuses the Clone");
        };
        assert!(
            said.contains("recording it failed: the records cannot be written"),
            "{said}"
        );
        assert_eq!(std::fs::read_dir(temporary.path()).unwrap().count(), 0);
    }

    /// A staging directory has its owner from the moment it is made: the
    /// guard is made where the directory is and handed over whole, so one
    /// sent to whoever asked for it, but never received because they went
    /// away first, removes it as the guard is dropped with the channel — as
    /// does one whose asker went away before it was even sent.
    #[tokio::test]
    async fn a_staging_directory_nobody_receives_is_removed() {
        /// Says so once dropped, as the reservation's thread lets go of what
        /// it ran, once it has handed over what it made.
        struct Finished(mpsc::Sender<()>);
        impl Drop for Finished {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("atlas");

        let (finished_tx, finished) = mpsc::channel();
        let finished_tx = Finished(finished_tx);
        let (release, released) = mpsc::channel::<()>();
        let suffix = move || {
            let _ = &finished_tx;
            let _ = released.recv();
            "unreceived".to_owned()
        };
        let (cloning, run) = (Cloning::default(), run_named("this run"));
        let mut reserving = Box::pin(Staged::reserve(
            &destination,
            "atlas",
            suffix,
            &cloning,
            &run,
        ));
        assert!(
            tokio::time::timeout(Duration::ZERO, &mut reserving)
                .await
                .is_err(),
            "the reservation is under way on a thread of its own"
        );
        release.send(()).unwrap();
        finished
            .recv_timeout(SURELY)
            .expect("the reservation hands over what it made");
        let unreceived = temporary.path().join(staging_name("atlas", "unreceived"));
        assert!(unreceived.exists(), "made, and sent, but not yet received");
        drop(reserving);
        gone(&unreceived).await;
        all_forgotten(&run).await;

        let (began, begun) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let (finished_tx, finished) = mpsc::channel();
        let finished_tx = Finished(finished_tx);
        let suffix = move || {
            let _ = &finished_tx;
            let _ = began.send(());
            let _ = released.recv();
            "abandoned".to_owned()
        };
        let reserving = tokio::spawn({
            let destination = destination.clone();
            async move {
                Staged::reserve(
                    &destination,
                    "atlas",
                    suffix,
                    &Cloning::default(),
                    &CloneStaging::in_memory(),
                )
                .await
            }
        });
        tokio::time::timeout(SURELY, async {
            while begun.try_recv().is_err() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the reservation begins");
        reserving.abort();
        assert!(reserving.await.is_err(), "whoever asked goes away");
        release.send(()).unwrap();
        finished
            .recv_timeout(SURELY)
            .expect("the reservation's thread finishes");
        gone(&temporary.path().join(staging_name("atlas", "abandoned"))).await;
    }

    /// A clone takes its destination's name only where nothing stands there,
    /// never replacing what does — not even an empty directory, which a
    /// plain rename would — and is removed where it cannot.
    #[tokio::test]
    async fn a_clone_takes_its_name_only_where_nothing_stands() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("atlas");
        let rename: Rename = Arc::new(rename_new);
        let staged = Staged::reserve(
            &destination,
            "atlas",
            suffixes(&["one"]),
            &Cloning::default(),
            &CloneStaging::in_memory(),
        )
        .await
        .unwrap();
        let staging = staged.path().to_owned();
        std::fs::create_dir(staging.join(CLONED)).unwrap();
        std::fs::write(staging.join(CLONED).join("README.md"), "# atlas").unwrap();
        std::fs::create_dir(&destination).unwrap();

        assert!(matches!(
            staged.land(destination.clone(), rename.clone()).await,
            Err(CloneFailure::Occupied)
        ));
        assert_eq!(
            std::fs::read_dir(&destination).unwrap().count(),
            0,
            "what stood there is left as it was"
        );
        assert!(
            !staging.exists(),
            "and the clone that could not land is removed"
        );

        std::fs::remove_dir(&destination).unwrap();
        let staged = Staged::reserve(
            &destination,
            "atlas",
            suffixes(&["two"]),
            &Cloning::default(),
            &CloneStaging::in_memory(),
        )
        .await
        .unwrap();
        let staging = staged.path().to_owned();
        std::fs::create_dir(staging.join(CLONED)).unwrap();
        std::fs::write(staging.join(CLONED).join("README.md"), "# atlas").unwrap();
        staged.land(destination.clone(), rename).await.unwrap();
        assert!(!staging.exists());
        assert_eq!(
            std::fs::read_to_string(destination.join("README.md")).unwrap(),
            "# atlas"
        );
    }

    /// Where the platform offers one, a clone is named by its own rename that
    /// refuses to replace anything, in one step, rather than by the fallback.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn the_platforms_own_rename_refuses_to_replace_even_an_empty_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let (from, to) = (temporary.path().join("from"), temporary.path().join("to"));
        std::fs::create_dir(&from).unwrap();
        std::fs::write(from.join("README.md"), "# atlas").unwrap();
        std::fs::create_dir(&to).unwrap();

        let refused = rename_exclusive(&from, &to)
            .expect("the temporary filesystem offers a rename that refuses to replace");
        assert_eq!(
            refused.map_err(|error| error.kind()),
            Err(std::io::ErrorKind::AlreadyExists)
        );
        assert_eq!(std::fs::read_dir(&to).unwrap().count(), 0);
        assert!(from.join("README.md").exists());

        std::fs::remove_dir(&to).unwrap();
        assert!(matches!(rename_exclusive(&from, &to), Some(Ok(()))));
        assert!(to.join("README.md").exists());
    }

    /// The fallback refuses what stands at the destination too, an empty
    /// directory among it. That another process might remove its own
    /// reservation between its two steps and make an empty directory there,
    /// which the rename would replace, cannot be brought about from here
    /// without a hook between two system calls; it is why the fallback is
    /// used only where no exclusive rename is to be had.
    #[cfg(unix)]
    #[test]
    fn the_fallback_rename_refuses_what_stands_there() {
        let temporary = tempfile::tempdir().unwrap();
        let (from, to) = (temporary.path().join("from"), temporary.path().join("to"));
        std::fs::create_dir(&from).unwrap();
        std::fs::write(from.join("README.md"), "# atlas").unwrap();
        std::fs::create_dir(&to).unwrap();
        assert_eq!(
            rename_reserving(&from, &to).map_err(|error| error.kind()),
            Err(std::io::ErrorKind::AlreadyExists)
        );
        assert_eq!(std::fs::read_dir(&to).unwrap().count(), 0);
        std::fs::remove_dir(&to).unwrap();
        rename_reserving(&from, &to).unwrap();
        assert!(to.join("README.md").exists() && !from.exists());
    }

    /// A Clone answers once its clone is whole, with its landing, rather
    /// than waiting on the clone to take its name — a rename held up, as a
    /// slow filesystem holds one, holds no one — and the owner sees the
    /// naming through meanwhile, touching nothing, the staging recorded all
    /// the while; once the rename goes on the clone lands whole, the landing
    /// comes to the Workspace made there, and its staging is removed and
    /// forgotten. Settling the Clones waits for all of it.
    #[tokio::test]
    async fn a_whole_clone_is_answered_at_once_and_landed_by_its_owner() {
        let temporary = tempfile::tempdir().unwrap();
        let root = crate::paths::canonical(temporary.path()).unwrap();
        let origin = origin(&root);
        let projects = root.join("projects");
        std::fs::create_dir(&projects).unwrap();
        let destination = projects.join("atlas");
        let (holding, rename) = held(rename_new);
        let adapter = Arc::new(renaming(rename));
        let run = run_named("this run");

        let landing = tokio::time::timeout(
            SURELY,
            GitSourceControl::clone_into(
                &adapter,
                origin.to_str().unwrap(),
                &destination,
                None,
                SURELY,
                &run,
            ),
        )
        .await
        .expect("the Clone answers once its clone is whole")
        .expect("the clone is whole");
        holding.begun().await;
        let staging = staging_within(&projects);
        assert_eq!(staging.len(), 1, "the whole clone waits to take its name");
        assert!(staging[0].join(CLONED).join("README.md").exists());
        assert!(!destination.exists());
        assert_eq!(
            recorded(&run),
            staging,
            "its staging recorded all the while"
        );

        let settling = tokio::spawn({
            let adapter = adapter.clone();
            async move { adapter.settle_clones().await }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), async {
                while !settling.is_finished() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .is_err(),
            "the Clones are not settled while one is still landing"
        );
        holding.release.send(()).unwrap();
        let landed = tokio::time::timeout(SURELY, landing)
            .await
            .expect("the landing comes to what it lands")
            .expect("the clone lands");
        assert_eq!(landed.path, destination);
        assert_eq!(landed.id, cloned_workspace(&destination).id);
        tokio::time::timeout(SURELY, settling)
            .await
            .expect("the Clones settle once it has landed")
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(destination.join("README.md")).unwrap(),
            "# atlas\n",
            "the owner landed the clone whole"
        );
        assert!(staging_within(&projects).is_empty());
        assert!(recorded(&run).is_empty(), "and forgot its staging");
    }

    /// A staging directory is recorded and marked as its run's, its marker
    /// locked while the Clone lives, so a sweep — even one of another run
    /// reading the same records — never takes it for abandoned, and keeps
    /// its record.
    #[tokio::test]
    async fn a_staging_directory_is_marked_as_its_runs_and_held_while_its_clone_lives() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("atlas");
        let (cloning, run) = (Cloning::default(), run_named("this run"));
        let staged = Staged::reserve(
            &destination,
            "atlas",
            suffixes(&["0123abcd"]),
            &cloning,
            &run,
        )
        .await
        .unwrap();
        let staging = staged.path().to_owned();
        assert!(is_staging_name(
            staging.file_name().unwrap().to_str().unwrap()
        ));
        assert_eq!(
            std::fs::read_to_string(staging.join(MARKER)).unwrap(),
            "this run"
        );
        let marker = std::fs::File::open(staging.join(MARKER)).unwrap();
        assert!(
            marker.try_lock_exclusive().is_err(),
            "the marker is held while the Clone lives"
        );
        drop(marker);
        for sweeping in [
            run.clone(),
            CloneStaging {
                run: "another run".to_owned(),
                ..run.clone()
            },
        ] {
            sweep(&sweeping);
            assert!(
                staging.exists(),
                "a sweep leaves a live Clone's staging alone"
            );
            assert_eq!(
                recorded(&run),
                std::slice::from_ref(&staging),
                "and its record"
            );
        }

        staged.remove().await.unwrap();
        assert!(!staging.exists());
        assert!(recorded(&run).is_empty());
    }

    /// A sweep removes what a Clone of a Server run no longer alive recorded
    /// and left — a directory it recorded, named as a Clone names its
    /// staging, its marker unheld and naming the run that recorded it — and
    /// nothing else: never a directory it did not record, however it is
    /// named and marked, as a completed clone given such a name and holding
    /// such a marker might be; nor one recorded but marked as another run's,
    /// unmarked, or named otherwise; nor one whose marker is held, nor one of
    /// this very run's. It forgets every record nothing of its Clone answers
    /// to any longer, and keeps those it cannot yet tell: held, or within a
    /// directory that cannot be read.
    #[test]
    fn a_sweep_removes_only_what_a_dead_runs_clones_recorded_and_left() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let run = run_named("this run");
        let left = |name: &str, marked: Option<&str>, recorded_as: Option<&str>| {
            let staging = root.join(name);
            std::fs::create_dir_all(staging.join(CLONED)).unwrap();
            std::fs::write(staging.join(CLONED).join("README.md"), "# atlas").unwrap();
            if let Some(marked) = marked {
                std::fs::write(staging.join(MARKER), marked).unwrap();
            }
            if let Some(recorded_as) = recorded_as {
                run.records
                    .record(&StagingRecord {
                        directory: staging.clone(),
                        run: recorded_as.to_owned(),
                    })
                    .unwrap();
            }
            staging
        };
        let abandoned = left(
            ".atlas.suru-clone-0123abcd",
            Some("a dead run"),
            Some("a dead run"),
        );
        let unrecorded = left(".maps.suru-clone-89abcdef", Some("a dead run"), None);
        let remarked = left(
            ".charts.suru-clone-00000000",
            Some("another run"),
            Some("a dead run"),
        );
        let unmarked = left(".logs.suru-clone-44444444", None, Some("a dead run"));
        let renamed = left("atlas", Some("a dead run"), Some("a dead run"));
        let ours = left(
            ".notes.suru-clone-11111111",
            Some("this run"),
            Some("this run"),
        );
        let held = left(
            ".held.suru-clone-22222222",
            Some("a dead run"),
            Some("a dead run"),
        );
        let holder = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(held.join(MARKER))
            .unwrap();
        holder.lock_exclusive().unwrap();
        let gone = root.join(".gone.suru-clone-33333333");
        let unmounted = root.join("unmounted").join(".atlas.suru-clone-55555555");
        for directory in [&gone, &unmounted] {
            run.records
                .record(&StagingRecord {
                    directory: directory.clone(),
                    run: "a dead run".to_owned(),
                })
                .unwrap();
        }

        sweep(&run);
        assert!(
            !abandoned.exists(),
            "what a dead run's Clone recorded and left is removed"
        );
        for survivor in [&unrecorded, &remarked, &unmarked, &renamed, &ours, &held] {
            assert!(
                survivor.join(CLONED).join("README.md").exists(),
                "{} is left as it was",
                survivor.display()
            );
        }
        let mut kept = vec![held.clone(), ours.clone(), unmounted.clone()];
        kept.sort();
        assert_eq!(
            recorded(&run),
            kept,
            "only what cannot yet be told, and this run's own, stays recorded"
        );
        drop(holder);
    }

    /// The fallback rename makes the destination and renames over it only
    /// while it is still the very directory it made: one another put there
    /// in its place, having removed it, refuses the rename and is left as it
    /// is.
    #[cfg(unix)]
    #[test]
    fn the_fallback_rename_refuses_a_reservation_replaced_before_it_renames() {
        let temporary = tempfile::tempdir().unwrap();
        let (from, to) = (temporary.path().join("from"), temporary.path().join("to"));
        std::fs::create_dir(&from).unwrap();
        std::fs::write(from.join("README.md"), "# atlas").unwrap();
        let theirs = temporary.path().join("theirs");
        std::fs::create_dir(&theirs).unwrap();
        std::fs::write(theirs.join("todo.txt"), "Mine.").unwrap();

        let refused = rename_reserving_with(&from, &to, || {
            std::fs::remove_dir(&to).unwrap();
            std::fs::rename(&theirs, &to).unwrap();
            std::fs::remove_file(to.join("todo.txt")).unwrap();
        });
        assert_eq!(
            refused.map_err(|error| error.kind()),
            Err(std::io::ErrorKind::AlreadyExists)
        );
        assert!(to.is_dir() && std::fs::read_dir(&to).unwrap().count() == 0);
        assert!(
            from.join("README.md").exists(),
            "nothing of the clone moved"
        );
    }

    /// A landing let go of while its whole clone takes its name — whoever
    /// awaited it gone — is still seen through by its owner, which removes
    /// nothing meanwhile; and one whose clone cannot take its name has it
    /// removed only once the rename has answered, and comes to saying so.
    #[tokio::test]
    async fn naming_a_clone_is_seen_through_by_one_owner_whatever_becomes_of_its_caller() {
        let temporary = tempfile::tempdir().unwrap();
        let root = crate::paths::canonical(temporary.path()).unwrap();
        let origin = origin(&root);
        let projects = root.join("projects");
        std::fs::create_dir(&projects).unwrap();
        let run = CloneStaging::in_memory();

        let destination = projects.join("atlas");
        let (holding, rename) = held(rename_new);
        let adapter = renaming(rename);
        let landing = adapter
            .clone_into(origin.to_str().unwrap(), &destination, None, SURELY, &run)
            .await
            .expect("the clone is whole");
        holding.begun().await;
        drop(landing);
        let staging = staging_within(&projects);
        assert_eq!(staging.len(), 1);
        assert!(
            staging[0].join(CLONED).join("README.md").exists(),
            "nothing is removed while the clone takes its name"
        );
        holding.release.send(()).unwrap();
        gone(&staging[0]).await;
        assert_eq!(
            std::fs::read_to_string(destination.join("README.md")).unwrap(),
            "# atlas\n",
            "the clone landed whole"
        );
        all_forgotten(&run).await;

        let refused = projects.join("refused");
        let (holding, rename) = held(|_, _| Err(std::io::ErrorKind::AlreadyExists.into()));
        let adapter = renaming(rename);
        let landing = adapter
            .clone_into(origin.to_str().unwrap(), &refused, None, SURELY, &run)
            .await
            .expect("the clone is whole");
        holding.begun().await;
        let staging = staging_within(&projects);
        assert_eq!(staging.len(), 1);
        assert!(staging[0].join(CLONED).join("README.md").exists());
        holding.release.send(()).unwrap();
        assert!(matches!(landing.await, Err(CloneFailure::Occupied)));
        assert!(staging_within(&projects).is_empty() && !refused.exists());
        assert!(recorded(&run).is_empty());
    }

    #[tokio::test]
    async fn removing_what_a_clone_left_removes_it_whole_read_only_objects_and_all() {
        let temporary = tempfile::tempdir().unwrap();
        let left = temporary.path().join("atlas");
        let objects = left.join(".git").join("objects").join("ab");
        std::fs::create_dir_all(&objects).unwrap();
        let object = objects.join("cdef");
        std::fs::write(&object, b"object").unwrap();
        let mut permissions = std::fs::metadata(&object).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&object, permissions).unwrap();

        remove_partial(&left).await.unwrap();
        assert!(!left.exists());
        remove_partial(&left)
            .await
            .expect("nothing there is nothing left to remove");
    }

    /// A removal abandoned while it waits to try again — as where a file it
    /// must remove is held, and whoever awaited it goes away — is finished
    /// on a task of its own once the file is let go.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_removal_abandoned_midway_is_finished_all_the_same() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("atlas");
        let staged = Staged::reserve(
            &destination,
            "atlas",
            suffixes(&["held"]),
            &Cloning::default(),
            &CloneStaging::in_memory(),
        )
        .await
        .unwrap();
        let staging = staged.path().to_owned();
        let held = staging.join("objects");
        std::fs::create_dir(&held).unwrap();
        std::fs::write(held.join("pack"), b"pack").unwrap();
        std::fs::set_permissions(&held, std::fs::Permissions::from_mode(0o555)).unwrap();
        if std::fs::remove_file(held.join("pack")).is_ok() {
            // Whoever runs this may remove what permissions forbid, so
            // nothing can be held from them here.
            std::fs::set_permissions(&held, std::fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }

        assert!(
            tokio::time::timeout(Duration::from_millis(250), staged.remove())
                .await
                .is_err(),
            "removing it waits on what is held"
        );
        assert!(staging.exists());
        std::fs::set_permissions(&held, std::fs::Permissions::from_mode(0o755)).unwrap();
        gone(&staging).await;
    }

    /// A staging directory found not to be there is taken for gone only
    /// where the directory holding it can be read: beneath one moved away —
    /// as a Root on a drive no longer mounted is — it is not found either,
    /// yet comes back with it. Removing it then fails, at once, and its
    /// record stands, so the first start once it is back removes it; one
    /// gone from a directory that can be read is forgotten.
    #[tokio::test]
    async fn a_staging_directory_whose_holder_cannot_be_read_stays_recorded_until_it_can() {
        let temporary = tempfile::tempdir().unwrap();
        let projects = temporary.path().join("projects");
        let staging = projects.join(staging_name("atlas", "0123abcd"));
        std::fs::create_dir_all(staging.join(CLONED)).unwrap();
        std::fs::write(staging.join(CLONED).join("README.md"), "# atlas").unwrap();
        drop(mark(&staging, "a run").unwrap());
        let run = run_named("a run");
        run.records
            .record(&StagingRecord {
                directory: staging.clone(),
                run: "a run".to_owned(),
            })
            .unwrap();
        let away = temporary.path().join("away");
        std::fs::rename(&projects, &away).unwrap();

        assert!(
            dismantle(&staging, &run).await.is_err(),
            "what cannot be told gone is not taken for removed"
        );
        assert_eq!(
            recorded(&run),
            std::slice::from_ref(&staging),
            "it stays recorded"
        );
        assert!(
            away.join(staging.file_name().unwrap())
                .join(CLONED)
                .join("README.md")
                .exists()
        );

        std::fs::rename(&away, &projects).unwrap();
        sweep(&CloneStaging {
            run: "the next run".to_owned(),
            ..run.clone()
        });
        assert!(
            !staging.exists(),
            "the next start removes it once it is back"
        );
        assert!(recorded(&run).is_empty());

        run.records
            .record(&StagingRecord {
                directory: staging.clone(),
                run: "a run".to_owned(),
            })
            .unwrap();
        dismantle(&staging, &run)
            .await
            .expect("one gone from a directory that can be read is gone");
        assert!(recorded(&run).is_empty(), "and forgotten");
    }
}
