//! Ownership of the Codex app-server child processes Suru launches.
//!
//! Every launched process is registered with a [`ProcessRegistry`] and supervised by a task that
//! outlives its transport, so a Session shutdown, a runtime shutdown, and an unexpected exit all
//! converge on the same terminated process tree and the same reported failure.

use std::{
    collections::HashMap,
    ffi::OsStr,
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

use super::{codex_error, transport::TransportLink};
use crate::provider::{ProviderError, wait_for_shutdown};

const PROCESS_EXIT_GRACE_PERIOD: Duration = Duration::from_millis(500);
const PROCESS_KILL_TIMEOUT: Duration = Duration::from_millis(500);

/// Tracks every live Codex app-server process so a runtime shutdown can stop them all.
#[derive(Clone, Debug)]
pub(super) struct ProcessRegistry {
    next_id: Arc<AtomicU64>,
    state: Arc<StdMutex<ProcessRegistryState>>,
}

#[derive(Debug)]
struct ProcessRegistryState {
    shutting_down: bool,
    processes: HashMap<u64, ProcessControl>,
}

impl ProcessRegistry {
    pub(super) fn new() -> Self {
        Self {
            next_id: Arc::new(AtomicU64::new(1)),
            state: Arc::new(StdMutex::new(ProcessRegistryState {
                shutting_down: false,
                processes: HashMap::new(),
            })),
        }
    }

    fn register(&self, process: ProcessControl) -> Result<u64, ProviderError> {
        let mut state = self
            .state
            .lock()
            .expect("Codex process registry lock is not poisoned");
        if state.shutting_down {
            return Err(codex_error("Codex runtime is shutting down"));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        state.processes.insert(id, process);
        Ok(id)
    }

    fn remove(&self, id: u64) {
        self.state
            .lock()
            .expect("Codex process registry lock is not poisoned")
            .processes
            .remove(&id);
    }

    pub(super) async fn shutdown(&self) -> Result<(), ProviderError> {
        let processes = {
            let mut state = self
                .state
                .lock()
                .expect("Codex process registry lock is not poisoned");
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
    shutdown: watch::Sender<bool>,
    stopped: watch::Receiver<bool>,
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
        let wait = async move {
            stopped
                .wait_for(|stopped| *stopped)
                .await
                .map(|_| ())
                .map_err(|_| {
                    codex_error("Codex app-server process supervisor stopped unexpectedly")
                })
        };
        timeout(
            PROCESS_EXIT_GRACE_PERIOD + PROCESS_KILL_TIMEOUT + Duration::from_millis(250),
            wait,
        )
        .await
        .map_err(|_| codex_error("Codex app-server did not stop within the shutdown deadline"))?
    }
}

/// Keeps one supervised process alive; dropping it asks that process to stop.
pub(super) struct ProcessGuard {
    control: ProcessControl,
}

impl ProcessGuard {
    pub(super) fn begin_shutdown(&self) {
        self.control.begin_shutdown();
    }

    pub(super) async fn wait_until_stopped(&self) -> Result<(), ProviderError> {
        self.control.wait_until_stopped().await
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        self.begin_shutdown();
    }
}

/// A launched app-server that nothing supervises yet.
pub(super) struct SpawnedProcess {
    child: Child,
    process_tree: ProcessTree,
}

/// The launched app-server's piped stdio, ready for a transport to speak over.
pub(super) struct ProcessStdio {
    pub(super) stdin: ChildStdin,
    pub(super) stdout: ChildStdout,
}

/// Launches `codex app-server` in its own process tree, draining its stderr.
pub(super) fn spawn_codex_process(
    executable: &OsStr,
) -> Result<(SpawnedProcess, ProcessStdio), ProviderError> {
    let mut command = Command::new(executable);
    command
        .arg("app-server")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let (mut child, process_tree) = spawn_codex_child(&mut command).map_err(|error| {
        codex_error(format!(
            "could not launch Codex app-server `{}`: {error}",
            executable.to_string_lossy()
        ))
    })?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| codex_error("Codex app-server stdin was unavailable"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| codex_error("Codex app-server stdout was unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| codex_error("Codex app-server stderr was unavailable"))?;
    tokio::spawn(async move {
        let mut stderr = stderr;
        let mut sink = tokio::io::sink();
        let _ = tokio::io::copy(&mut stderr, &mut sink).await;
    });

    Ok((
        SpawnedProcess {
            child,
            process_tree,
        },
        ProcessStdio { stdin, stdout },
    ))
}

/// Registers `process` and hands it to a supervisor task.
///
/// The returned guard stops the process when it is dropped, and the returned receiver publishes
/// the process's exit failure once the supervisor observes it.
pub(super) async fn supervise_codex_process(
    process: SpawnedProcess,
    processes: ProcessRegistry,
    transport: TransportLink,
) -> Result<(Arc<ProcessGuard>, watch::Receiver<Option<ProviderError>>), ProviderError> {
    let SpawnedProcess {
        mut child,
        process_tree,
    } = process;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (stopped_tx, stopped_rx) = watch::channel(false);
    let (exit_tx, exit_rx) = watch::channel(None::<ProviderError>);
    let control = ProcessControl {
        shutdown: shutdown_tx,
        stopped: stopped_rx,
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
        child,
        shutdown: shutdown_rx,
        stopped: stopped_tx,
        exit: exit_tx,
        transport,
        processes,
        registration_id,
        process_tree,
    }));
    Ok((Arc::new(ProcessGuard { control }), exit_rx))
}

struct ChildSupervisor {
    child: Child,
    shutdown: watch::Receiver<bool>,
    stopped: watch::Sender<bool>,
    exit: watch::Sender<Option<ProviderError>>,
    transport: TransportLink,
    processes: ProcessRegistry,
    registration_id: u64,
    process_tree: ProcessTree,
}

async fn supervise_child(supervisor: ChildSupervisor) {
    let ChildSupervisor {
        mut child,
        mut shutdown,
        stopped,
        exit,
        transport,
        processes,
        registration_id,
        process_tree,
    } = supervisor;
    let status = tokio::select! {
        biased;
        _ = wait_for_shutdown(&mut shutdown) => {
            transport.close();
            match timeout(PROCESS_EXIT_GRACE_PERIOD, async {
                transport.close_stdin().await;
                child.wait().await
            }).await {
                Ok(status) => status,
                Err(_) => {
                    let _ = process_tree.terminate(&mut child);
                    match timeout(PROCESS_KILL_TIMEOUT, child.wait()).await {
                        Ok(status) => status,
                        Err(_) => Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "forced Codex termination did not complete before the deadline",
                        )),
                    }
                }
            }
        }
        status = child.wait() => status,
    };
    let _ = process_tree.terminate(&mut child);

    let message = match status {
        Ok(status) if status.success() => "Codex app-server exited unexpectedly".to_owned(),
        Ok(status) => format!("Codex app-server exited unexpectedly with {status}"),
        Err(error) => format!("could not wait for Codex app-server: {error}"),
    };
    let error = codex_error(message);
    exit.send_replace(Some(error.clone()));
    transport.terminate(error);
    processes.remove(registration_id);
    process_tree.close();
    stopped.send_replace(true);
}

#[cfg(unix)]
struct ProcessTree {
    process_group_id: libc::pid_t,
}

#[cfg(unix)]
fn spawn_codex_child(command: &mut Command) -> std::io::Result<(Child, ProcessTree)> {
    command.process_group(0);
    let child = command.spawn()?;
    let process_group_id = child
        .id()
        .and_then(|id| libc::pid_t::try_from(id).ok())
        .filter(|id| *id > 0)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Codex app-server had no process group ID",
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
fn spawn_codex_child(command: &mut Command) -> std::io::Result<(Child, ProcessTree)> {
    use std::{mem, os::windows::io::FromRawHandle, ptr};
    use windows_sys::Win32::System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject, TerminateJobObject,
        },
        Threading::CREATE_SUSPENDED,
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

    command.creation_flags(CREATE_SUSPENDED);
    let mut child = command.spawn()?;
    let process_handle = child
        .raw_handle()
        .ok_or_else(|| std::io::Error::other("Codex app-server had no process handle"))?;
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
            "could not resume Codex app-server: NTSTATUS {resumed:#x}"
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
fn spawn_codex_child(command: &mut Command) -> std::io::Result<(Child, ProcessTree)> {
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
