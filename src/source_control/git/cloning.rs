//! Cloning a Repository URL into a new directory: Git's ordinary clone —
//! neither shallow nor recursing into submodules — run through the runner
//! every other Git command is, with nothing it may ask anyone.
//!
//! Git makes the clone out of sight, in a hidden directory beside its
//! destination, which discovery passes over and nobody mistakes for a
//! Workspace, and the clone takes its destination's name only once it is
//! whole — never over whatever has come to stand there meanwhile, another
//! Clone of the same name among them. So a Root never holds a half-made
//! Workspace, even while one is being made. What a Clone that fails, runs
//! out of time, or is abandoned by whoever awaited it leaves there is
//! removed.
use super::*;
use crate::source_control::CloneFailure;

/// The ssh a Clone runs where the user names no ssh of their own: one that
/// asks nothing — no passphrase, no password, no new host to trust — so a key
/// or host it would have to ask about fails the Clone at once.
const UNASKING_SSH: &str = "ssh -o BatchMode=yes";

/// How many more times removing or naming what a Clone made is tried once it
/// has failed, and how long apart: a Git stopped at its deadline may still be
/// letting go of what it held, and on Windows a file just written may be
/// held a moment longer by whatever reads it, so what Git made can be
/// neither removed nor renamed until then.
const RETRIES: u32 = 20;
const RETRY: Duration = Duration::from_millis(50);

impl GitSourceControl {
    /// Clones `url` into `destination`, as
    /// [`SourceControl::clone_repository`] describes.
    pub(super) async fn clone_into(
        &self,
        url: &str,
        destination: &Path,
        branch: Option<&str>,
        timeout: Duration,
    ) -> Result<ResolvedWorkspace, CloneFailure> {
        let (Some(parent), Some(name)) = (
            destination.parent(),
            destination.file_name().and_then(|name| name.to_str()),
        ) else {
            return Err(CloneFailure::Failed(format!(
                "`{}` names no directory to Clone into",
                destination.display()
            )));
        };
        if std::fs::symlink_metadata(destination).is_ok() {
            return Err(CloneFailure::Occupied);
        }
        let hidden = format!(
            ".{name}.suru-clone-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        );
        let staged = Staged(Some(parent.join(&hidden)));
        let environment = unasking_environment(self.names_its_own_ssh(parent).await);
        let branch = branch.map(|branch| format!("--branch={branch}"));
        let mut args = vec!["clone", "--quiet"];
        args.extend(branch.as_deref());
        // Everything after `--` is the URL and the directory, however they
        // begin, so no URL is read as an option.
        args.extend(["--", url, &hidden]);
        let failure = match self.run(parent, &args, timeout, None, &environment).await {
            Ok(output) if output.status.success() => {
                match take_name(staged.path(), destination).await {
                    Ok(()) => {
                        staged.disarm();
                        return Ok(self.discover(destination).await);
                    }
                    Err(failure) => failure,
                }
            }
            Ok(output) => CloneFailure::Failed(said(&output)),
            Err(Unrun::TimedOut) => CloneFailure::TimedOut,
            Err(Unrun::Failed(reason)) => CloneFailure::Failed(reason),
        };
        let staging = staged.disarm();
        match remove_partial(&staging).await {
            Ok(()) => Err(failure),
            Err(error) => {
                tracing::warn!(
                    staging = %staging.display(),
                    "what a Clone left behind could not be removed: {error}"
                );
                let why = match failure {
                    CloneFailure::Failed(said) => said,
                    CloneFailure::Occupied => {
                        "Something came to stand at its destination".to_owned()
                    }
                    _ => "It did not finish in time".to_owned(),
                };
                Err(CloneFailure::LeftBehind(format!(
                    "{why}; removing what it left at `{}` failed: {error}",
                    staging.display()
                )))
            }
        }
    }

    /// Whether the user names the ssh Git runs: in the environment, which a
    /// Clone keeps, or in Git's configuration, which naming one in the
    /// environment would override. Theirs is left to them.
    async fn names_its_own_ssh(&self, directory: &Path) -> bool {
        ["GIT_SSH_COMMAND", "GIT_SSH"]
            .into_iter()
            .any(|name| std::env::var_os(name).is_some())
            || self
                .text(directory, &["config", "--get", "core.sshCommand"])
                .await
                .is_some()
    }
}

/// The hidden directory a Clone is made in, removed on a task of its own
/// should the Clone be abandoned before it is named or removed — as where
/// whoever awaited it goes away, and Git with it.
struct Staged(Option<PathBuf>);

impl Staged {
    fn path(&self) -> &Path {
        self.0
            .as_deref()
            .expect("a staged Clone is held until disarmed")
    }

