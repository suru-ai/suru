//! Cloning a Repository URL into a new directory: Git's ordinary clone —
//! neither shallow nor recursing into submodules — run through the runner
//! every other Git command is, with nothing it may ask anyone.
//!
//! Git makes the clone out of sight, in a hidden directory beside its
//! destination, which discovery passes over and nobody mistakes for a
//! Workspace. That directory is made by the Clone itself before Git runs, so
//! it is the Clone's alone: a name something already holds is passed over
//! for another, and nothing the Clone removes was ever anyone else's. The
//! clone takes its destination's name only once it is whole, by a rename
//! that never replaces whatever has come to stand there meanwhile — another
//! Clone of the same name among them — so a Root never holds a half-made
//! Workspace, even while one is being made.
//!
//! The whole of it — reserving, cloning, naming, and removing what a failed
//! Clone made — is bounded by the Clone's one timeout. What a Clone that
//! fails leaves is removed before it answers; what one stopped at its
//! deadline, or abandoned by whoever awaited it, leaves is removed on a task
//! of its own, since nothing waits for it then, and the hidden directory it
//! was made in stays the Clone's to remove until it is gone.
use super::*;
use crate::source_control::CloneFailure;

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

impl GitSourceControl {
    /// Clones `url` into `destination`, as
    /// [`SourceControl::clone_repository`] describes, all of it within
    /// `timeout` but for reading the Workspace made once it has its name.
    pub(super) async fn clone_into(
        &self,
        url: &str,
        destination: &Path,
        branch: Option<&str>,
        timeout: Duration,
    ) -> Result<ResolvedWorkspace, CloneFailure> {
        let made =
            tokio::time::timeout(timeout, self.make_clone(url, destination, branch, timeout)).await;
        match made {
            Ok(Ok(())) => Ok(self.discover(destination).await),
            Ok(Err(failure)) => Err(failure),
            // What it had begun goes with the staging it was abandoned in.
            Err(_) => Err(CloneFailure::TimedOut),
        }
    }

