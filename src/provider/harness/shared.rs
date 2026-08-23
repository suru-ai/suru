//! One shared harness server process per Provider runtime, hosting all of that Provider's
//! Sessions.
//!
//! [`SharedHarness`] launches its process lazily on the first demand — a Model discovery or a
//! Session start — and every concurrent demand shares that one launch. The process lives until
//! server shutdown; nothing is torn down when the last Session lets go of it. When the process
//! crashes, every handle granted from it observes the failure as a lost Provider Session, and the
//! next demand launches a fresh process.

use std::sync::Arc;

use tokio::sync::{Mutex as TokioMutex, watch};

use super::process::{
    HarnessLink, HarnessSpec, ProcessGuard, ProcessRegistry, ProcessStdio, spawn_harness_process,
    supervise_harness_process,
};
use crate::provider::{ProviderError, ProviderFuture};

/// Builds a Provider's client over a freshly launched shared harness process.
pub(crate) trait HarnessConnector: Send + Sync + 'static {
    type Connection: HarnessLink + Clone;

    /// Wraps the spawned process's stdio in the Provider's transport. Runs before the process is
    /// supervised, so it must not wait on the harness; the handshake belongs in `initialize`.
    fn connect(&self, io: ProcessStdio) -> Result<Self::Connection, ProviderError>;

    /// Completes the Provider's startup handshake over the now-supervised connection. Must fail
    /// once the connection's `HarnessLink` is terminated or closed — the launch holds every later
    /// demand behind it, so a handshake that outlives its process must not hang.
    fn initialize(&self, connection: &Self::Connection) -> ProviderFuture<'_, ()>;
}

/// A Provider runtime's one shared harness server process, launched lazily and supervised until
/// server shutdown.
pub(crate) struct SharedHarness<C: HarnessConnector> {
    spec: HarnessSpec,
    connector: C,
    processes: ProcessRegistry,
    state: TokioMutex<Option<LiveHarness<C::Connection>>>,
}

/// One demand's grant: the shared connection to the live harness process, and that process's
/// crash as something the demander can wait on.
pub(crate) struct SharedHarnessHandle<T> {
    connection: T,
    crash: watch::Receiver<Option<ProviderError>>,
}

impl<T> SharedHarnessHandle<T> {
    pub(crate) fn connection(&self) -> &T {
        &self.connection
    }

    /// Resolves with the lost-Session failure once the process this handle was granted from
    /// crashes. An intended shutdown is not a crash: across one, this pends forever.
    pub(crate) async fn crashed(&self) -> ProviderError {
        let mut crash = self.crash.clone();
        match published_failure(&mut crash).await {
            Some(error) => error,
            None => std::future::pending().await,
        }
    }
}

/// The failure `watch` eventually publishes, or `None` once its sender ends without one.
async fn published_failure(
    watch: &mut watch::Receiver<Option<ProviderError>>,
) -> Option<ProviderError> {
    loop {
        if let Some(error) = watch.borrow_and_update().clone() {
            return Some(error);
        }
        watch.changed().await.ok()?;
    }
}

struct LiveHarness<T> {
    connection: T,
    /// Held so the process stays supervised while it is the live harness; nothing reads it —
    /// dropping it is what asks the process to stop.
    guard: Arc<ProcessGuard>,
    crash: watch::Receiver<Option<ProviderError>>,
}

impl<T: Clone> LiveHarness<T> {
    fn handle(&self) -> SharedHarnessHandle<T> {
        SharedHarnessHandle {
            connection: self.connection.clone(),
            crash: self.crash.clone(),
        }
    }
}

impl<C: HarnessConnector> SharedHarness<C> {
    pub(crate) fn new(spec: HarnessSpec, connector: C) -> Self {
        let processes = ProcessRegistry::new(spec.name.clone());
        Self {
            spec,
            connector,
            processes,
            state: TokioMutex::new(None),
        }
    }

    /// Bounds how long the stopping process may exit gracefully before it is forced down;
    /// injectable so tests with fixtures that ignore stdin closure do not wait out the default.
    pub(crate) fn with_process_exit_grace(mut self, exit_grace: tokio::time::Duration) -> Self {
        self.processes.set_exit_grace(exit_grace);
        self
    }

    /// Grants a handle on the live shared harness process, launching it first when no launch has
    /// happened yet. Demands arriving while a launch is in flight wait for it and share its
    /// outcome rather than launching again.
    pub(crate) async fn demand(
        &self,
    ) -> Result<SharedHarnessHandle<C::Connection>, ProviderError> {
        let mut state = self.state.lock().await;
        self.processes.refuse_if_shutting_down()?;
        if let Some(live) = state.as_ref() {
            // A demand racing the supervisor's own notice of a crash may still
            // be granted the dying process; its handle observes the crash like
            // any other, and the demand after that launches fresh.
            if live.crash.borrow().is_none() {
                return Ok(live.handle());
            }
            *state = None;
        }
        let live = self.launch().await?;
        let handle = live.handle();
        *state = Some(live);
        Ok(handle)
    }

