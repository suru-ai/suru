//! A child process held together with every process it starts.
//!
//! Suru launches processes that launch processes of their own: a Provider's harness runs tool
//! shells and MCP servers, and Git runs hooks and remote helpers. Signalling the child alone leaves
//! those descendants running as orphans of init, so each such child is launched as the root of a
//! [`ProcessTree`] that contains its descendants too — a process group of its own on Unix, a Job
//! Object on Windows — and is taken down whole.
//!
//! A tree is taken down when it is terminated, and when it is dropped while its root still runs,
//! however that comes about: the task waiting on it cancelled at a deadline, the runtime that task
//! ran on shut down, or a panic unwinding past it. A tree whose root exits on its own takes down
//! what the root left running or leaves it be, as the [`Descendants`] it was launched with say.

use std::{
    io,
    process::{ExitStatus, Output, Stdio},
    sync::{Arc, Mutex as StdMutex, MutexGuard, PoisonError, Weak},
};

use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

/// What becomes of the processes a tree's root leaves running when it exits on its own. A tree
/// abandoned while its root still runs is taken down whole either way.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Descendants {
    /// They end with it. A harness's tool shells and MCP servers serve nothing but the harness,
    /// and on Windows they end with it even when Suru itself is ended outright, since the kernel
    /// closing the Job Object takes them down.
    EndWithRoot,
    /// They are left to run. A Git hook may start work in the background meant to outlive the
    /// command that ran it, such as regenerating tags after a checkout.
    MayOutliveRoot,
}

/// A launched child process and the descendants it starts, taken down together.
///
/// The tree is the one owner of its root, so nothing else can wait on the root and free its
/// process ID behind the tree's back; see [`Root::terminate`] for why that matters. Whoever must
/// be able to take the tree down without owning it holds a [`ProcessTreeTerminator`].
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

/// The root process and what contains its descendants, locked as one so that reaping the root and
/// signalling its tree never interleave.
struct Root {
    child: Child,
    containment: Containment,
    descendants: Descendants,
}

impl ProcessTree {
    /// Launches `command` as the root of a tree of its own, its pipes handed back beside it.
    pub(crate) fn spawn(
        command: &mut Command,
        descendants: Descendants,
    ) -> io::Result<(Self, ProcessPipes)> {
        // The root's own handle signals it alone, should the tree somehow fail to; the tree's
        // drop signals everything the root started before this does.
        command.kill_on_drop(true);
        let (mut child, containment) = spawn_contained(command, descendants)?;
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
        Ok((
            Self {
                root: Arc::new(StdMutex::new(root)),
            },
            pipes,
        ))
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

    /// Waits for the root to exit and reaps it, first taking down whatever it left running in
    /// the tree when its descendants [`Descendants::EndWithRoot`].
    pub(crate) async fn wait(&self) -> io::Result<ExitStatus> {
        wait_for_root(&self.root).await
    }
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
    let (tree, pipes) = ProcessTree::spawn(command, Descendants::MayOutliveRoot)?;
    drop(pipes.stdin);
    async fn read_to_end(pipe: Option<impl tokio::io::AsyncRead + Unpin>) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            pipe.read_to_end(&mut bytes).await?;
        }
        Ok(bytes)
    }
    // The output is collected before the root is waited on, not beside it. Something the command
    // left running may hold its output open past its exit; reaping the root then would free the
    // group's ID while the collection still waits, and a deadline abandoning it could no longer
    // safely take that straggler down. Unreaped, the root keeps the whole tree abandonable until
    // the output is in.
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
    /// On Unix this is what keeps the tree from ever signalling a stranger. The tree is a process
    /// group whose ID is the root's process ID, and the system may hand that ID out again — as a
    /// process ID, and so as the ID of a new group — once the root has been reaped and nothing is
    /// left in its group. Until it is reaped the root holds the ID, running or as a zombie, so
    /// the group it names can only be this tree's. Only the [`ProcessTree`] waits on the root, and
    /// it does so under the same lock as this, so the check below cannot go stale before the
    /// signal is sent.
    ///
    /// On Windows the Job Object is held by handle and cannot be confused with another, so the
    /// check only keeps a tree whose descendants [`Descendants::MayOutliveRoot`] from taking down
    /// what its root left running once the root has exited.
    fn terminate(&mut self) -> io::Result<()> {
        if self.child.id().is_none() {
            return Ok(());
        }
        self.kill_tree()
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        if self.child.id().is_none() {
            return;
        }
        if let Err(error) = self.kill_tree() {
            tracing::debug!(%error, "could not take down an abandoned process tree");
        }
        self.reap_killed();
    }
}

