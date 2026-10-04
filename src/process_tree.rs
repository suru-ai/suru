//! A child process held together with every process it starts.
//!
//! Suru launches processes that launch processes of their own: a Provider's harness runs tool
//! shells and MCP servers, and Git runs hooks and remote helpers. Signalling the child alone leaves
//! those descendants running as orphans of init, so each such child is launched as the root of a
//! [`ProcessTree`] that contains its descendants too — a process group on Unix, a Job Object on
//! Windows — and is taken down whole.
//!
//! A tree is taken down when it is terminated, and when it is dropped while its root still runs,
//! however that comes about: the task waiting on it cancelled at a deadline, the runtime that task
//! ran on shut down, or a panic unwinding past it. A tree whose root exits on its own takes down
//! what the root left running or leaves it be, as the [`Descendants`] it was launched with say.
//!
//! A tree is taken down as well when the Suru process holding it ends without dropping it at all:
//! killed outright, ended by the out-of-memory killer, or aborted by a crash. On Windows the
//! kernel closing the tree's Job Object does that. On Unix the tree's process group is led by an
//! anchor that kills the group once Suru is gone; see `ANCHOR_SCRIPT`.

use std::{
    io,
    pin::pin,
    process::{ExitStatus, Output, Stdio},
    sync::{Arc, Mutex as StdMutex, MutexGuard, PoisonError, Weak},
    task::{Context, Poll, ready},
};

use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

/// What becomes of the processes a tree's root leaves running when it exits on its own. A tree
/// abandoned while its root still runs is taken down whole either way.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Descendants {
    /// They end with it. A harness's tool shells and MCP servers serve nothing but the harness.
    EndWithRoot,
    /// They are left to run. A Git hook may start work in the background meant to outlive the
    /// command that ran it, such as regenerating tags after a checkout. On Windows they are left
    /// to run even when Suru is ended outright while the root still runs, since only a tree whose
    /// descendants end with it has its Job Object kill them as it closes.
    MayOutliveRoot,
}

/// A launched child process and the descendants it starts, taken down together.
///
/// The tree is the one owner of its root, so nothing else can wait on the root and free its
/// process ID behind the back of a tree that signals it. Whoever must be able to take the tree
/// down without owning it holds a [`ProcessTreeTerminator`].
pub(crate) struct ProcessTree {
    root: Arc<StdMutex<Root>>,
}

/// The pipes a tree's root was launched with, handed over to be read and written apart from the
/// tree, so whatever speaks over them never holds the tree up.
pub(crate) struct ProcessPipes {
    pub(crate) stdin: Option<ChildStdin>,
    pub(crate) stdout: Option<ChildStdout>,
    pub(crate) stderr: Option<ChildStderr>,
}

/// Takes a [`ProcessTree`] down from wherever it is held without keeping it alive: once the tree
/// is gone there is nothing left for it to do.
#[derive(Clone, Debug)]
pub(crate) struct ProcessTreeTerminator {
    root: Weak<StdMutex<Root>>,
}

/// The root process and what contains its descendants, locked as one so that letting go of the
/// tree as its root exits and signalling the tree never interleave.
struct Root {
    child: Child,
    containment: Containment,
    descendants: Descendants,
}

impl ProcessTree {
    /// Launches `command` as the root of a tree of its own, its pipes handed back beside it.
    ///
    /// Fails rather than launching the root uncontained: on Unix, should the anchor that would
    /// take the tree down with Suru not launch, or not say it is ready, neither does the root.
    pub(crate) async fn spawn(
        command: &mut Command,
        descendants: Descendants,
    ) -> io::Result<(Self, ProcessPipes)> {
        prepare_root(command);
        let (child, containment) = spawn_contained(command, descendants).await?;
        Ok(Self::contain(child, containment, descendants))
    }

    fn contain(
        mut child: Child,
        containment: Containment,
        descendants: Descendants,
    ) -> (Self, ProcessPipes) {
        let pipes = ProcessPipes {
            stdin: child.stdin.take(),
            stdout: child.stdout.take(),
            stderr: child.stderr.take(),
        };
        let root = Root {
            child,
            containment,
            descendants,
        };
        (
            Self {
                root: Arc::new(StdMutex::new(root)),
            },
            pipes,
        )
    }

    /// The root's process ID, until it has been waited on.
    pub(crate) fn id(&self) -> Option<u32> {
        lock(&self.root).child.id()
    }

    /// Takes the whole tree down at once, unless its root has already been waited on: by then
    /// the tree has been dealt with as its [`Descendants`] say.
    pub(crate) fn terminate(&self) -> io::Result<()> {
        lock(&self.root).terminate()
    }

    /// A handle that takes this tree down from elsewhere for as long as it is held.
    pub(crate) fn terminator(&self) -> ProcessTreeTerminator {
        ProcessTreeTerminator {
            root: Arc::downgrade(&self.root),
        }
    }

    /// Waits for the root to exit and reaps it, taking down whatever it left running in the
    /// tree when its descendants [`Descendants::EndWithRoot`], and then lets go of the tree.
    pub(crate) async fn wait(&self) -> io::Result<ExitStatus> {
        std::future::poll_fn(|context| lock(&self.root).poll_wait(context)).await
    }
}

#[cfg(all(test, unix))]
impl ProcessTree {
    /// Launches `command` as [`ProcessTree::spawn`] does, but into the group of an anchor `shell`
    /// runs, reading `lifeline` in place of this process's own, and given `readiness` to say it
    /// is ready.
    pub(crate) async fn spawn_anchored_by(
        command: &mut Command,
        descendants: Descendants,
        shell: &str,
        lifeline: io::PipeReader,
        readiness: std::time::Duration,
    ) -> io::Result<(Self, ProcessPipes)> {
        prepare_root(command);
        let (child, containment) = spawn_anchored(command, shell, lifeline, readiness).await?;
        Ok(Self::contain(child, containment, descendants))
    }

