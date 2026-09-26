//! Ownership of the harness server child processes Suru launches.
//!
//! Every launched process is registered with a [`ProcessRegistry`] and supervised by a task that
//! outlives its transport, so a Session shutdown, a runtime shutdown, and an unexpected exit all
//! converge on the same terminated process tree and the same reported failure. The machinery is
//! Provider-neutral: a [`HarnessSpec`] names the executable to launch and how the harness is
//! called in the Log and in failures, and a [`HarnessLink`] is the supervisor's handle into
//! whatever transport the Provider runs over the process's stdio.

use std::{
    collections::HashMap,
    ffi::OsString,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicU64, Ordering},
    },
};

use tokio::{
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::watch,
    time::{Duration, timeout},
};

use crate::{
    protocol::ProviderUnavailability,
    provider::{ProviderError, wait_for_shutdown},
};

/// How long a harness may take to exit on its own after its stdin closes before it is forced
/// down. A CLI starting in a directory it has never seen (a fresh Worktree) can spend seconds
/// on first-run setup before it notices the closed pipe; killing it that early turns routine
/// cleanup into spurious failures.
const PROCESS_EXIT_GRACE_PERIOD: Duration = Duration::from_secs(5);
const PROCESS_KILL_TIMEOUT: Duration = Duration::from_millis(500);

/// The harness server process a Provider runtime launches: its executable, the arguments that put
/// it in server mode, and the name the Log and failures call it, such as `Codex app-server`.
#[derive(Clone, Debug)]
pub(crate) struct HarnessSpec {
    pub(crate) executable: OsString,
    pub(crate) args: Vec<OsString>,
    pub(crate) name: String,
    /// The directory the process starts in, for a harness that takes its working
    /// directory from the process rather than over its wire. `None` inherits Suru's own.
    pub(crate) cwd: Option<std::path::PathBuf>,
}

/// The supervisor's handle into the transport running over the process it owns.
///
/// The supervisor decides when the connection ends; this is everything it needs to say so.
pub(crate) trait HarnessLink: Send + Sync + 'static {
    /// Fails the in-flight requests without publishing a Provider failure, for an intended stop.
    fn close(&self);

    /// Closes the harness's stdin so it can exit on its own.
    fn close_stdin(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;

    /// Fails the in-flight requests and publishes `error` as a lost Provider Session.
    fn terminate(&self, error: ProviderError);
}

/// Tracks every live harness process one runtime launched so a runtime shutdown can stop them all.
#[derive(Clone, Debug)]
pub(crate) struct ProcessRegistry {
    name: Arc<str>,
    next_id: Arc<AtomicU64>,
    exit_grace: Duration,
    state: Arc<StdMutex<ProcessRegistryState>>,
}

#[derive(Debug)]
struct ProcessRegistryState {
    shutting_down: bool,
    processes: HashMap<u64, ProcessControl>,
}

impl ProcessRegistry {
    pub(crate) fn new(name: impl Into<String>) -> Self {
        Self {
            name: Arc::from(name.into()),
            next_id: Arc::new(AtomicU64::new(1)),
            exit_grace: PROCESS_EXIT_GRACE_PERIOD,
            state: Arc::new(StdMutex::new(ProcessRegistryState {
                shutting_down: false,
                processes: HashMap::new(),
            })),
        }
    }