    /// Makes the clone out of sight and gives it its name.
    async fn make_clone(
        &self,
        url: &str,
        destination: &Path,
        branch: Option<&str>,
        timeout: Duration,
    ) -> Result<(), CloneFailure> {
        let (Some(parent), Some(name)) = (
            destination.parent(),
            destination.file_name().and_then(|name| name.to_str()),
        ) else {
            return Err(CloneFailure::Failed(format!(
                "`{}` names no directory to Clone into",
                destination.display()
            )));
        };
        let mut staged = Staged::reserve(destination, name, random_suffix).await?;
        let hidden = staged
            .path()
            .file_name()
            .and_then(|hidden| hidden.to_str())
            .expect("a staging directory is named as it was made")
            .to_owned();
        let ssh = self.unasking_ssh(parent).await;
        let environment = unasking_environment(ssh.as_deref());
        let branch = branch.map(|branch| format!("--branch={branch}"));
        let mut args = vec!["clone", "--quiet"];
        args.extend(branch.as_deref());
        // Everything after `--` is the URL and the directory, however they
        // begin, so no URL is read as an option.
        args.extend(["--", url, &hidden]);
        let failure = match self.run(parent, &args, timeout, None, &environment).await {
            Ok(output) if output.status.success() => match staged.take_name(destination).await {
                Ok(()) => return Ok(()),
                Err(failure) => failure,
            },
            Ok(output) => CloneFailure::Failed(said(&output)),
            Err(Unrun::TimedOut) => CloneFailure::TimedOut,
            Err(Unrun::Failed(reason)) => CloneFailure::Failed(reason),
        };
        let staging = staged.path().to_owned();
        match staged.remove().await {
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

    /// The ssh command a Clone has Git run, where it must name one for Git
    /// to ask nothing; see [`unasking_ssh`]. Reads what the user names: in
    /// the environment, which a Clone keeps, and in Git's configuration.
    async fn unasking_ssh(&self, directory: &Path) -> Option<String> {
        let named = |name: &str| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
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

/// Eight hex digits, enough to keep two Clones' hidden names apart.
fn random_suffix() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_owned()
}

/// The hidden name a Clone of `name` is made under, told apart by `suffix`.
fn staging_name(name: &str, suffix: &str) -> String {
    format!(".{name}.suru-clone-{suffix}")
}

/// The hidden directory a Clone is made in, which this Clone made and so
/// alone may remove. It stays the Clone's to remove until it is named or
/// gone: should the Clone be abandoned before then — as where whoever
/// awaited it goes away, Git with it, or its deadline passes — it is removed
/// on a task of its own.
struct Staged(Option<PathBuf>);

impl Staged {
    /// Makes a hidden directory beside `destination` for a Clone of `name`
    /// to be made in, named by the first of `suffix`'s answers that nothing
    /// holds yet, unless something already stands at `destination`. Made on
    /// a thread of its own, so a Root that does not answer holds up no one;
    /// one made after whoever asked for it went away is removed there.
    async fn reserve(
        destination: &Path,
        name: &str,
        mut suffix: impl FnMut() -> String + Send + 'static,
    ) -> Result<Self, CloneFailure> {
        let (reserved, reservation) = tokio::sync::oneshot::channel();
        let (destination, name) = (destination.to_owned(), name.to_owned());
        tokio::task::spawn_blocking(move || {
            let made = reserve_now(&destination, &name, &mut suffix);
            if let Err(Ok(staging)) = reserved.send(made) {
                let _ = std::fs::remove_dir(&staging);
            }
        });
        match reservation.await {
            Ok(made) => made.map(|staging| Self(Some(staging))),
            Err(_) => Err(CloneFailure::Failed(
                "Making a directory to Clone into failed".to_owned(),
            )),
        }
    }

    fn path(&self) -> &Path {
        self.0
            .as_deref()
            .expect("a staged Clone is held until named or removed")
    }

    /// Gives the whole clone its name, `destination`, never over whatever
    /// has come to stand there since; once named, nothing staged is left to
    /// remove.
    async fn take_name(&mut self, destination: &Path) -> Result<(), CloneFailure> {
        let (from, to) = (self.path().to_owned(), destination.to_owned());
        let named = persistently(
            move || rename_new(&from, &to),
            |error| error.kind() == std::io::ErrorKind::AlreadyExists,
        )
        .await;
        match named {
            Ok(()) => {
                self.0 = None;
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(CloneFailure::Occupied)
            }
            Err(error) => Err(CloneFailure::Failed(format!(
                "Could not give the clone its name, `{}`: {error}",
                destination.display()
            ))),
        }
    }

    /// Removes what is staged, holding it as the Clone's until removing it
    /// has finished, so a removal abandoned midway is finished on a task of
    /// its own.
    async fn remove(mut self) -> std::io::Result<()> {
        let removed = remove_partial(self.path()).await;
        self.0 = None;
        removed
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

/// Makes the directory a Clone of `name` to `destination` is made in, as
/// [`Staged::reserve`] describes, answering where it is.
fn reserve_now(
    destination: &Path,
    name: &str,
    suffix: &mut impl FnMut() -> String,
) -> Result<PathBuf, CloneFailure> {
    if std::fs::symlink_metadata(destination).is_ok() {
        return Err(CloneFailure::Occupied);
    }
    let Some(parent) = destination.parent() else {
        return Err(CloneFailure::Failed(format!(
            "`{}` names no directory to Clone into",
            destination.display()
        )));
    };
    for _ in 0..RESERVATIONS {
        let staging = parent.join(staging_name(name, &suffix()));
        match std::fs::create_dir(&staging) {
            Ok(()) => return Ok(staging),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(CloneFailure::Failed(format!(
                    "Could not make a directory to Clone into beside `{}`: {error}",
                    destination.display()
                )));
            }
        }
    }
    Err(CloneFailure::Failed(format!(
        "Could not make a directory to Clone into beside `{}`: every name tried was taken",
        destination.display()
    )))
}

/// Renames the directory `from` to `to` where nothing stands at `to`, and
/// otherwise fails as `AlreadyExists`, replacing nothing. POSIX `rename`
/// replaces an empty directory, so `to` is first made as one — which fails
/// where anything stands there — and it is that directory, this call's own,
/// the rename replaces; one something was put in meanwhile is left as it is.
#[cfg(unix)]
fn rename_new(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir(to)?;
    std::fs::rename(from, to).map_err(|error| {
        if std::fs::remove_dir(to).is_err() {
            return std::io::ErrorKind::AlreadyExists.into();
        }
        error
    })
}

/// Renames the directory `from` to `to` where nothing stands at `to`, and
/// otherwise fails as `AlreadyExists`, replacing nothing: `MoveFileExW`
/// asked to replace nothing refuses an existing `to`.
#[cfg(windows)]
fn rename_new(from: &Path, to: &Path) -> std::io::Result<()> {
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
    if moved != 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if std::fs::symlink_metadata(to).is_ok() {
        return Err(std::io::ErrorKind::AlreadyExists.into());
    }
    Err(error)
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
    /// OpenSSH, which takes `-o BatchMode=yes`.
    OpenSsh,
    /// PuTTY's plink or TortoisePlink, which take `-batch`.
    Plink,
    /// One Git passes nothing but where to connect and what to run: no
    /// option reaches it.
    Other,
}

impl SshVariant {
    /// The variant `named` names, as `GIT_SSH_VARIANT` or `ssh.variant`
    /// does; `None` for `auto`, which leaves it to the program's name.
    fn named(named: &str) -> Option<Self> {
        match named.trim().to_ascii_lowercase().as_str() {
            "auto" => None,
            "ssh" => Some(Self::OpenSsh),
            "plink" | "putty" | "tortoiseplink" => Some(Self::Plink),
            _ => Some(Self::Other),
        }
    }

    /// The variant of the program at `program`, read as Git reads it: by
    /// its name, whatever directory it is in.
    fn of_program(program: &str) -> Self {
        let name = program
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(program)
            .to_ascii_lowercase();
        match name.strip_suffix(".exe").unwrap_or(&name) {
            "ssh" => Self::OpenSsh,
            "plink" | "tortoiseplink" => Self::Plink,
            _ => Self::Other,
        }
    }

    /// What this variant is told so it asks nothing, where anything is.
    fn unasking(&self) -> Option<&'static str> {
        match self {
            Self::OpenSsh => Some("-o BatchMode=yes"),
            Self::Plink => Some("-batch"),
            Self::Other => None,
        }
    }
}

/// The `GIT_SSH_COMMAND` a Clone has Git run so that its ssh asks nothing —
/// no passphrase, no password, no new host to trust — and a key or host it
/// would have to ask about fails the Clone at once: the very ssh Git would
/// run, as `named` names it or `ssh` where it names none, told what its
/// variant takes to ask nothing — OpenSSH `-o BatchMode=yes`, plink and
/// TortoisePlink `-batch`. `None` where nothing can be told to it: an ssh of
/// another kind, which Git passes no option, is run as the user names it,
/// and is the one way a Clone may wait on a prompt until its timeout. An
/// option the user's own command gives itself comes first, and so stands.
fn unasking_ssh(named: &SshNamed) -> Option<String> {
    let overridden = named.variant.as_deref().and_then(SshVariant::named);
    let (command, program) = match (&named.command, &named.program) {
        (Some(command), _) => (command.clone(), first_word(command)),
        (None, Some(program)) => (sh_quoted(program), program.clone()),
        (None, None) => ("ssh".to_owned(), "ssh".to_owned()),
    };
    let variant = overridden.unwrap_or_else(|| SshVariant::of_program(&program));
    variant
        .unasking()
        .map(|unasking| format!("{command} {unasking}"))
}

/// The first word of the shell command line `command`, its quotes taken
/// away, as Git reads the program a command line runs.
fn first_word(command: &str) -> String {
    let mut word = String::new();
    let mut quote = None;
    let mut characters = command.trim_start().chars();
    while let Some(character) = characters.next() {
        match (quote, character) {
            (None, character) if character.is_whitespace() => break,
            (None, '\'' | '"') => quote = Some(character),
            (Some(open), character) if character == open => quote = None,
            (None | Some('"'), '\\') => word.extend(characters.next()),
            (_, character) => word.push(character),
        }
    }
    word
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
/// there is nothing to remove.
async fn remove_partial(staging: &Path) -> std::io::Result<()> {
    let staging = staging.to_owned();
    persistently(
        move || match remove_tree(&staging) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            removed => removed,
        },
        |_| false,
    )
    .await
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
    use super::*;

    /// Waits until nothing stands at `path`, as a removal on a task of its
    /// own leaves it.
    async fn gone(path: &Path) {
        tokio::time::timeout(Duration::from_secs(30), async {
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

    #[test]
    fn a_clone_asks_no_one_anything() {
        let environment = unasking_environment(Some("ssh -o BatchMode=yes"));
        assert!(environment.contains(&("GIT_ASKPASS", "")));
        assert!(environment.contains(&("SSH_ASKPASS_REQUIRE", "never")));
        assert!(environment.contains(&("GCM_INTERACTIVE", "never")));
        assert!(environment.contains(&("GIT_SSH_COMMAND", "ssh -o BatchMode=yes")));
        assert!(
            unasking_environment(None)
                .iter()
                .all(|(name, _)| *name != "GIT_SSH_COMMAND"),
            "an ssh nothing can be told to is left as the user names it"
        );
    }

    /// Whatever ssh the user names is the one Git runs, told what its kind
    /// takes to ask nothing; one of a kind Git passes no option is left as
    /// they name it.
    #[test]
    fn the_ssh_a_clone_runs_is_the_users_own_told_to_ask_nothing() {
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
                Some("ssh -i ~/.ssh/locked_key -o BatchMode=yes"),
            ),
            (
                Some("'/opt/my ssh/bin/ssh' -F config"),
                Some("/usr/bin/plink"),
                None,
                Some("'/opt/my ssh/bin/ssh' -F config -o BatchMode=yes"),
            ),
            (
                Some(r#""C:\\Program Files\\OpenSSH\\ssh.exe" -i key"#),
                None,
                None,
                Some(r#""C:\\Program Files\\OpenSSH\\ssh.exe" -i key -o BatchMode=yes"#),
            ),
            (
                Some("plink.exe -load work"),
                None,
                None,
                Some("plink.exe -load work -batch"),
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
            (Some("my-ssh-wrapper --vpn"), None, None, None),
            (
                Some("my-ssh-wrapper --vpn"),
                None,
                Some("ssh"),
                Some("my-ssh-wrapper --vpn -o BatchMode=yes"),
            ),
            (Some("ssh -i key"), None, Some("simple"), None),
            (
                Some("ssh -i key"),
                None,
                Some("auto"),
                Some("ssh -i key -o BatchMode=yes"),
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

    /// A Clone is made only in a directory it made itself: a hidden name
    /// something already holds is passed over, and what holds it is never
    /// touched — not when the Clone is made, nor when what it made is
    /// removed.
    #[tokio::test]
    async fn a_clone_is_made_only_in_a_directory_of_its_own() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("atlas");
        let taken = temporary.path().join(staging_name("atlas", "taken"));
        std::fs::create_dir(&taken).unwrap();
        std::fs::write(taken.join("notes.txt"), "Mine.").unwrap();

        let staged = Staged::reserve(&destination, "atlas", suffixes(&["taken", "free"]))
            .await
            .unwrap();
        let staging = temporary.path().join(staging_name("atlas", "free"));
        assert_eq!(staged.path(), staging);
        assert_eq!(std::fs::read_dir(&staging).unwrap().count(), 0);
        staged.remove().await.unwrap();
        assert!(!staging.exists());
        assert_eq!(
            std::fs::read_to_string(taken.join("notes.txt")).unwrap(),
            "Mine.",
            "what another put under a hidden name is left as it was"
        );

        let staged = Staged::reserve(&destination, "atlas", suffixes(&["free"]))
            .await
            .unwrap();
        drop(staged);
        gone(&staging).await;
        assert!(taken.join("notes.txt").exists());

        std::fs::create_dir(&destination).unwrap();
        assert!(matches!(
            Staged::reserve(&destination, "atlas", suffixes(&[])).await,
            Err(CloneFailure::Occupied)
        ));
    }

    /// A clone takes its destination's name only where nothing stands there,
    /// never replacing what does — not even an empty directory, which a
    /// plain rename would.
    #[tokio::test]
    async fn a_clone_takes_its_name_only_where_nothing_stands() {
        let temporary = tempfile::tempdir().unwrap();
        let destination = temporary.path().join("atlas");
        let mut staged = Staged::reserve(&destination, "atlas", suffixes(&["one"]))
            .await
            .unwrap();
        std::fs::write(staged.path().join("README.md"), "# atlas").unwrap();
        std::fs::create_dir(&destination).unwrap();

        assert!(matches!(
            staged.take_name(&destination).await,
            Err(CloneFailure::Occupied)
        ));
        assert_eq!(
            std::fs::read_dir(&destination).unwrap().count(),
            0,
            "what stood there is left as it was"
        );
        assert!(staged.path().join("README.md").exists());

        std::fs::remove_dir(&destination).unwrap();
        let staging = staged.path().to_owned();
        staged.take_name(&destination).await.unwrap();
        drop(staged);
        assert!(!staging.exists());
        assert_eq!(
            std::fs::read_to_string(destination.join("README.md")).unwrap(),
            "# atlas"
        );
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
        let staged = Staged::reserve(&destination, "atlas", suffixes(&["held"]))
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
}