    /// The process ID of the anchor leading the tree's group, until the tree lets go of it.
    pub(crate) fn anchor_id(&self) -> Option<libc::pid_t> {
        lock(&self.root)
            .containment
            .anchor
            .as_ref()
            .and_then(Child::id)
            .map(|id| id as libc::pid_t)
    }

    /// Whether the tree is letting go of its group, its anchor killed but not yet reaped.
    pub(crate) fn is_letting_go(&self) -> bool {
        let root = lock(&self.root);
        root.containment.let_go && root.containment.anchor.is_some()
    }
}

/// Readies `command` to be a tree's root, whatever contains it.
fn prepare_root(command: &mut Command) {
    // The root's own handle signals it alone, should the tree somehow fail to; the tree's
    // drop signals everything the root started before this does.
    command.kill_on_drop(true);
}

impl ProcessTreeTerminator {
    /// Takes the tree down at once, if it is still held and its root has not been waited on.
    pub(crate) fn terminate(&self) {
        let Some(root) = self.root.upgrade() else {
            return;
        };
        if let Err(error) = lock(&root).terminate() {
            tracing::debug!(%error, "could not take down a process tree");
        }
    }
}

/// Runs `command` to completion as the root of a tree of its own and collects its output, as
/// [`Command::output`] does — except that abandoning the wait, at a deadline say, takes down the
/// command and everything it started rather than the command alone. What the command leaves
/// running once it exits on its own, and once its output is collected, is left be. Its stdin is
/// whatever the caller set.
pub(crate) async fn output(command: &mut Command) -> io::Result<Output> {
    use tokio::io::AsyncReadExt;

    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let (tree, pipes) = ProcessTree::spawn(command, Descendants::MayOutliveRoot).await?;
    drop(pipes.stdin);
    async fn read_to_end(pipe: Option<impl tokio::io::AsyncRead + Unpin>) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            pipe.read_to_end(&mut bytes).await?;
        }
        Ok(bytes)
    }
    // The output is collected before the root is waited on, not beside it. Something the command
    // left running may hold its output open past its exit; waiting on the root then would let go
    // of the tree while the collection still waits, and a deadline abandoning it could no longer
    // take that straggler down. Unwaited, the tree stays abandonable whole until the output is in.
    let (stdout, stderr) = tokio::try_join!(read_to_end(pipes.stdout), read_to_end(pipes.stderr))?;
    let status = tree.wait().await?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Locks a tree's root even where a panic poisoned the lock: taking the tree down matters most
/// on exactly that path, and nothing the lock guards is left half-changed by one.
fn lock(root: &StdMutex<Root>) -> MutexGuard<'_, Root> {
    root.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Root {
    /// Takes the whole tree down, unless the root has already been waited on.
    ///
    /// On Unix the tree is the process group its anchor leads, named by the anchor's process ID.
    /// The system may hand that ID out again — as a process ID, and so as the ID of a new group —
    /// only once the anchor has been reaped: until then it holds the ID, running or as a zombie,
    /// so the group the ID names can only be this tree's. The tree reaps its anchor only once it
    /// has let go of the group, which it does under the same lock as this, so no signal sent here
    /// can reach a stranger.
    ///
    /// On Windows the Job Object is held by handle and cannot be confused with another.
    ///
    /// Either way the check below only keeps a tree whose descendants
    /// [`Descendants::MayOutliveRoot`] from taking down what its root left running once the root
    /// has exited and been waited on.
    fn terminate(&mut self) -> io::Result<()> {
        if self.child.id().is_none() {
            return Ok(());
        }
        self.kill_tree()
    }

    /// Waits for the root to exit and reaps it, then deals with the rest of the tree as its
    /// descendants say, and lets go of the tree — each step under the tree's lock, so nothing can
    /// take the tree down part way through one.
    ///
    /// The wait is done only once whatever contained the tree has been reaped too, so it is not
    /// left to Tokio to reap — which it does only while some runtime runs, and the one this wait
    /// ran on may stop as soon as it is done.
    fn poll_wait(&mut self, context: &mut Context<'_>) -> Poll<io::Result<ExitStatus>> {
        // Each poll waits afresh: Tokio keeps what a wait has registered in the child itself,
        // and answers at once with the status of a child it has already reaped.
        let status = ready!(pin!(self.child.wait()).poll(context))?;
        if self.descendants == Descendants::EndWithRoot {
            let _ = self.kill_tree();
        }
        ready!(self.containment.poll_let_go(context));
        Poll::Ready(Ok(status))
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        if self.child.id().is_some() {
            if let Err(error) = self.kill_tree() {
                tracing::debug!(%error, "could not take down an abandoned process tree");
            }
            reap_killed(&mut self.child);
        }
        // What contained the tree is let go of as it is dropped in turn.
    }
}

/// How long a tree waits, at most, for a process it killed to be gone so it can reap the process
/// itself, looking again each step. A killed process is gone in well under a millisecond, so
/// this is a bound, not a wait: it only keeps a process the kernel is slow to let go of from
/// blocking whoever dropped the tree, or launched it.
const KILLED_REAP_BOUND: std::time::Duration = std::time::Duration::from_millis(100);
const KILLED_REAP_STEP: std::time::Duration = std::time::Duration::from_millis(1);

/// Reaps a process of a tree just killed, rather than leaving it to Tokio, which reaps a child it
/// has been handed back only once some runtime's driver next runs — never, where the tree was
/// dropped with the last runtime, leaving the process a zombie for the life of Suru. A process
/// still not gone at the bound is left to Tokio after all.
fn reap_killed(child: &mut Child) {
    let deadline = std::time::Instant::now() + KILLED_REAP_BOUND;
    while let Ok(None) = child.try_wait() {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return;
        }
        std::thread::sleep(KILLED_REAP_STEP.min(left));
    }
}