    /// Overrides how long a stopping process may exit gracefully before it is
    /// forced down. Takes effect for processes launched after the call.
    pub(crate) fn set_exit_grace(&mut self, exit_grace: Duration) {
        self.exit_grace = exit_grace;
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn is_shutting_down(&self) -> bool {
        self.state
            .lock()
            .expect("harness process registry lock is not poisoned")
            .shutting_down
    }

    /// The refusal every demand and registration meets once shutdown has begun.
    pub(crate) fn refuse_if_shutting_down(&self) -> Result<(), ProviderError> {
        if self.is_shutting_down() {
            return Err(self.shutting_down_error());
        }
        Ok(())
    }

    fn shutting_down_error(&self) -> ProviderError {
        ProviderError::new(format!("{} is shutting down", self.name))
    }

    fn register(&self, process: ProcessControl) -> Result<u64, ProviderError> {
        let mut state = self
            .state
            .lock()
            .expect("harness process registry lock is not poisoned");
        if state.shutting_down {
            return Err(self.shutting_down_error());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        state.processes.insert(id, process);
        Ok(id)
    }

    fn remove(&self, id: u64) {
        self.state
            .lock()
            .expect("harness process registry lock is not poisoned")
            .processes
            .remove(&id);
    }

    pub(crate) async fn shutdown(&self) -> Result<(), ProviderError> {
        let processes = {
            let mut state = self
                .state
                .lock()
                .expect("harness process registry lock is not poisoned");
            state.shutting_down = true;
            state.processes.values().cloned().collect::<Vec<_>>()
        };
        for process in &processes {
            process.begin_shutdown();
        }
        let mut first_error = None;
        for process in processes {
            if let Err(error) = process.wait_until_stopped().await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

#[derive(Clone, Debug)]
struct ProcessControl {
    name: Arc<str>,
    shutdown: watch::Sender<bool>,
    stopped: watch::Receiver<bool>,
    /// The exit grace, forced-kill wait, and margin a stop may take in total.
    wait_budget: Duration,
}

impl ProcessControl {
    fn begin_shutdown(&self) {
        self.shutdown.send_replace(true);
    }

    async fn wait_until_stopped(&self) -> Result<(), ProviderError> {
        let mut stopped = self.stopped.clone();
        if *stopped.borrow() {
            return Ok(());
        }
        let name = self.name.clone();
        let wait = async move {
            stopped
                .wait_for(|stopped| *stopped)
                .await
                .map(|_| ())
                .map_err(|_| {
                    ProviderError::new(format!("{name} process supervisor stopped unexpectedly"))
                })
        };
        timeout(self.wait_budget, wait).await.map_err(|_| {
            ProviderError::new(format!(
                "{} did not stop within the shutdown deadline",
                self.name
            ))
        })?
    }
}

/// Keeps one supervised process alive; dropping it asks that process to stop.
pub(crate) struct ProcessGuard {
    control: ProcessControl,
}

impl ProcessGuard {
    pub(crate) fn begin_shutdown(&self) {
        self.control.begin_shutdown();
    }

    pub(crate) async fn wait_until_stopped(&self) -> Result<(), ProviderError> {
        self.control.wait_until_stopped().await
    }

    /// The longest a stop of this process may take — its exit grace, the forced kill after it,
    /// and a margin — which is what [`Self::wait_until_stopped`] waits at most. A caller waiting
    /// on something the process's end should bring about bounds that wait by the same budget.
    pub(crate) fn wait_budget(&self) -> Duration {
        self.control.wait_budget
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        self.begin_shutdown();
    }
}

/// A launched harness server that nothing supervises yet.
pub(crate) struct SpawnedProcess {
    name: Arc<str>,
    child: Child,
    process_tree: ProcessTree,
}

/// The launched harness server's piped stdio, ready for a transport to speak over.
pub(crate) struct ProcessStdio {
    pub(crate) stdin: ChildStdin,
    pub(crate) stdout: ChildStdout,
}

/// The command `spec` names, with its stdio piped and its process bound to the lifetime of the
/// handle Suru holds on it.
fn harness_command(spec: &HarnessSpec) -> Command {
    let mut command = Command::new(&spec.executable);
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    command
        .args(&spec.args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    command
}

/// What a harness that could not be launched at all reports.
fn launch_failure(name: &str, spec: &HarnessSpec, error: &std::io::Error) -> ProviderError {
    let failure = ProviderError::new(format!(
        "could not launch {name} `{}`: {error}",
        spec.executable.to_string_lossy()
    ));
    // A harness executable that isn't there is the user's to install, not a
    // fault of the run — every Provider launching a CLI reports it the same
    // typed way, so a client can say so instead of quoting an OS error.
    if error.kind() == std::io::ErrorKind::NotFound {
        return failure.mark_unavailable(ProviderUnavailability::NotInstalled);
    }
    failure
}

/// Launches the harness server `spec` names in its own process tree, draining its stderr.
/// Native diagnostics may contain conversation input, so only byte counts reach the Log.
pub(crate) fn spawn_harness_process(
    spec: &HarnessSpec,
) -> Result<(SpawnedProcess, ProcessStdio), ProviderError> {
    let name: Arc<str> = Arc::from(spec.name.as_str());
    let mut command = harness_command(spec);
    let (mut child, process_tree) =
        spawn_harness_child(&mut command).map_err(|error| launch_failure(&name, spec, &error))?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| ProviderError::new(format!("{name} stdin was unavailable")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ProviderError::new(format!("{name} stdout was unavailable")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ProviderError::new(format!("{name} stderr was unavailable")))?;
    tracing::info!(pid = child.id(), harness = %name, "launched harness server process");
    let stderr_name = name.clone();
    tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::debug!(target: "suru::provider::harness::stderr", harness = %stderr_name, bytes = line.len(), "drained native stderr");
        }
    });

    Ok((
        SpawnedProcess {
            name,
            child,
            process_tree,
        },
        ProcessStdio { stdin, stdout },
    ))
}

/// What a harness Suru ran once and waited out left behind.
pub(crate) struct HarnessRun {
    pub(crate) status: std::process::ExitStatus,
    pub(crate) stdout: String,
    /// Everything the harness said on its stderr. Kept because a one-shot run says
    /// here — and only here — why it would not do what it was asked.
    pub(crate) stderr: String,
}

/// Runs the harness `spec` names to completion, handing it `input` on its stdin and collecting
/// everything it wrote.
///
/// This is the one-shot counterpart to [`spawn_harness_process`]: a harness invoked to answer
/// once and exit — the mode a Provider fulfils an Errand through where its harness offers one
/// (ADR 0011) — rather than a server Suru speaks a protocol to for as long as a Session lives.
/// Nothing is registered with a [`ProcessRegistry`], because there is no Session to lose and
/// nothing for a runtime shutdown to stop: whoever asked bounds the wait with its own deadline,
/// and abandoning that wait takes the process tree down with it.
pub(crate) async fn run_harness_to_completion(
    spec: &HarnessSpec,
    input: String,
) -> Result<HarnessRun, ProviderError> {
    let name = spec.name.as_str();
    let mut command = harness_command(spec);
    let (mut child, process_tree) =
        spawn_harness_child(&mut command).map_err(|error| launch_failure(name, spec, &error))?;
    let missing = |stream| ProviderError::new(format!("{name} {stream} was unavailable"));
    let mut stdin = child.stdin.take().ok_or_else(|| missing("stdin"))?;
    let mut stdout = child.stdout.take().ok_or_else(|| missing("stdout"))?;
    let mut stderr = child.stderr.take().ok_or_else(|| missing("stderr"))?;
    tracing::info!(pid = child.id(), harness = %name, "launched one-shot harness process");
    let mut process = OneShotProcess {
        child,
        process_tree,
    };

    // Nothing here fails the run on its own. A harness that has already made up
    // its mind stops reading, and one that says something Suru cannot read back
    // as text has still said it in the only place that matters — the exit
    // status, and whatever did arrive, are what the run is judged on.
    let mut collected_stdout = String::new();
    let mut collected_stderr = String::new();
    let (_delivered, _read_out, _read_err, status) = tokio::join!(
        async move {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(input.as_bytes()).await;
            let _ = stdin.shutdown().await;
        },
        tokio::io::AsyncReadExt::read_to_string(&mut stdout, &mut collected_stdout),
        tokio::io::AsyncReadExt::read_to_string(&mut stderr, &mut collected_stderr),
        process.child.wait(),
    );
    let status = status
        .map_err(|error| ProviderError::new(format!("could not wait for {name}: {error}")))?;

    Ok(HarnessRun {
        status,
        stdout: collected_stdout,
        stderr: collected_stderr,
    })
}

/// A harness process running one-shot, held so that whatever ends the wait —
/// the answer, a failure, or the caller abandoning it at a deadline — takes the
/// whole process tree down rather than leaving it running unwatched.
struct OneShotProcess {
    child: Child,
    process_tree: ProcessTree,
}

impl Drop for OneShotProcess {
    fn drop(&mut self) {
        let _ = self.process_tree.terminate(&mut self.child);
    }
}

/// Registers `process` and hands it to a supervisor task.
///
/// The returned guard stops the process when it is dropped, and the returned receiver publishes
/// the process's exit failure once the supervisor observes it.
pub(crate) async fn supervise_harness_process<L: HarnessLink>(
    process: SpawnedProcess,
    processes: ProcessRegistry,
    link: L,
) -> Result<(Arc<ProcessGuard>, watch::Receiver<Option<ProviderError>>), ProviderError> {
    let SpawnedProcess {
        name,
        mut child,
        process_tree,
    } = process;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (stopped_tx, stopped_rx) = watch::channel(false);
    let (exit_tx, exit_rx) = watch::channel(None::<ProviderError>);
    let control = ProcessControl {
        name: name.clone(),
        shutdown: shutdown_tx,
        stopped: stopped_rx,
        wait_budget: processes.exit_grace + PROCESS_KILL_TIMEOUT + Duration::from_millis(250),
    };
    let registration_id = match processes.register(control.clone()) {
        Ok(registration_id) => registration_id,
        Err(error) => {
            let _ = process_tree.terminate(&mut child);
            let _ = timeout(PROCESS_KILL_TIMEOUT, child.wait()).await;
            return Err(error);
        }
    };
    tokio::spawn(supervise_child(ChildSupervisor {
        name,
        child,
        shutdown: shutdown_rx,
        stopped: stopped_tx,
        exit: exit_tx,
        link,
        processes,
        registration_id,
        process_tree,
    }));
    Ok((Arc::new(ProcessGuard { control }), exit_rx))
}

struct ChildSupervisor<L: HarnessLink> {
    name: Arc<str>,
    child: Child,
    shutdown: watch::Receiver<bool>,
    stopped: watch::Sender<bool>,
    exit: watch::Sender<Option<ProviderError>>,
    link: L,
    processes: ProcessRegistry,
    registration_id: u64,
    process_tree: ProcessTree,
}

async fn supervise_child<L: HarnessLink>(supervisor: ChildSupervisor<L>) {
    let ChildSupervisor {
        name,
        mut child,
        mut shutdown,
        stopped,
        exit,
        link,
        processes,
        registration_id,
        process_tree,
    } = supervisor;
    let status = tokio::select! {
        biased;
        _ = wait_for_shutdown(&mut shutdown) => {
            link.close();
            match timeout(processes.exit_grace, async {
                link.close_stdin().await;
                child.wait().await
            }).await {
                Ok(status) => status,
                Err(_) => {
                    let _ = process_tree.terminate(&mut child);
                    match timeout(PROCESS_KILL_TIMEOUT, child.wait()).await {
                        Ok(status) => status,
                        Err(_) => Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "forced harness termination did not complete before the deadline",
                        )),
                    }
                }
            }
        }
        status = child.wait() => status,
    };
    let _ = process_tree.terminate(&mut child);

    let message = match status {
        Ok(status) if status.success() => format!("{name} exited unexpectedly"),
        Ok(status) => format!("{name} exited unexpectedly with {status}"),
        Err(error) => format!("could not wait for {name}: {error}"),
    };
    tracing::warn!("{message}");
    // The process is gone, so whatever it hosted is lost with it; marking that
    // here keeps every observer — the exit watch and the link — on one truth.
    let error = ProviderError::new(message).mark_session_lost();
    exit.send_replace(Some(error.clone()));
    link.terminate(error);
    processes.remove(registration_id);
    process_tree.close();
    stopped.send_replace(true);
}

#[cfg(unix)]
struct ProcessTree {
    process_group_id: libc::pid_t,
}

#[cfg(unix)]
fn spawn_harness_child(command: &mut Command) -> std::io::Result<(Child, ProcessTree)> {
    command.process_group(0);
    let child = command.spawn()?;
    let process_group_id = child
        .id()
        .and_then(|id| libc::pid_t::try_from(id).ok())
        .filter(|id| *id > 0)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "harness server process had no process group ID",
            )
        })?;
    Ok((child, ProcessTree { process_group_id }))
}