    async fn launch(&self) -> Result<LiveHarness<C::Connection>, ProviderError> {
        let (process, stdio) = spawn_harness_process(&self.spec)?;
        let connection = self.connector.connect(stdio)?;
        let (guard, exit) =
            supervise_harness_process(process, self.processes.clone(), connection.clone()).await?;
        self.connector.initialize(&connection).await?;
        let (crash_tx, crash_rx) = watch::channel(None);
        tokio::spawn(fan_out_crash(exit, crash_tx, self.processes.clone()));
        Ok(LiveHarness {
            connection,
            guard,
            crash: crash_rx,
        })
    }

    /// Tears the shared process down with the registry's grace-then-kill discipline and refuses
    /// every later demand.
    pub(crate) async fn shutdown(&self) -> Result<(), ProviderError> {
        let result = self.processes.shutdown().await;
        *self.state.lock().await = None;
        result
    }
}

/// Publishes the supervised process's exit — already marked a lost Provider Session by the
/// supervisor — to every handle, unless the registry is shutting down, in which case the exit was
/// asked for and no Session loses anything it wasn't already losing.
async fn fan_out_crash(
    mut exit: watch::Receiver<Option<ProviderError>>,
    crash: watch::Sender<Option<ProviderError>>,
    processes: ProcessRegistry,
) {
    let Some(error) = published_failure(&mut exit).await else {
        return;
    };
    if processes.is_shutting_down() {
        return;
    }
    tracing::warn!(
        harness = %processes.name(),
        "shared harness process crashed; its hosted Sessions lose their active Turns"
    );
    crash.send_replace(Some(error));
}

// The scripted fixtures are `sh` programs and the liveness probe is `libc::kill`, so these tests
// are Unix-only; the machinery itself carries its own per-platform process-tree paths.
#[cfg(all(test, unix))]
mod tests {
    use std::{
        future::Future,
        pin::Pin,
        sync::{Arc, Mutex as StdMutex, atomic::AtomicBool},
    };

    use tokio::{
        io::AsyncWriteExt,
        process::{ChildStdin, ChildStdout},
        sync::Mutex as TokioMutex,
        time::{Duration, timeout},
    };

    use super::{HarnessConnector, SharedHarness};
    use crate::provider::{
        ProviderError, ProviderFuture,
        harness::{HarnessLink, HarnessSpec, ProcessStdio},
    };

    /// A scripted harness server: counts its launches, records its PID, obeys
    /// `die` on stdin, exits gracefully when stdin closes, and leaves an
    /// `exited` marker behind unless it was force-killed.
    struct ScriptedHarness {
        directory: tempfile::TempDir,
    }

    const SCRIPT: &str = r#"#!/bin/sh
attempt=1
if [ -e "$DIR/attempts" ]; then
  attempt=$(( $(cat "$DIR/attempts") + 1 ))
fi
printf '%s\n' "$$" > "$DIR/pid"
printf '%s\n' "$attempt" > "$DIR/attempts"
trap 'printf exited > "$DIR/exited"' EXIT
while IFS= read -r line; do
  case "$line" in
    die) exit 7 ;;
  esac