/// What the anchor leading a tree's process group runs on Unix, tying the tree to the life of the
/// Suru process holding it.
///
/// The anchor is launched first, as the leader of a group of its own, and the root then joins
/// that group, so the group's ID is the anchor's process ID rather than the root's. The anchor
/// reads its stdin, which is this process's lifeline (see [`Lifeline`]), until it ends — as it
/// does only once this process has ended, however it ended — and then kills its whole group,
/// itself included. `kill 0` names the anchor's own group, which is the tree's for as long as
/// the anchor lives, so that kill can reach no stranger, however long Suru has been gone.
///
/// The anchor ignores every signal that would end or stop it short of SIGKILL, so a signal sent
/// to the whole group — by a tool tidying up with `kill 0`, say — reaches the programs in it but
/// leaves the tree anchored: like the tree, the anchor is ended only by being killed, by the tree
/// letting go of it or taking the group down, or by itself. Once it is ignoring them it says so,
/// writing [`ANCHOR_READY`] to its stdout, and only then is the root launched into its group, so
/// nothing in the group can signal it before it is ready. Someone killing it outright can still
/// end it; the tree then loses only its tie to Suru's life, since its group stays the tree's for
/// as long as Suru leaves the dead anchor unreaped.
///
/// The anchor stands beside the root rather than the root being launched through it, so the root
/// is still Suru's own child: Suru waits on it directly, sees how it truly exited — the code it
/// exited with or the signal that ended it — and speaks over its stdin, stdout and stderr with
/// nothing between.
///
/// It is a shell rather than Suru itself, so launching one needs no Suru executable to be found:
/// a test binary hosts the Server in tests, and an installed Suru may be replaced or removed
/// under a running Server. The script is plain POSIX shell, run by the first of
/// [`ANCHOR_SHELLS`] there is; its environment is cleared, so no startup file that environment
/// names runs in it.
#[cfg(unix)]
const ANCHOR_SCRIPT: &str = "trap '' HUP INT QUIT TERM ALRM USR1 USR2 TSTP TTIN TTOU
printf r
exec >&-
while read -r line; do :; done
kill -s KILL 0";

/// The shells an anchor may be run by, in the order they are preferred. `/bin/sh` is where every
/// Unix Suru runs on keeps the shell its C library's `system` and `popen` run commands with.
/// Dash is preferred where it is installed, since every tree's launch waits on its anchor being
/// ready: it is what `/bin/sh` already is on Debian and Ubuntu, and on macOS, where `/bin/sh` is
/// a stand-in that itself launches Bash, it is ready in about a millisecond rather than four.
#[cfg(unix)]
const ANCHOR_SHELLS: [&str; 2] = ["/bin/dash", "/bin/sh"];

/// The first of [`ANCHOR_SHELLS`] there is, or the last, to fail to launch for want of it.
#[cfg(unix)]
fn anchor_shell() -> &'static str {
    static CHOSEN: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    CHOSEN.get_or_init(|| {
        ANCHOR_SHELLS
            .into_iter()
            .find(|shell| std::path::Path::new(shell).is_file())
            .unwrap_or(ANCHOR_SHELLS[ANCHOR_SHELLS.len() - 1])
    })
}

/// What an anchor writes to say it is ready.
#[cfg(unix)]
const ANCHOR_READY: u8 = b'r';

/// How long an anchor is given to say it is ready; a tree whose anchor has not by then is not
/// launched at all. An anchor is ready in a few milliseconds, so this bounds only a failure,
/// generously enough for a machine loaded down with launching many processes at once.
#[cfg(unix)]
const ANCHOR_READINESS_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// The pipe whose reading end every anchor holds as its stdin, learning by its end that this
/// process has ended. Its writing end is held here for the rest of the process's life and never
/// written to, so only the kernel closing it as the process ends can end an anchor's read.
///
/// Both ends are close-on-exec, so no process Suru launches holds the writing end open past
/// Suru. Where the system cannot create a pipe close-on-exec at once, as on macOS, a process
/// launched while it is being created could still inherit it; every tree's launch takes the
/// lifeline first, waiting on its creation, and the Server launches nothing but trees.
#[cfg(unix)]
struct Lifeline {
    reader: io::PipeReader,
    _writer: io::PipeWriter,
}

/// This process's lifeline, created on first use, for an anchor to read.
#[cfg(unix)]
fn lifeline() -> io::Result<io::PipeReader> {
    static LIFELINE: std::sync::OnceLock<Lifeline> = std::sync::OnceLock::new();
    static CREATING: StdMutex<()> = StdMutex::new(());

    if let Some(lifeline) = LIFELINE.get() {
        return lifeline.reader.try_clone();
    }
    let _creating = CREATING.lock().unwrap_or_else(PoisonError::into_inner);
    let lifeline = match LIFELINE.get() {
        Some(lifeline) => lifeline,
        None => {
            let (reader, writer) = io::pipe()?;
            LIFELINE.get_or_init(|| Lifeline {
                reader,
                _writer: writer,
            })
        }
    };
    lifeline.reader.try_clone()
}

/// The anchor leading a tree's process group, until it has been reaped.
#[cfg(unix)]
struct Containment {
    anchor: Option<Child>,
    /// Whether the tree has let go of its group, the group's ID no longer its to signal.
    let_go: bool,
}

#[cfg(unix)]
async fn spawn_contained(
    command: &mut Command,
    _descendants: Descendants,
) -> io::Result<(Child, Containment)> {
    spawn_anchored(
        command,
        anchor_shell(),
        lifeline()?,
        ANCHOR_READINESS_DEADLINE,
    )
    .await
}

/// Launches `command` into the group of an anchor `shell` runs, reading `lifeline`, once the
/// anchor has said within `readiness` that it is ready — or launches nothing.
#[cfg(unix)]
async fn spawn_anchored(
    command: &mut Command,
    shell: &str,
    lifeline: io::PipeReader,
    readiness: std::time::Duration,
) -> io::Result<(Child, Containment)> {
    // Held from here on, so a launch that fails or is abandoned takes the anchor down with it.
    let containment = Containment::launch(shell, lifeline, readiness).await?;
    let process_group_id = containment
        .anchor
        .as_ref()
        .and_then(Child::id)
        .and_then(|id| libc::pid_t::try_from(id).ok())
        .filter(|id| *id > 0)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "launched anchor had no process ID",
            )
        })?;
    command.process_group(process_group_id);
    let child = command.spawn()?;
    Ok((child, containment))
}