/// How long a dropped tree waits, at most, for its killed root to be gone so it can reap the root
/// itself, looking again each step. A killed process is gone in well under a millisecond, so
/// this is a bound, not a wait: it only keeps a root the kernel is slow to let go of from
/// blocking whoever dropped the tree.
const KILLED_ROOT_REAP_BOUND: std::time::Duration = std::time::Duration::from_millis(100);
const KILLED_ROOT_REAP_STEP: std::time::Duration = std::time::Duration::from_millis(1);

impl Root {
    /// Reaps the root just killed, rather than leaving it to Tokio, which reaps a child it has
    /// been handed back only once some runtime's driver next runs — never, where the tree was
    /// dropped with the last runtime, leaving the root a zombie for the life of Suru. A root
    /// still not gone at the bound is left to Tokio after all.
    fn reap_killed(&mut self) {
        let deadline = std::time::Instant::now() + KILLED_ROOT_REAP_BOUND;
        while let Ok(None) = self.child.try_wait() {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return;
            }
            std::thread::sleep(KILLED_ROOT_REAP_STEP.min(left));
        }
    }
}

#[cfg(unix)]
struct Containment {
    process_group_id: libc::pid_t,
}

#[cfg(unix)]
fn spawn_contained(
    command: &mut Command,
    _descendants: Descendants,
) -> io::Result<(Child, Containment)> {
    command.process_group(0);
    let child = command.spawn()?;
    let process_group_id = child
        .id()
        .and_then(|id| libc::pid_t::try_from(id).ok())
        .filter(|id| *id > 0)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "launched process had no process group ID",
            )
        })?;
    Ok((child, Containment { process_group_id }))
}

#[cfg(unix)]
impl Root {
    /// Kills every process in the tree's group. Sound only while the root is unreaped; see
    /// [`Root::terminate`].
    fn kill_tree(&mut self) -> io::Result<()> {
        let killed = unsafe { libc::killpg(self.containment.process_group_id, libc::SIGKILL) };
        let error = (killed == -1).then(io::Error::last_os_error);
        // The root is signalled directly as well, in case it left its group for a group or
        // session of its own. Unreaped, its ID is still its own, and signalling one that has
        // already exited is harmless.
        let _ = self.child.start_kill();
        match error {
            // The group is empty: everything in it has exited, the root included — or the root
            // left it and has just been signalled where it went.
            Some(error) if error.raw_os_error() != Some(libc::ESRCH) => Err(error),
            _ => Ok(()),
        }
    }

    /// Reaps the root if it has exited, taking down first whatever it left running in its group
    /// when its descendants end with it — while the unreaped root still holds the group's ID.
    fn try_reap(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.child.id().is_some() {
            if !has_exited(self.containment.process_group_id)? {
                return Ok(None);
            }
            if self.descendants == Descendants::EndWithRoot {
                let _ = self.kill_tree();
            }
        }
        self.child.try_wait()
    }
}

