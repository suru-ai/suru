//! Cloning a Repository URL into a new directory: Git's ordinary clone —
//! neither shallow nor recursing into submodules — run through the runner
//! every other Git command is, with nothing it may ask anyone.
//!
//! The destination is made before Git runs, so a Clone never writes into
//! what another put there, nor removes it: two Clones of one name race to
//! make it, and the one that loses is refused as though it had found it.
//! What a Clone that fails or is stopped leaves there is removed, the
//! directory with it.
use super::*;
use crate::source_control::CloneFailure;

/// The ssh a Clone runs where the user names no ssh of their own: one that
/// asks nothing — no passphrase, no password, no new host to trust — so a key
/// or host it would have to ask about fails the Clone at once.
const UNASKING_SSH: &str = "ssh -o BatchMode=yes";

/// How many more times removing what a Clone left behind is tried once it
/// has failed, and how long apart: a Git stopped at its deadline may still
/// be letting go of what it held, and on Windows its files cannot be removed
/// until it has.
const REMOVAL_RETRIES: u32 = 20;
const REMOVAL_RETRY: Duration = Duration::from_millis(50);

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
        match std::fs::create_dir(destination) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(CloneFailure::Occupied);
            }
            Err(error) => {
                return Err(CloneFailure::Failed(format!(
                    "Could not make `{}`: {error}",
                    destination.display()
                )));
            }
        }
        let environment = unasking_environment(self.names_its_own_ssh(parent).await);
        let branch = branch.map(|branch| format!("--branch={branch}"));
        let mut args = vec!["clone", "--quiet"];
        args.extend(branch.as_deref());
        // Everything after `--` is the URL and the directory, however they
        // begin, so no URL is read as an option.
        args.extend(["--", url, name]);
        let failure = match self.run(parent, &args, timeout, None, &environment).await {
            Ok(output) if output.status.success() => return Ok(self.discover(destination).await),
            Ok(output) => CloneFailure::Failed(said(&output)),
            Err(Unrun::TimedOut) => CloneFailure::TimedOut,
            Err(Unrun::Failed(reason)) => CloneFailure::Failed(reason),
        };
        match remove_partial(destination).await {
            Ok(()) => Err(failure),
            Err(error) => {
                tracing::warn!(
                    destination = %destination.display(),
                    "what a Clone left behind could not be removed: {error}"
                );
                let why = match failure {
                    CloneFailure::Failed(said) => said,
                    CloneFailure::TimedOut => "It did not finish in time".to_owned(),
                    CloneFailure::Occupied => unreachable!("an occupied destination runs no Git"),
                };
                Err(CloneFailure::Failed(format!(
                    "{why}; and what it left at `{}` could not be removed: {error}",
                    destination.display()
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

/// Removes what a Clone that did not finish left at `destination`, the
/// directory it made itself, trying again a few times where that fails.
async fn remove_partial(destination: &Path) -> std::io::Result<()> {
    let mut retries = 0;
    loop {
        let removing = destination.to_owned();
        let removed = tokio::task::spawn_blocking(move || remove_tree(&removing))
            .await
            .unwrap_or_else(|error| Err(std::io::Error::other(error)));
        match removed {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) if retries == REMOVAL_RETRIES => return Err(error),
            Err(_) => {
                retries += 1;
                tokio::time::sleep(REMOVAL_RETRY).await;
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