/// Launches an anchor; see [`ANCHOR_SCRIPT`].
#[cfg(unix)]
fn spawn_anchor(shell: &str, lifeline: io::PipeReader) -> io::Result<Child> {
    Command::new(shell)
        .args(["-c", ANCHOR_SCRIPT])
        .env_clear()
        // Nowhere a Workspace could be, so an anchor never holds one in use.
        .current_dir("/")
        .stdin(lifeline)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|error| {
            // Of no kind of its own, lest a missing shell be taken for a missing program.
            io::Error::other(format!(
                "could not launch {shell} to anchor a process tree: {error}"
            ))
        })
}

#[cfg(unix)]
impl Root {
    /// Kills every process in the tree's group, its anchor included, while the tree still holds
    /// the group; see [`Root::terminate`].
    fn kill_tree(&mut self) -> io::Result<()> {
        let killed = self.containment.kill_group();
        // The root is signalled directly as well, in case it left its group for a group or
        // session of its own. Tokio signals it only while it is unreaped, and so its ID its own.
        let _ = self.child.start_kill();
        killed
    }
}

#[cfg(unix)]
impl Containment {
    /// Kills every process in the group the anchor leads, unless the group has been let go of.
    fn kill_group(&self) -> io::Result<()> {
        if self.let_go {
            return Ok(());
        }
        // Held, the anchor is unreaped: the tree only waits on it once it has let go of it.
        let Some(process_group_id) = self
            .anchor
            .as_ref()
            .and_then(Child::id)
            .and_then(|id| libc::pid_t::try_from(id).ok())
        else {
            return Ok(());
        };
        if unsafe { libc::killpg(process_group_id, libc::SIGKILL) } == -1 {
            let error = io::Error::last_os_error();
            // The group is empty: everything in it has exited, the anchor included.
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        Ok(())
    }

    /// Launches an anchor reading `lifeline` and waits up to `readiness` for it to say it is
    /// ready. An anchor that does not is killed and reaped as the containment is dropped.
    async fn launch(
        shell: &str,
        lifeline: io::PipeReader,
        readiness: std::time::Duration,
    ) -> io::Result<Self> {
        use tokio::io::AsyncReadExt;

        let mut anchor = spawn_anchor(shell, lifeline)?;
        let announcement = anchor.stdout.take();
        let containment = Self {
            anchor: Some(anchor),
            let_go: false,
        };
        let said = tokio::time::timeout(readiness, async {
            let mut said = [0];
            match announcement {
                Some(mut announcement) => announcement.read_exact(&mut said).await.map(|_| said[0]),
                None => Err(io::Error::other("its stdout was unavailable")),
            }
        })
        .await;
        match said {
            Ok(Ok(ANCHOR_READY)) => Ok(containment),
            Ok(Ok(other)) => Err(io::Error::other(format!(
                "the {shell} anchoring a process tree said {other:#04x}, not that it was ready"
            ))),
            Ok(Err(error)) => Err(io::Error::other(format!(
                "the {shell} anchoring a process tree did not say it was ready: {error}"
            ))),
            Err(_) => Err(io::Error::other(format!(
                "the {shell} anchoring a process tree did not say it was ready within {readiness:?}"
            ))),
        }
    }

    /// Lets go of the group, killing its anchor alone, and waits for the anchor to be reaped.
    /// What is left in the group no longer goes down with Suru, and the group's ID is no longer
    /// the tree's to signal.
    fn poll_let_go(&mut self, context: &mut Context<'_>) -> Poll<()> {
        let Some(anchor) = self.anchor.as_mut() else {
            return Poll::Ready(());
        };
        if !self.let_go {
            self.let_go = true;
            let _ = anchor.start_kill();
        }
        // Killed, the anchor is gone at once; only a failure to wait on it at all is left to
        // Tokio, as the anchor is dropped.
        let _ = ready!(pin!(anchor.wait()).poll(context));
        self.anchor = None;
        Poll::Ready(())
    }
}

/// An anchor still held as its containment is dropped — the tree dropped, its launch failed or
/// was abandoned, or a wait abandoned while letting go of it — is killed alone and reaped here:
/// whatever was to be taken down with it already has been.
#[cfg(unix)]
impl Drop for Containment {
    fn drop(&mut self) {
        if let Some(anchor) = self.anchor.as_mut() {
            let _ = anchor.start_kill();
            reap_killed(anchor);
        }
    }
}

#[cfg(windows)]
struct Containment {
    job: std::os::windows::io::OwnedHandle,
}

#[cfg(windows)]
impl Containment {
    /// Nothing to let go of: the Job Object is held by handle until the tree is dropped.
    fn poll_let_go(&mut self, _context: &mut Context<'_>) -> Poll<()> {
        Poll::Ready(())
    }
}

#[cfg(windows)]
async fn spawn_contained(
    command: &mut Command,
    descendants: Descendants,
) -> io::Result<(Child, Containment)> {
    use std::{
        mem,
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
        ptr,
    };
    use windows_sys::Win32::System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject, TerminateJobObject,
        },
        Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED},
    };

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtResumeProcess(process_handle: windows_sys::Win32::Foundation::HANDLE) -> i32;
    }

    let job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }
    let job = unsafe { OwnedHandle::from_raw_handle(job) };
    if descendants == Descendants::EndWithRoot {
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                ptr::addr_of!(limits).cast(),
                mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            return Err(io::Error::last_os_error());
        }
    }

    // The root starts suspended so it is in the job before it can start anything outside it.
    // `CREATE_NO_WINDOW` keeps a console-subsystem process from allocating a console of its own:
    // Suru's server runs detached and so has no console to lend it, which would otherwise make
    // Windows pop a terminal window for every process launched.
    command.creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW);
    let mut child = command.spawn()?;
    let process_handle = child
        .raw_handle()
        .ok_or_else(|| io::Error::other("launched process had no process handle"))?;
    if unsafe { AssignProcessToJobObject(job.as_raw_handle(), process_handle) } == 0 {
        let error = io::Error::last_os_error();
        let _ = child.start_kill();
        return Err(error);
    }
    let resumed = unsafe { NtResumeProcess(process_handle) };
    if resumed < 0 {
        unsafe {
            TerminateJobObject(job.as_raw_handle(), 1);
        }
        return Err(io::Error::other(format!(
            "could not resume launched process: NTSTATUS {resumed:#x}"
        )));
    }

    Ok((child, Containment { job }))
}