done
STUBBORN_TAIL
"#;

    impl ScriptedHarness {
        fn new() -> Self {
            Self::with_tail("exit 0")
        }

        /// A harness that ignores its closed stdin, forcing the kill path.
        fn new_stubborn() -> Self {
            Self::with_tail("trap '' TERM\nwhile :; do sleep 1; done")
        }

        fn with_tail(tail: &str) -> Self {
            let directory = tempfile::tempdir().expect("create scripted harness directory");
            let executable = directory.path().join("harness");
            let script = SCRIPT
                .replace(
                    "$DIR",
                    directory.path().to_str().expect("fixture path is UTF-8"),
                )
                .replace("STUBBORN_TAIL", tail);
            std::fs::write(&executable, script).expect("write scripted harness");
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
                .expect("make scripted harness executable");
            Self { directory }
        }

        fn spec(&self) -> HarnessSpec {
            HarnessSpec {
                executable: self.directory.path().join("harness").into(),
                args: Vec::new(),
                name: "Fixture harness".to_owned(),
            }
        }

        fn attempts(&self) -> usize {
            std::fs::read_to_string(self.directory.path().join("attempts"))
                .unwrap_or_default()
                .trim()
                .parse()
                .unwrap_or(0)
        }

        async fn wait_for_attempts(&self, expected: usize) {
            timeout(Duration::from_secs(2), async {
                while self.attempts() < expected {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "scripted harness reaches {expected} launches; saw {}",
                    self.attempts()
                )
            });
        }

        fn pid(&self) -> i32 {
            std::fs::read_to_string(self.directory.path().join("pid"))
                .expect("read scripted harness PID")
                .trim()
                .parse()
                .expect("scripted harness PID is numeric")
        }

        fn is_running(&self) -> bool {
            unsafe { libc::kill(self.pid(), 0) == 0 }
        }

        fn exited_gracefully(&self) -> bool {
            self.directory.path().join("exited").exists()
        }
    }

    /// The test Provider client over a scripted harness: holds the stdio so
    /// the harness stays connected, and records what the supervisor did to it.
    #[derive(Clone, Default)]
    struct FixtureConnection {
        stdin: Arc<TokioMutex<Option<ChildStdin>>>,
        stdout: Arc<StdMutex<Option<ChildStdout>>>,
        closed: Arc<AtomicBool>,
        terminated: Arc<StdMutex<Option<ProviderError>>>,
    }

    impl FixtureConnection {
        async fn send(&self, line: &str) {
            let mut stdin = self.stdin.lock().await;
            let stdin = stdin.as_mut().expect("fixture connection still has stdin");
            stdin
                .write_all(format!("{line}\n").as_bytes())
                .await
                .expect("write to scripted harness");
            stdin.flush().await.expect("flush scripted harness stdin");
        }
    }

    impl HarnessLink for FixtureConnection {
        fn close(&self) {
            self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        }

        fn close_stdin(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
            Box::pin(async move {
                let stdin = self.stdin.lock().await.take();
                if let Some(mut stdin) = stdin {
                    let _ = stdin.shutdown().await;
                }
            })
        }

        fn terminate(&self, error: ProviderError) {
            let mut terminated = self
                .terminated
                .lock()
                .expect("fixture termination lock is not poisoned");
            terminated.get_or_insert(error);
        }
    }

    #[derive(Default)]
    struct FixtureConnector {
        connections: Arc<StdMutex<Vec<FixtureConnection>>>,
        /// When set, `initialize` pends until the watched flag turns true, so
        /// a test can hold several demands open across one launch.
        hold_initialize: Option<tokio::sync::watch::Receiver<bool>>,
        /// How many `initialize` calls fail before the connector cooperates.
        fail_initializations: std::sync::atomic::AtomicUsize,
    }

    impl FixtureConnector {
        fn latest_connection(&self) -> FixtureConnection {
            self.connections
                .lock()
                .expect("fixture connector lock is not poisoned")
                .last()
                .expect("a connection was made")
                .clone()
        }
    }

    impl HarnessConnector for Arc<FixtureConnector> {
        type Connection = FixtureConnection;

        fn connect(&self, io: ProcessStdio) -> Result<FixtureConnection, ProviderError> {
            let connection = FixtureConnection {
                stdin: Arc::new(TokioMutex::new(Some(io.stdin))),
                stdout: Arc::new(StdMutex::new(Some(io.stdout))),
                ..FixtureConnection::default()
            };
            self.connections
                .lock()
                .expect("fixture connector lock is not poisoned")
                .push(connection.clone());
            Ok(connection)
        }

        fn initialize(&self, _connection: &FixtureConnection) -> ProviderFuture<'_, ()> {
            let hold = self.hold_initialize.clone();
            let failures_left = &self.fail_initializations;
            Box::pin(async move {
                if failures_left
                    .fetch_update(
                        std::sync::atomic::Ordering::SeqCst,
                        std::sync::atomic::Ordering::SeqCst,
                        |left| left.checked_sub(1),
                    )
                    .is_ok()
                {
                    return Err(ProviderError::new("fixture handshake failed"));
                }
                if let Some(mut released) = hold {
                    released
                        .wait_for(|released| *released)
                        .await
                        .map_err(|_| ProviderError::new("fixture initialize gate was dropped"))?;
                }
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn the_first_demand_launches_and_every_later_demand_shares_the_process() {
        let fixture = ScriptedHarness::new();
        let connector = Arc::new(FixtureConnector::default());
        let harness = SharedHarness::new(fixture.spec(), connector.clone());

        let first = harness.demand().await.expect("first demand launches");
        fixture.wait_for_attempts(1).await;
        let second = harness.demand().await.expect("second demand shares");
        assert_eq!(fixture.attempts(), 1, "one launch serves both demands");

        // The process is the runtime's, not any demand's: it outlives every handle.
        drop(first);
        drop(second);
        tokio::task::yield_now().await;
        assert!(fixture.is_running(), "process outlives its handles");
        harness.demand().await.expect("later demand still shares");
        assert_eq!(fixture.attempts(), 1);

        harness.shutdown().await.expect("shutdown tears down");
    }

    #[tokio::test]
    async fn demands_arriving_during_a_launch_share_it() {
        let fixture = ScriptedHarness::new();
        let (release, released) = tokio::sync::watch::channel(false);
        let connector = Arc::new(FixtureConnector {
            hold_initialize: Some(released),
            ..FixtureConnector::default()
        });
        let harness = Arc::new(SharedHarness::new(fixture.spec(), connector.clone()));

        let demands = [(); 2].map(|_| {
            let harness = harness.clone();
            tokio::spawn(async move { harness.demand().await })
        });
        // Both demands are in flight before the one launch is allowed to finish.
        fixture.wait_for_attempts(1).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        release.send_replace(true);

        for demand in demands {
            demand
                .await
                .expect("demand task runs")
                .expect("demand shares the launch");
        }
        assert_eq!(fixture.attempts(), 1, "concurrent demands share one spawn");

        harness.shutdown().await.expect("shutdown tears down");
    }

    #[tokio::test]
    async fn a_crash_reaches_every_hosted_session_and_the_next_demand_relaunches() {
        let fixture = ScriptedHarness::new();
        let connector = Arc::new(FixtureConnector::default());
        let harness = SharedHarness::new(fixture.spec(), connector.clone());

        let first = harness.demand().await.expect("first demand launches");
        let second = harness.demand().await.expect("second demand shares");
        fixture.wait_for_attempts(1).await;
        let crashed_pid = fixture.pid();

        connector.latest_connection().send("die").await;

        for handle in [&first, &second] {
            let failure = timeout(Duration::from_secs(2), handle.crashed())
                .await
                .expect("every hosted Session observes the crash");
            assert!(
                failure.is_session_lost(),
                "a crash reads as a lost Provider Session, got: {failure}"
            );
        }

        harness.demand().await.expect("the next demand relaunches");
        fixture.wait_for_attempts(2).await;
        assert_ne!(fixture.pid(), crashed_pid, "the relaunch is a fresh process");

        harness.shutdown().await.expect("shutdown tears down");
    }

    #[tokio::test]
    async fn server_shutdown_stops_the_process_gracefully_and_refuses_later_demands() {
        let fixture = ScriptedHarness::new();
        let connector = Arc::new(FixtureConnector::default());
        let harness = SharedHarness::new(fixture.spec(), connector.clone());

        let handle = harness.demand().await.expect("demand launches");
        fixture.wait_for_attempts(1).await;

        harness.shutdown().await.expect("shutdown succeeds");
        assert!(
            fixture.exited_gracefully(),
            "closing stdin lets the process exit on its own"
        );
        assert!(
            timeout(Duration::from_millis(250), handle.crashed())
                .await
                .is_err(),
            "an intended shutdown is not a crash"
        );

        let Err(refused) = harness.demand().await else {
            panic!("demands after shutdown are refused");
        };
        assert!(
            refused.to_string().contains("is shutting down"),
            "the refusal names the reason, got: {refused}"
        );
        assert_eq!(fixture.attempts(), 1, "a refused demand launches nothing");
    }

    #[tokio::test]
    async fn an_unresponsive_process_is_forced_down_within_the_injected_grace() {
        let fixture = ScriptedHarness::new_stubborn();
        let connector = Arc::new(FixtureConnector::default());
        let harness = SharedHarness::new(fixture.spec(), connector.clone())
            .with_process_exit_grace(Duration::from_millis(50));

        harness.demand().await.expect("demand launches");
        fixture.wait_for_attempts(1).await;

        let started = std::time::Instant::now();
        harness
            .shutdown()
            .await
            .expect("shutdown forces the process down");
        assert!(
            started.elapsed() < Duration::from_millis(300),
            "the injected grace governs the stop, took {:?}",
            started.elapsed()
        );
        assert!(
            !fixture.exited_gracefully(),
            "an ignored stdin closure ends in a forced kill"
        );
        timeout(Duration::from_secs(2), async {
            while fixture.is_running() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the forced-down process is gone");
    }

    #[tokio::test]
    async fn a_failed_launch_is_torn_down_and_the_next_demand_retries_fresh() {
        let fixture = ScriptedHarness::new();
        let connector = Arc::new(FixtureConnector {
            fail_initializations: std::sync::atomic::AtomicUsize::new(1),
            ..FixtureConnector::default()
        });
        let harness = SharedHarness::new(fixture.spec(), connector.clone());

        let Err(failure) = harness.demand().await else {
            panic!("a failed handshake fails the demand");
        };
        assert!(
            failure.to_string().contains("fixture handshake failed"),
            "the demand reports the handshake failure, got: {failure}"
        );
        timeout(Duration::from_secs(2), async {
            while !fixture.exited_gracefully() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the failed launch's process is stopped");

        harness.demand().await.expect("the next demand retries");
        fixture.wait_for_attempts(2).await;

        harness.shutdown().await.expect("shutdown tears down");
    }
}
