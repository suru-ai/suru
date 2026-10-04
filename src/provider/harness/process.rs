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
    process::{ChildStdin, ChildStdout, Command},
    sync::watch,
    time::{Duration, timeout},
};

use crate::{
    process_tree::{Descendants, ProcessPipes, ProcessTree, ProcessTreeTerminator},
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
    /// Variables set in the process's environment over those it inherits from Suru's.
    pub(crate) env: Vec<(OsString, OsString)>,
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

    /// Stops every live process this registry's runtime launched and refuses any later launch.
    ///
    /// Each process is asked to exit and given its exit grace to do so, but the runtime is
    /// stopping, so whoever awaits this may stop waiting sooner — the Server bounds how long it
    /// waits on each runtime. However the wait ends, every process it was stopping is taken down
    /// with its whole process tree before the wait is let go of: a harness that ignores the
    /// request and its stdin closing, still inside its grace when the wait is abandoned, would
    /// otherwise outlive the runtime that launched it, along with everything it started.
    pub(crate) async fn shutdown(&self) -> Result<(), ProviderError> {
        let processes = {
            let mut state = self
                .state
                .lock()
                .expect("harness process registry lock is not poisoned");
            state.shutting_down = true;
            state.processes.values().cloned().collect::<Vec<_>>()
        };
        let _forced = TerminateOnDrop(&processes);
        for process in &processes {
            process.begin_shutdown();
        }
        let mut first_error = None;
        for process in &processes {
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

/// Takes down, as a registry shutdown lets go of them, every process tree it was stopping.
struct TerminateOnDrop<'a>(&'a [ProcessControl]);

impl Drop for TerminateOnDrop<'_> {
    fn drop(&mut self) {
        for process in self.0 {
            process.terminate();
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
    /// Takes the process tree down at once, without waiting on its supervisor to.
    tree: ProcessTreeTerminator,
}

impl ProcessControl {
    fn begin_shutdown(&self) {
        self.shutdown.send_replace(true);
    }

    /// Kills the process and everything it started, skipping its exit grace. Its supervisor
    /// still reaps it and reports it stopped.
    fn terminate(&self) {
        self.tree.terminate();
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

/// A launched harness server that nothing supervises yet. Dropped as it is, it takes the harness
/// down with everything the harness started.
pub(crate) struct SpawnedProcess {
    name: Arc<str>,
    tree: ProcessTree,
}

/// The launched harness server's piped stdio, ready for a transport to speak over.
pub(crate) struct ProcessStdio {
    pub(crate) stdin: ChildStdin,
    pub(crate) stdout: ChildStdout,
}

/// The command `spec` names, with its stdio piped.
fn harness_command(spec: &HarnessSpec) -> Command {
    let mut command = Command::new(&spec.executable);
    if let Some(cwd) = &spec.cwd {
        command.current_dir(cwd);
    }
    command
        .args(&spec.args)
        .envs(spec.env.iter().map(|(name, value)| (name, value)))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    command
}

/// Launches `command` as the root of a process tree bound to the handle Suru holds on it: a
/// harness's tool shells and MCP servers are its own, so they end with it however it ends.
async fn spawn_harness_tree(command: &mut Command) -> std::io::Result<(ProcessTree, ProcessPipes)> {
    ProcessTree::spawn(command, Descendants::EndWithRoot).await
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
pub(crate) async fn spawn_harness_process(
    spec: &HarnessSpec,
) -> Result<(SpawnedProcess, ProcessStdio), ProviderError> {
    let name: Arc<str> = Arc::from(spec.name.as_str());
    let mut command = harness_command(spec);
    let (tree, pipes) = spawn_harness_tree(&mut command)
        .await
        .map_err(|error| launch_failure(&name, spec, &error))?;

    let stdin = pipes
        .stdin
        .ok_or_else(|| ProviderError::new(format!("{name} stdin was unavailable")))?;
    let stdout = pipes
        .stdout
        .ok_or_else(|| ProviderError::new(format!("{name} stdout was unavailable")))?;
    let stderr = pipes
        .stderr
        .ok_or_else(|| ProviderError::new(format!("{name} stderr was unavailable")))?;
    tracing::info!(pid = tree.id(), harness = %name, "launched harness server process");
    let stderr_name = name.clone();
    tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::debug!(target: "suru::provider::harness::stderr", harness = %stderr_name, bytes = line.len(), "drained native stderr");
        }
    });

    Ok((
        SpawnedProcess { name, tree },
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
    // Held to the end of the run, so whatever ends the wait — the answer, a failure, or the
    // caller abandoning it at a deadline — takes the whole process tree down rather than leaving
    // it running unwatched. Whatever the harness leaves running is taken down as it exits, so
    // nothing it started can hold its output open past the answer.
    let (tree, pipes) = spawn_harness_tree(&mut command)
        .await
        .map_err(|error| launch_failure(name, spec, &error))?;
    let missing = |stream| ProviderError::new(format!("{name} {stream} was unavailable"));
    let mut stdin = pipes.stdin.ok_or_else(|| missing("stdin"))?;
    let mut stdout = pipes.stdout.ok_or_else(|| missing("stdout"))?;
    let mut stderr = pipes.stderr.ok_or_else(|| missing("stderr"))?;
    tracing::info!(pid = tree.id(), harness = %name, "launched one-shot harness process");

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
        tree.wait(),
    );
    let status = status
        .map_err(|error| ProviderError::new(format!("could not wait for {name}: {error}")))?;

    Ok(HarnessRun {
        status,
        stdout: collected_stdout,
        stderr: collected_stderr,
    })
}

/// Registers `process` and hands it to a supervisor task.
///
/// The returned guard stops the process when it is dropped, and the returned receiver publishes
/// the process's exit failure once the supervisor observes it. The supervisor task owns the
/// process tree, so a supervisor that never finishes — its task dropped as the runtime shuts
/// down, or unwound by a panic — still takes the tree down as it is dropped.
pub(crate) async fn supervise_harness_process<L: HarnessLink>(
    process: SpawnedProcess,
    processes: ProcessRegistry,
    link: L,
) -> Result<(Arc<ProcessGuard>, watch::Receiver<Option<ProviderError>>), ProviderError> {
    let SpawnedProcess { name, tree } = process;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (stopped_tx, stopped_rx) = watch::channel(false);
    let (exit_tx, exit_rx) = watch::channel(None::<ProviderError>);
    let control = ProcessControl {
        name: name.clone(),
        shutdown: shutdown_tx,
        stopped: stopped_rx,
        wait_budget: processes.exit_grace + PROCESS_KILL_TIMEOUT + Duration::from_millis(250),
        tree: tree.terminator(),
    };
    let registration_id = match processes.register(control.clone()) {
        Ok(registration_id) => registration_id,
        Err(error) => {
            let _ = tree.terminate();
            let _ = timeout(PROCESS_KILL_TIMEOUT, tree.wait()).await;
            return Err(error);
        }
    };
    tokio::spawn(supervise_child(ChildSupervisor {
        name,
        tree,
        shutdown: shutdown_rx,
        stopped: stopped_tx,
        exit: exit_tx,
        link,
        processes,
        registration_id,
    }));
    Ok((Arc::new(ProcessGuard { control }), exit_rx))
}

struct ChildSupervisor<L: HarnessLink> {
    name: Arc<str>,
    tree: ProcessTree,
    shutdown: watch::Receiver<bool>,
    stopped: watch::Sender<bool>,
    exit: watch::Sender<Option<ProviderError>>,
    link: L,
    processes: ProcessRegistry,
    registration_id: u64,
}

async fn supervise_child<L: HarnessLink>(supervisor: ChildSupervisor<L>) {
    let ChildSupervisor {
        name,
        tree,
        mut shutdown,
        stopped,
        exit,
        link,
        processes,
        registration_id,
    } = supervisor;
    // However the harness exits, the wait takes down whatever it left running.
    let status = tokio::select! {
        biased;
        _ = wait_for_shutdown(&mut shutdown) => {
            link.close();
            match timeout(processes.exit_grace, async {
                link.close_stdin().await;
                tree.wait().await
            }).await {
                Ok(status) => status,
                Err(_) => {
                    let _ = tree.terminate();
                    match timeout(PROCESS_KILL_TIMEOUT, tree.wait()).await {
                        Ok(status) => status,
                        Err(_) => Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "forced harness termination did not complete before the deadline",
                        )),
                    }
                }
            }
        }
        status = tree.wait() => status,
    };

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
    // Released before shutdown observers are notified, taking down a tree whose forced
    // termination outlasted its deadline once more.
    drop(tree);
    stopped.send_replace(true);
}

#[cfg(all(test, unix))]
mod tests {
    use std::{future::Future, pin::Pin};

    use tokio::time::{Duration, timeout};

    use super::{
        HarnessLink, HarnessSpec, ProcessRegistry, spawn_harness_process, supervise_harness_process,
    };
    use crate::{
        process_tree::test_support::{StubbornTree, assert_ended, assert_ended_blocking},
        provider::ProviderError,
    };

    /// A transport that does nothing, over a harness that heeds nothing it could say.
    struct InertLink;

    impl HarnessLink for InertLink {
        fn close(&self) {}

        fn close_stdin(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
            Box::pin(async {})
        }

        fn terminate(&self, _error: ProviderError) {}
    }

    fn spec(fixture: &StubbornTree) -> HarnessSpec {
        let (executable, args, env) = fixture.invocation();
        HarnessSpec {
            executable,
            args,
            name: "Fixture harness".to_owned(),
            cwd: None,
            env,
        }
    }

    /// The runtime going away drops the supervisor mid-wait, with nothing left to run the
    /// supervisor's own forced termination; the process tree it owned takes the harness down
    /// with everything it started as it is dropped.
    #[test]
    fn dropping_the_runtime_takes_down_a_supervised_harness_and_everything_it_started() {
        let fixture = StubbornTree::running();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build a runtime");
        let (guard, root, anchor, descendant) = runtime.block_on(async {
            let (process, _stdio) = spawn_harness_process(&spec(&fixture))
                .await
                .expect("launch");
            let root = process.tree.id().expect("the harness is running") as libc::pid_t;
            let anchor = process.tree.anchor_id().expect("the harness is anchored");
            let (guard, _exit) = supervise_harness_process(
                process,
                ProcessRegistry::new("Fixture harness"),
                InertLink,
            )
            .await
            .expect("supervise");
            (guard, root, anchor, fixture.descendant().await)
        });

        drop(runtime);

        assert_ended_blocking(descendant);
        // Reaped as its tree was dropped, as is its anchor: no runtime is left to reap either.
        assert_ended_blocking(root);
        assert_ended_blocking(anchor);
        drop(guard);
    }

    /// The Server stops waiting on a runtime's shutdown at its own deadline, which may come well
    /// within a harness's exit grace. A harness ignoring the request to stop is taken down with
    /// everything it started as the wait is abandoned, not once its grace runs out.
    #[tokio::test]
    async fn a_registry_shutdown_abandoned_at_its_deadline_takes_down_every_process_tree() {
        let fixture = StubbornTree::running();
        let mut processes = ProcessRegistry::new("Fixture harness");
        // Longer than any test waits: only the abandoned wait can be what ends the harness.
        processes.set_exit_grace(Duration::from_secs(600));
        let (process, _stdio) = spawn_harness_process(&spec(&fixture))
            .await
            .expect("launch");
        let root = process.tree.id().expect("the harness is running") as libc::pid_t;
        let (_guard, _exit) = supervise_harness_process(process, processes.clone(), InertLink)
            .await
            .expect("supervise");
        let descendant = fixture.descendant().await;

        let abandoned = timeout(Duration::from_millis(50), processes.shutdown()).await;

        assert!(
            abandoned.is_err(),
            "the harness ignores being asked to stop"
        );
        assert_ended(descendant).await;
        // Reaped by its supervisor, which goes on running.
        assert_ended(root).await;
    }
}