#[cfg(windows)]
impl Root {
    fn kill_tree(&mut self) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;

        if unsafe { TerminateJobObject(self.containment.job.as_raw_handle(), 1) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(not(any(unix, windows)))]
struct Containment;

#[cfg(not(any(unix, windows)))]
async fn spawn_contained(
    command: &mut Command,
    _descendants: Descendants,
) -> io::Result<(Child, Containment)> {
    command.spawn().map(|child| (child, Containment))
}

#[cfg(not(any(unix, windows)))]
impl Root {
    fn kill_tree(&mut self) -> io::Result<()> {
        self.child.start_kill()
    }
}

#[cfg(not(any(unix, windows)))]
impl Containment {
    fn poll_let_go(&mut self, _context: &mut Context<'_>) -> Poll<()> {
        Poll::Ready(())
    }
}

/// A process tree for tests to abandon: a root that starts a descendant in the background, then
/// either exits or runs on — heedless of its stdin closing — until the fixture is dropped. The
/// descendant runs until then too, so a test that fails leaves nothing behind once its fixture
/// is dropped, whatever it failed to take down.
#[cfg(all(test, unix))]
pub(crate) mod test_support {
    use std::{ffi::OsString, time::Duration};

    /// How long a process the code under test was to have killed may take to be gone. Generous,
    /// since it bounds only a failure: a killed process is gone in milliseconds, and one left
    /// running is never gone.
    pub(crate) const ENDING_DEADLINE: Duration = Duration::from_secs(5);

    const RUN_ON: &str = r#"while [ -e "$TREE_DIR/keepalive" ]; do sleep 0.01; done"#;

    pub(crate) struct StubbornTree {
        directory: tempfile::TempDir,
        script: String,
    }

    impl StubbornTree {
        /// A root that runs on, ignoring its stdin, until the fixture is dropped.
        pub(crate) fn running() -> Self {
            Self::new("", RUN_ON)
        }

        /// A root that runs on as [`StubbornTree::running`] does, but that it and its
        /// descendant ignore SIGTERM from before the root records its descendant.
        pub(crate) fn running_heedless_of_sigterm() -> Self {
            Self::new("trap '' TERM", RUN_ON)
        }

        /// A root that exits as soon as it has started its descendant.
        pub(crate) fn exiting() -> Self {
            Self::new("", "exit 0")
        }

        fn new(root_head: &str, root_tail: &str) -> Self {
            let script = format!(
                r#"{root_head}
printf '%s\n' "$$" > "$TREE_DIR/root.tmp"
mv "$TREE_DIR/root.tmp" "$TREE_DIR/root"
( while [ -e "$TREE_DIR/keepalive" ]; do sleep 0.01; done ) &
printf '%s\n' "$!" > "$TREE_DIR/descendant.tmp"
mv "$TREE_DIR/descendant.tmp" "$TREE_DIR/descendant"
{root_tail}
"#
            );
            let directory = tempfile::tempdir().expect("create process tree fixture directory");
            std::fs::write(directory.path().join("keepalive"), b"")
                .expect("let the fixture's processes run");
            Self { directory, script }
        }

        /// The program the root runs, its arguments, and its environment.
        pub(crate) fn invocation(&self) -> (OsString, Vec<OsString>, Vec<(OsString, OsString)>) {
            (
                "sh".into(),
                vec!["-c".into(), self.script.clone().into()],
                vec![("TREE_DIR".into(), self.directory.path().into())],
            )
        }

        pub(crate) fn command(&self) -> tokio::process::Command {
            let (program, args, env) = self.invocation();
            let mut command = tokio::process::Command::new(program);
            command.args(args).envs(env);
            command
        }

        /// Whether the root has recorded its process ID, as it does first thing once it runs.
        pub(crate) fn has_recorded_root(&self) -> bool {
            self.directory.path().join("root").exists()
                || self.directory.path().join("root.tmp").exists()
        }

        /// The root's process ID, once it has recorded it.
        pub(crate) async fn root(&self) -> libc::pid_t {
            self.recorded("root").await
        }

        /// The descendant's process ID, once the root has recorded it.
        pub(crate) async fn descendant(&self) -> libc::pid_t {
            self.recorded("descendant").await
        }