    /// Leaves what is staged to whoever disarms it, answering where it is.
    fn disarm(mut self) -> PathBuf {
        self.0.take().expect("a staged Clone is disarmed once")
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if let Some(staging) = self.0.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move {
                if let Err(error) = remove_partial(&staging).await {
                    tracing::warn!(
                        staging = %staging.display(),
                        "what an abandoned Clone left behind could not be removed: {error}"
                    );
                }
            });
        }
    }
}

/// What a Clone's Git is run with, beside what every Git command is, so that
/// nothing it runs asks anyone anything: a credential it cannot find, or a
/// host it would have to be told to trust, fails it at once rather than
/// leaving it waiting on a prompt nobody sees. The ssh the user names, where
/// `names_its_own_ssh`, is theirs to make so.
fn unasking_environment(names_its_own_ssh: bool) -> Vec<(&'static str, &'static str)> {
    let mut environment = vec![
        // An empty askpass is none, whatever `core.askPass` or `SSH_ASKPASS`
        // names, so Git falls back on the terminal prompt that
        // `GIT_TERMINAL_PROMPT=0` already refuses.
        ("GIT_ASKPASS", ""),
        // Nor does ssh ask through a program of its own,
        ("SSH_ASKPASS_REQUIRE", "never"),
        // nor Git Credential Manager through a window of its own.
        ("GCM_INTERACTIVE", "never"),
    ];
    if !names_its_own_ssh {
        environment.push(("GIT_SSH_COMMAND", UNASKING_SSH));
    }
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

/// Gives the whole clone at `staging` its name, `destination`, unless
/// something has come to stand there since, which it never replaces.
async fn take_name(staging: &Path, destination: &Path) -> Result<(), CloneFailure> {
    let (from, to) = (staging.to_owned(), destination.to_owned());
    let named = persistently(move || {
        if std::fs::symlink_metadata(&to).is_ok() {
            return Err(std::io::ErrorKind::AlreadyExists.into());
        }
        std::fs::rename(&from, &to)
    })
    .await;
    match named {
        Ok(()) => Ok(()),
        Err(_) if std::fs::symlink_metadata(destination).is_ok() => Err(CloneFailure::Occupied),
        Err(error) => Err(CloneFailure::Failed(format!(
            "Could not give the clone its name, `{}`: {error}",
            destination.display()
        ))),
    }
}

/// Removes what a Clone that did not finish left at `staging`; nothing
/// there is nothing to remove.
async fn remove_partial(staging: &Path) -> std::io::Result<()> {
    let staging = staging.to_owned();
    persistently(move || match remove_tree(&staging) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        removed => removed,
    })
    .await
}

/// Runs `act` on a thread of its own until it succeeds, trying it again a
/// few times, a moment apart, where it fails.
async fn persistently(
    act: impl Fn() -> std::io::Result<()> + Clone + Send + 'static,
) -> std::io::Result<()> {
    let mut retries = 0;
    loop {
        let attempt = act.clone();
        let acted = tokio::task::spawn_blocking(attempt)
            .await
            .unwrap_or_else(|error| Err(std::io::Error::other(error)));
        match acted {
            Ok(()) => return Ok(()),
            Err(error) if retries == RETRIES => return Err(error),
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
    use super::*;

    #[test]
    fn a_clone_asks_no_one_and_runs_an_ssh_that_never_asks_unless_the_user_names_their_own() {
        let theirs = unasking_environment(true);
        assert!(theirs.contains(&("GIT_ASKPASS", "")));
        assert!(theirs.contains(&("SSH_ASKPASS_REQUIRE", "never")));
        assert!(theirs.contains(&("GCM_INTERACTIVE", "never")));
        assert!(
            theirs.iter().all(|(name, _)| *name != "GIT_SSH_COMMAND"),
            "an ssh the user names is left to them"
        );
        let ours = unasking_environment(false);
        assert!(ours.contains(&("GIT_SSH_COMMAND", "ssh -o BatchMode=yes")));
        assert!(ours.contains(&("GIT_ASKPASS", "")));
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
}