/// Whether the child `pid` names has exited, leaving it unreaped for whoever waits on it next.
#[cfg(unix)]
fn has_exited(pid: libc::pid_t) -> io::Result<bool> {
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
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Waits for the root to exit without letting Tokio reap it first, so what it left running can
/// be taken down while its group's ID is still the tree's: every child's exit raises SIGCHLD,
/// and each one is answered by looking — without reaping — whether it was the root's.
#[cfg(unix)]
async fn wait_for_root(root: &StdMutex<Root>) -> io::Result<ExitStatus> {
    use tokio::signal::unix::{SignalKind, signal};

    // Listening begins before the first look, so an exit between the two is still heard.
    let mut child_exits = signal(SignalKind::child())?;
    loop {
        if let Some(status) = lock(root).try_reap()? {
            return Ok(status);
        }
        if child_exits.recv().await.is_none() {
            return Err(io::Error::other(
                "stopped hearing of child processes exiting",
            ));
        }
    }
}

#[cfg(windows)]
struct Containment {
    job: std::os::windows::io::OwnedHandle,
}

#[cfg(windows)]
fn spawn_contained(
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
fn spawn_contained(
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

/// Waits for the root to exit and reaps it, then takes down whatever it left running in the tree
/// when its descendants end with it. Nothing here can name another process's tree, so there is
/// no need to act before the root is reaped.
#[cfg(not(unix))]
async fn wait_for_root(root: &StdMutex<Root>) -> io::Result<ExitStatus> {
    std::future::poll_fn(|context| {
        let mut root = lock(root);
        // Each poll waits afresh: Tokio keeps what a wait has registered in the child itself.
        let polled = std::pin::pin!(root.child.wait()).poll(context);
        if polled.is_ready() && root.descendants == Descendants::EndWithRoot {
            let _ = root.kill_tree();
        }
        polled
    })
    .await
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
    const ENDING_DEADLINE: Duration = Duration::from_secs(5);

    pub(crate) struct StubbornTree {
        directory: tempfile::TempDir,
        script: String,
    }

    impl StubbornTree {
        /// A root that runs on, ignoring its stdin, until the fixture is dropped.
        pub(crate) fn running() -> Self {
            Self::new(r#"while [ -e "$TREE_DIR/keepalive" ]; do sleep 0.01; done"#)
        }

        /// A root that exits as soon as it has started its descendant.
        pub(crate) fn exiting() -> Self {
            Self::new("exit 0")
        }

        fn new(root_tail: &str) -> Self {
            let script = format!(
                r#"printf '%s\n' "$$" > "$TREE_DIR/root.tmp"
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
                    match super::has_exited(root) {
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

    /// Every process the fixture starts runs only while its keepalive exists, so taking the
    /// keepalive away — here, and again as the directory holding it is removed — ends whatever
    /// the code under test failed to, however long it takes to look again.
    impl Drop for StubbornTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.directory.path().join("keepalive"));
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
    use super::{
        Descendants, ProcessTree, output,
        test_support::{StubbornTree, assert_ended, is_running},
    };

    #[tokio::test]
    async fn dropping_a_tree_takes_down_everything_its_root_started() {
        let fixture = StubbornTree::running();
        let (tree, _pipes) = ProcessTree::spawn(&mut fixture.command(), Descendants::EndWithRoot)
            .expect("launch the tree");
        let root = tree.id().expect("the root is running") as libc::pid_t;
        let descendant = fixture.descendant().await;

        drop(tree);

        assert_ended(descendant).await;
        assert_ended(root).await;
    }

    #[tokio::test]
    async fn a_root_exiting_takes_down_what_it_left_running_when_its_descendants_end_with_it() {
        let fixture = StubbornTree::exiting();
        let (tree, _pipes) = ProcessTree::spawn(&mut fixture.command(), Descendants::EndWithRoot)
            .expect("launch the tree");

        let status = tree.wait().await.expect("wait for the root");

        assert!(status.success());
        assert_ended(fixture.descendant().await).await;
    }

    #[tokio::test]
    async fn a_root_exiting_leaves_what_it_started_running_when_its_descendants_may_outlive_it() {
        let fixture = StubbornTree::exiting();
        let (tree, _pipes) =
            ProcessTree::spawn(&mut fixture.command(), Descendants::MayOutliveRoot)
                .expect("launch the tree");

        tree.wait().await.expect("wait for the root");
        drop(tree);

        let descendant = fixture.descendant().await;
        assert!(
            is_running(descendant),
            "what a root that has exited left running is left be"
        );
    }

    #[tokio::test]
    async fn output_abandoned_at_its_deadline_takes_down_the_command_and_what_it_started() {
        let fixture = StubbornTree::running();
        let mut command = fixture.command();
        command.stdin(std::process::Stdio::null());

        // Abandoned once its descendant has started, as a deadline would abandon it.
        let descendant = tokio::select! {
            _ = output(&mut command) => panic!("the command runs until it is abandoned"),
            descendant = fixture.descendant() => descendant,
        };

        assert_ended(descendant).await;
    }

    /// A helper the command leaves behind, still holding the command's output open, keeps the
    /// collection waiting past the command's own exit; abandoning it then must still reach the
    /// helper, which it can only while the command is left unreaped.
    #[tokio::test]
    async fn output_abandoned_after_the_command_exits_takes_down_a_helper_holding_its_output() {
        let fixture = StubbornTree::exiting();
        let mut command = fixture.command();
        command.stdin(std::process::Stdio::null());

        let abandoned = tokio::select! {
            _ = output(&mut command) => false,
            // Abandoned only once the command has exited, leaving the helper holding its output,
            // and after a moment more of collecting it, in which a collection that reaps the
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
            .stdin(std::process::Stdio::null());

        let output = output(&mut command).await.expect("run the command");

        assert_eq!(output.status.code(), Some(3));
        assert_eq!(output.stdout, b"out");
        assert_eq!(output.stderr, b"err");
    }
}