        /// The process ID the root records under `name`, once it has.
        async fn recorded(&self, name: &str) -> libc::pid_t {
            let file = self.directory.path().join(name);
            tokio::time::timeout(ENDING_DEADLINE, async {
                loop {
                    if let Ok(pid) = std::fs::read_to_string(&file) {
                        return pid.trim().parse().expect("recorded PID is numeric");
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("the fixture's root records its {name}"))
        }

        /// Waits until the root, a child of this process, has exited — without reaping it, which
        /// is left to the code under test. A root the code under test has reaped already has
        /// exited too.
        pub(crate) async fn root_exited(&self) {
            let root = self.root().await;
            tokio::time::timeout(ENDING_DEADLINE, async {
                loop {
                    match has_exited(root) {
                        Ok(true) => return,
                        Err(error) if error.raw_os_error() == Some(libc::ECHILD) => return,
                        Ok(false) => tokio::time::sleep(Duration::from_millis(5)).await,
                        Err(error) => panic!("look at the fixture's root: {error}"),
                    }
                }
            })
            .await
            .expect("the fixture's root exits");
        }
    }

    /// Whether the child `pid` names has exited, leaving it unreaped for whoever waits on it next.
    fn has_exited(pid: libc::pid_t) -> std::io::Result<bool> {
        loop {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let checked = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if checked == 0 {
                // With `WNOHANG` a child still running leaves `info` as it was: zeroed.
                return Ok(unsafe { info.si_pid() } != 0);
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    /// Every process the fixture starts runs only while its keepalive exists, so taking the
    /// keepalive away — here, and again as the directory holding it is removed — ends whatever
    /// the code under test failed to, however long it takes to look again.
    impl Drop for StubbornTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.directory.path().join("keepalive"));
        }
    }

    /// A stand-in for an anchor's shell that never says it is ready: an executable that ignores
    /// what it is asked to run and runs `body` instead, recording its process ID first. It runs
    /// on only while the fixture is held.
    pub(crate) struct SilentShell {
        directory: tempfile::TempDir,
    }

    impl SilentShell {
        pub(crate) fn new(body: &str) -> Self {
            use std::os::unix::fs::PermissionsExt;

            let directory = tempfile::tempdir().expect("create silent shell directory");
            let path = directory.path().join("sh");
            std::fs::write(
                &path,
                format!("#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$0.pid\"\n{body}\n"),
            )
            .expect("write the silent shell");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("make the silent shell executable");
            std::fs::write(directory.path().join("sh.keepalive"), b"")
                .expect("let the silent shell run");
            Self { directory }
        }

        pub(crate) fn path(&self) -> String {
            self.directory
                .path()
                .join("sh")
                .to_str()
                .expect("temporary paths are UTF-8")
                .to_owned()
        }

        /// The process ID it records before anything else, once it has.
        pub(crate) async fn recorded_pid(&self) -> libc::pid_t {
            tokio::time::timeout(ENDING_DEADLINE, async {
                loop {
                    if let Some(pid) = self.pid() {
                        return pid;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("the silent shell records its PID")
        }

        /// The process ID it recorded, as it does before anything else — unless it was killed
        /// before it could.
        pub(crate) fn pid(&self) -> Option<libc::pid_t> {
            let recorded = std::fs::read_to_string(self.directory.path().join("sh.pid")).ok()?;
            // Read part written, it is not recorded yet.
            recorded.strip_suffix('\n')?.parse().ok()
        }
    }

    impl Drop for SilentShell {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.directory.path().join("sh.keepalive"));
        }
    }

    /// Whether a process `pid` names is still running, or is still a zombie nobody has reaped.
    pub(crate) fn is_running(pid: libc::pid_t) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// Waits for `pid` to be gone — killed, and reaped by whichever process it was left to — and
    /// fails if it is still there at the deadline. The runtime keeps running meanwhile, so a
    /// process this one reaps can be.
    pub(crate) async fn assert_ended(pid: libc::pid_t) {
        let ended = tokio::time::timeout(ENDING_DEADLINE, async {
            while is_running(pid) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        assert!(ended.is_ok(), "process {pid} was left running");
    }

    /// [`assert_ended`], for a test with no runtime left to wait on.
    pub(crate) fn assert_ended_blocking(pid: libc::pid_t) {
        let started = std::time::Instant::now();
        while is_running(pid) {
            assert!(
                started.elapsed() < ENDING_DEADLINE,
                "process {pid} was left running"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{os::unix::process::ExitStatusExt, process::Stdio};

    use super::{
        Descendants, ProcessTree, anchor_shell, output,
        test_support::{
            ENDING_DEADLINE, SilentShell, StubbornTree, assert_ended, assert_ended_blocking,
            is_running,
        },
    };

    #[tokio::test]
    async fn dropping_a_tree_takes_down_everything_its_root_started() {
        let fixture = StubbornTree::running();
        let (tree, _pipes) = ProcessTree::spawn(&mut fixture.command(), Descendants::EndWithRoot)
            .await
            .expect("launch the tree");
        let root = tree.id().expect("the root is running") as libc::pid_t;
        let anchor = tree.anchor_id().expect("the tree is anchored");
        let descendant = fixture.descendant().await;

        drop(tree);

        assert_ended(descendant).await;
        assert_ended(root).await;
        assert_ended(anchor).await;
    }

    #[tokio::test]
    async fn a_root_exiting_takes_down_what_it_left_running_when_its_descendants_end_with_it() {
        let fixture = StubbornTree::exiting();
        let (tree, _pipes) = ProcessTree::spawn(&mut fixture.command(), Descendants::EndWithRoot)
            .await
            .expect("launch the tree");
        let anchor = tree.anchor_id().expect("the tree is anchored");

        let status = tree.wait().await.expect("wait for the root");

        assert!(status.success());
        assert_ended(fixture.descendant().await).await;
        assert_ended(anchor).await;
    }

    #[tokio::test]
    async fn a_root_exiting_leaves_what_it_started_running_when_its_descendants_may_outlive_it() {
        let fixture = StubbornTree::exiting();
        let (tree, _pipes) =
            ProcessTree::spawn(&mut fixture.command(), Descendants::MayOutliveRoot)
                .await
                .expect("launch the tree");
        let anchor = tree.anchor_id().expect("the tree is anchored");

        tree.wait().await.expect("wait for the root");
        drop(tree);

        // Let go of, the anchor is gone, leaving what it led be.
        assert_ended(anchor).await;
        let descendant = fixture.descendant().await;
        assert!(
            is_running(descendant),
            "what a root that has exited left running is left be"
        );
    }

    /// The process holding a tree ending, however it ends, closes its end of the lifeline the
    /// tree's anchor reads; here the test holds that end in its place and drops it.
    #[tokio::test]
    async fn a_tree_is_taken_down_whole_once_the_process_holding_it_is_gone() {
        let fixture = StubbornTree::running();
        let (lifeline, held) = std::io::pipe().expect("create a lifeline");
        let (tree, _pipes) = ProcessTree::spawn_anchored_by(
            &mut fixture.command(),
            Descendants::EndWithRoot,
            anchor_shell(),
            lifeline,
            ENDING_DEADLINE,
        )
        .await
        .expect("launch the tree");
        let anchor = tree.anchor_id().expect("the tree is anchored");
        let descendant = fixture.descendant().await;

        drop(held);

        let status = tokio::time::timeout(ENDING_DEADLINE, tree.wait())
            .await
            .expect("the anchor takes the tree down")
            .expect("wait for the root");
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert_ended(descendant).await;
        assert_ended(anchor).await;
    }

    /// A signal sent to the whole group — such as a tool tidying up with `kill 0` — reaches the
    /// programs in it, which here ignore it, and leaves the anchor to take them down with Suru.
    /// It is sent by the root at the earliest moment it can be, before the root's own program
    /// has even begun: the anchor has said it is ready by then.
    #[tokio::test]
    async fn a_tree_stays_anchored_through_a_signal_its_root_sends_its_whole_group_at_once() {
        let fixture = StubbornTree::running_heedless_of_sigterm();
        let mut command = fixture.command();
        // Runs in the root once it has joined the anchor's group, before its program does.
        unsafe {
            command.pre_exec(|| {
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
                if libc::kill(0, libc::SIGTERM) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let (lifeline, held) = std::io::pipe().expect("create a lifeline");
        let (tree, _pipes) = ProcessTree::spawn_anchored_by(
            &mut command,
            Descendants::EndWithRoot,
            anchor_shell(),
            lifeline,
            ENDING_DEADLINE,
        )
        .await
        .expect("launch the tree");
        let descendant = fixture.descendant().await;

        drop(held);

        let status = tokio::time::timeout(ENDING_DEADLINE, tree.wait())
            .await
            .expect("the anchor outlasts the signal to take the tree down")
            .expect("wait for the root");
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert_ended(descendant).await;
    }

    /// A tree that could not be tied to Suru's life is not launched at all, rather than launched
    /// to outlive it; and the failure does not pass for the root's program being missing.
    #[tokio::test]
    async fn a_tree_whose_anchor_cannot_launch_launches_nothing() {
        let fixture = StubbornTree::running();
        let (lifeline, _held) = std::io::pipe().expect("create a lifeline");

        let error = ProcessTree::spawn_anchored_by(
            &mut fixture.command(),
            Descendants::EndWithRoot,
            "/nonexistent/sh",
            lifeline,
            ENDING_DEADLINE,
        )
        .await
        .err()
        .expect("the tree does not launch");

        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert!(error.to_string().contains("anchor"), "{error}");
        assert!(!fixture.has_recorded_root(), "the root never ran");
    }

    /// A launch abandoned while its anchor has yet to say it is ready — as one awaited at a
    /// deadline is — kills and reaps the anchor as it is dropped, even with no runtime left
    /// running after it, and never launches the root.
    #[test]
    fn a_launch_abandoned_while_its_anchor_readies_takes_the_anchor_down() {
        let fixture = StubbornTree::running();
        let silent = SilentShell::new(r#"while [ -e "$0.keepalive" ]; do sleep 0.01; done"#);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build a runtime");
        let anchor = runtime.block_on(async {
            let (lifeline, _held) = std::io::pipe().expect("create a lifeline");
            let mut command = fixture.command();
            let shell = silent.path();
            tokio::select! {
                _ = ProcessTree::spawn_anchored_by(
                    &mut command,
                    Descendants::EndWithRoot,
                    &shell,
                    lifeline,
                    ENDING_DEADLINE,
                ) => panic!("the anchor never says it is ready"),
                anchor = silent.recorded_pid() => anchor,
            }
        });

        drop(runtime);

        // Gone, and reaped: a zombie would still answer.
        assert_ended_blocking(anchor);
        assert!(!fixture.has_recorded_root(), "the root never ran");
    }

    /// A wait abandoned while it lets go of the tree — its root reaped, its group killed, its
    /// anchor killed but not yet reaped — leaves the anchor to the tree, which reaps it as it is
    /// dropped, even with no runtime left running.
    ///
    /// A killed anchor is gone in moments, so the wait is caught at that point by polling it
    /// step by step, and the whole is tried afresh should the anchor be gone by the first look.
    #[test]
    fn a_wait_abandoned_while_letting_go_of_the_tree_leaves_nothing_behind() {
        /// How many times the wait is tried before the test gives up on catching it letting go.
        const ATTEMPTS: usize = 50;

        for _ in 0..ATTEMPTS {
            let fixture = StubbornTree::exiting();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build a runtime");
            let caught = runtime.block_on(async {
                let (tree, _pipes) =
                    ProcessTree::spawn(&mut fixture.command(), Descendants::EndWithRoot)
                        .await
                        .expect("launch the tree");
                let anchor = tree.anchor_id().expect("the tree is anchored");
                let descendant = fixture.descendant().await;
                fixture.root_exited().await;
                let caught = {
                    let mut wait = std::pin::pin!(tree.wait());
                    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
                    loop {
                        if wait.as_mut().poll(&mut context).is_ready() {
                            break false;
                        }
                        if tree.is_letting_go() {
                            break true;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    }
                    // The wait is dropped here, part way through letting go.
                };
                caught.then_some((tree, anchor, descendant))
            });

            drop(runtime);

            let Some((tree, anchor, descendant)) = caught else {
                continue;
            };
            drop(tree);
            // Gone, and reaped: a zombie would still answer.
            assert_ended_blocking(anchor);
            assert_ended_blocking(descendant);
            return;
        }
        panic!("no wait was caught letting go of its tree in {ATTEMPTS} attempts");
    }

    /// An anchor that never says it is ready — here a stand-in for the shell that runs on
    /// without a word — is killed and reaped at the deadline, and the root is never launched.
    #[tokio::test]
    async fn a_tree_whose_anchor_does_not_say_it_is_ready_launches_nothing() {
        let fixture = StubbornTree::running();
        let silent = SilentShell::new(r#"while [ -e "$0.keepalive" ]; do sleep 0.01; done"#);
        let (lifeline, _held) = std::io::pipe().expect("create a lifeline");

        let error = ProcessTree::spawn_anchored_by(
            &mut fixture.command(),
            Descendants::EndWithRoot,
            &silent.path(),
            lifeline,
            std::time::Duration::from_millis(50),
        )
        .await
        .err()
        .expect("the tree does not launch");

        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert!(error.to_string().contains("ready"), "{error}");
        assert!(!fixture.has_recorded_root(), "the root never ran");
        if let Some(anchor) = silent.pid() {
            assert_ended(anchor).await;
        }
    }

    /// An anchor that ends without saying it is ready fails the launch at once.
    #[tokio::test]
    async fn a_tree_whose_anchor_ends_without_saying_it_is_ready_launches_nothing() {
        let fixture = StubbornTree::running();
        let silent = SilentShell::new("exit 0");
        let (lifeline, _held) = std::io::pipe().expect("create a lifeline");

        let error = ProcessTree::spawn_anchored_by(
            &mut fixture.command(),
            Descendants::EndWithRoot,
            &silent.path(),
            lifeline,
            ENDING_DEADLINE,
        )
        .await
        .err()
        .expect("the tree does not launch");

        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert!(error.to_string().contains("ready"), "{error}");
        assert!(!fixture.has_recorded_root(), "the root never ran");
        if let Some(anchor) = silent.pid() {
            assert_ended(anchor).await;
        }
    }

    /// A runtime may stop as soon as a wait it ran is done, leaving nothing to reap what the
    /// wait handed back; the wait reaps the anchor itself before it is done.
    #[test]
    fn a_waited_tree_leaves_no_anchor_for_a_stopped_runtime_to_reap() {
        for descendants in [Descendants::EndWithRoot, Descendants::MayOutliveRoot] {
            let fixture = StubbornTree::exiting();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build a runtime");
            let anchor = runtime.block_on(async {
                let (tree, _pipes) = ProcessTree::spawn(&mut fixture.command(), descendants)
                    .await
                    .expect("launch the tree");
                let anchor = tree.anchor_id().expect("the tree is anchored");
                tree.wait().await.expect("wait for the root");
                anchor
            });

            drop(runtime);

            assert_ended_blocking(anchor);
        }
    }

    /// As [`a_waited_tree_leaves_no_anchor_for_a_stopped_runtime_to_reap`], for a command whose
    /// output is collected.
    #[test]
    fn collected_output_leaves_no_anchor_for_a_stopped_runtime_to_reap() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build a runtime");
        let pid_file = tempfile::NamedTempFile::new().expect("create a PID file");
        runtime.block_on(async {
            // The command records its group, which is its anchor's process ID.
            let mut command = tokio::process::Command::new("sh");
            command
                .args(["-c", r#"ps -o pgid= -p $$ > "$1""#, "sh"])
                .arg(pid_file.path())
                .stdin(Stdio::null());
            output(&mut command).await.expect("run the command");
        });

        drop(runtime);

        let anchor = std::fs::read_to_string(pid_file.path())
            .expect("read the recorded group")
            .trim()
            .parse()
            .expect("the recorded group is numeric");
        assert_ended_blocking(anchor);
    }

    #[tokio::test]
    async fn a_tree_reports_the_code_its_root_exited_with() {
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "exit 3"]);
        let (tree, _pipes) = ProcessTree::spawn(&mut command, Descendants::EndWithRoot)
            .await
            .expect("launch the tree");

        let status = tree.wait().await.expect("wait for the root");

        assert_eq!(status.code(), Some(3));
    }

    #[tokio::test]
    async fn a_tree_reports_the_signal_that_ended_its_root() {
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "kill -s TERM $$"]);
        let (tree, _pipes) = ProcessTree::spawn(&mut command, Descendants::EndWithRoot)
            .await
            .expect("launch the tree");

        let status = tree.wait().await.expect("wait for the root");

        assert_eq!(status.signal(), Some(libc::SIGTERM));
        assert_eq!(status.code(), None);
    }

    #[tokio::test]
    async fn output_abandoned_at_its_deadline_takes_down_the_command_and_what_it_started() {
        let fixture = StubbornTree::running();
        let mut command = fixture.command();
        command.stdin(Stdio::null());

        // Abandoned once its descendant has started, as a deadline would abandon it.
        let descendant = tokio::select! {
            _ = output(&mut command) => panic!("the command runs until it is abandoned"),
            descendant = fixture.descendant() => descendant,
        };

        assert_ended(descendant).await;
    }

    /// A helper the command leaves behind, still holding the command's output open, keeps the
    /// collection waiting past the command's own exit; abandoning it then must still reach the
    /// helper, which it can only while the tree has not been let go of — as waiting on the
    /// command would.
    #[tokio::test]
    async fn output_abandoned_after_the_command_exits_takes_down_a_helper_holding_its_output() {
        let fixture = StubbornTree::exiting();
        let mut command = fixture.command();
        command.stdin(Stdio::null());

        let abandoned = tokio::select! {
            _ = output(&mut command) => false,
            // Abandoned only once the command has exited, leaving the helper holding its output,
            // and after a moment more of collecting it, in which a collection that waits on the
            // command early would hear of its exit and do so.
            () = async {
                fixture.root_exited().await;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            } => true,
        };

        assert!(abandoned, "the helper holds the command's output open");
        assert_ended(fixture.descendant().await).await;
    }

    #[tokio::test]
    async fn output_collects_what_the_command_wrote_and_how_it_exited() {
        let mut command = tokio::process::Command::new("sh");
        command
            .args(["-c", "printf out; printf err >&2; exit 3"])
            .stdin(Stdio::null());

        let output = output(&mut command).await.expect("run the command");

        assert_eq!(output.status.code(), Some(3));
        assert_eq!(output.stdout, b"out");
        assert_eq!(output.stderr, b"err");
    }

    #[tokio::test]
    async fn output_collects_what_the_command_wrote_before_a_signal_ended_it() {
        let mut command = tokio::process::Command::new("sh");
        command
            .args(["-c", "printf out; printf err >&2; kill -s TERM $$"])
            .stdin(Stdio::null());

        let output = output(&mut command).await.expect("run the command");

        assert_eq!(output.status.signal(), Some(libc::SIGTERM));
        assert_eq!(output.stdout, b"out");
        assert_eq!(output.stderr, b"err");
    }
}