#[cfg(unix)]
impl ProcessTree {
    fn terminate(&self, child: &mut Child) -> std::io::Result<()> {
        if unsafe { libc::killpg(self.process_group_id, libc::SIGKILL) } == -1 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                if child.id().is_some() {
                    child.start_kill()?;
                }
                return Ok(());
            }
            let _ = child.start_kill();
            return Err(error);
        }
        Ok(())
    }
}

#[cfg(windows)]
struct ProcessTree {
    job: std::os::windows::io::OwnedHandle,
}

#[cfg(windows)]
fn spawn_harness_child(command: &mut Command) -> std::io::Result<(Child, ProcessTree)> {
    use std::{mem, os::windows::io::FromRawHandle, ptr};
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
        return Err(std::io::Error::last_os_error());
    }
    let job = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(job) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let configured = unsafe {
        use std::os::windows::io::AsRawHandle;
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            ptr::addr_of!(limits).cast(),
            mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if configured == 0 {
        return Err(std::io::Error::last_os_error());
    }

    // `CREATE_NO_WINDOW` keeps the console-subsystem app-server from allocating a console of its
    // own. Suru's server runs detached and so has no console to inherit, which would otherwise
    // make Windows pop a terminal window for every launched app-server.
    command.creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW);
    let mut child = command.spawn()?;
    let process_handle = child
        .raw_handle()
        .ok_or_else(|| std::io::Error::other("harness server process had no process handle"))?;
    let assigned = unsafe {
        use std::os::windows::io::AsRawHandle;
        AssignProcessToJobObject(job.as_raw_handle(), process_handle)
    };
    if assigned == 0 {
        let error = std::io::Error::last_os_error();
        let _ = child.start_kill();
        return Err(error);
    }
    let resumed = unsafe { NtResumeProcess(process_handle) };
    if resumed < 0 {
        unsafe {
            use std::os::windows::io::AsRawHandle;
            TerminateJobObject(job.as_raw_handle(), 1);
        }
        return Err(std::io::Error::other(format!(
            "could not resume harness server process: NTSTATUS {resumed:#x}"
        )));
    }

    Ok((child, ProcessTree { job }))
}

#[cfg(windows)]
impl ProcessTree {
    fn terminate(&self, _child: &mut Child) -> std::io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;

        if unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) } == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(not(any(unix, windows)))]
struct ProcessTree;

#[cfg(not(any(unix, windows)))]
fn spawn_harness_child(command: &mut Command) -> std::io::Result<(Child, ProcessTree)> {
    command.spawn().map(|child| (child, ProcessTree))
}

#[cfg(not(any(unix, windows)))]
impl ProcessTree {
    fn terminate(&self, child: &mut Child) -> std::io::Result<()> {
        child.start_kill()
    }
}

impl ProcessTree {
    /// Releases the containment handle before shutdown observers are notified.
    fn close(self) {}
}
