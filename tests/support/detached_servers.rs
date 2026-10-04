//! The detached Suru servers a test causes to launch through the real binary,
//! owned so that they are gone when the test is, however it ends.
//!
//! A managed client launches its server in a process group of its own, on
//! purpose, so the server outlives the client that launched it. Nothing a
//! test drops therefore takes that server down: not the client, not the
//! runtime, and not the temporary state directory, which the server neither
//! watches nor needs once it is running. Every such server a test fails to
//! stop runs on for good, and a test fails to stop it whenever it panics,
//! times out or is cancelled, or loses track of it — a server that lost an
//! election and was still waiting out its handoff when the winner was stopped
//! takes the channel over afterwards, recreating the directory the test had
//! just deleted.
//!
//! [`DetachedServers`] owns the state directory those servers are launched
//! against, and identifies them by it rather than by the runtime descriptor,
//! which only ever names the one that won.

use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use suru::{
    managed_client::ManagedClientConfig,
    protocol::{RuntimeDescriptor, ServerShutdown, ShutdownReason},
};
use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System, UpdateKind};

use super::PROGRESS_DEADLINE;

/// The variables that name each Provider's executable, every one of which a
/// server launched under a [`DetachedServers`] finds pointed at nothing.
const PROVIDER_PATH_VARIABLES: [&str; 3] =
    ["SURU_CODEX_PATH", "SURU_COPILOT_PATH", "SURU_CLAUDE_PATH"];

/// How long a server asked to stop is given to finish before it is killed. A
/// test's server runs no real Provider, so one that has not finished well
/// within this is not going to.
const GRACEFUL_STOP: Duration = Duration::from_secs(2);

/// The file in a state directory that marks it as one a [`DetachedServers`]
/// made, holding the token it was made with. The [`watcher`] acts on no
/// directory that does not carry the token it was told.
const MARKER_FILE: &str = ".detached-servers";

/// An isolated state directory for the real `suru` binary, and everything a
/// test launches against it. Dropped — at the end of the test, or while it
/// unwinds from a panic — it takes all of it down before the directory goes:
///
/// 1. Every Suru CLI or TUI process launched against the directory is killed
///    first, since either may launch a server of its own at any moment.
/// 2. Every registered server is asked to stop, as `suru server stop` would
///    ask it, so it takes its Providers down the way it always does, and is
///    given [`GRACEFUL_STOP`] to finish.
/// 3. Every `suru __server` process launched against the directory that is
///    still running is killed: the winner if it ignored the request, but also
///    whichever servers lost an election, replaced another, or never got as
///    far as registering, none of which the runtime descriptor names.
///
/// Processes are identified by the directory itself — named in a server's
/// arguments, or in `SURU_STATE_DIR` for a CLI or TUI — which every test
/// owns alone, so nothing another test or the user is running is touched.
///
/// Nothing can launch a server once that is done. A managed client launches
/// from a task of the test's own runtime, and `#[tokio::test]` gives a test a
/// single-threaded runtime, so while this runs on that thread no other task
/// of the test makes progress, and the runtime cancels them all without
/// polling them again once the test is over. A CLI or TUI process runs on its
/// own, which is why those are killed first. Once it is done, a test that
/// ended cleanly checks that no server a managed client launched for it is
/// left a zombie of the test process.
///
/// A test process that never gets as far as dropping it — one a runner times
/// out, cancelled with Ctrl-C, or killed outright — leaves the same work to
/// the [`watcher`], a process of its own that does it once the test process
/// has ended, when nothing in it can launch anything again.
///
/// Servers launched under it are also kept from the Providers installed on
/// the machine: [`DetachedServers::client_config`] and
/// [`DetachedServers::isolated_environment`] point each Provider's executable
/// at a path where nothing exists. A test that wants a Provider launched sets
/// its variable again, after the isolated ones.
pub struct DetachedServers {
    state_dir: tempfile::TempDir,
    token: watcher::Token,
}

impl Default for DetachedServers {
    fn default() -> Self {
        Self::new()
    }
}

impl DetachedServers {
    pub fn new() -> Self {
        let state_dir = tempfile::tempdir().expect("create isolated state directory");
        let token = watcher::Token::new();
        std::fs::write(state_dir.path().join(MARKER_FILE), token.as_str())
            .expect("mark the isolated state directory");
        watcher::tell(watcher::Change::InUse, &token, state_dir.path());
        Self { state_dir, token }
    }

    /// The directory every server and client of the test is launched
    /// against, removed only once nothing launched against it is running.
    pub fn state_dir(&self) -> &Path {
        self.state_dir.path()
    }

    /// A managed client for `channel` under [`Self::state_dir`] that launches
    /// the tested `suru` binary, isolated as [`Self::isolate`] isolates one.
    pub fn client_config(&self, channel: &str) -> ManagedClientConfig {
        self.isolate(
            ManagedClientConfig::new(self.state_dir(), channel)
                .expect("configure managed client")
                .with_server_executable(env!("CARGO_BIN_EXE_suru")),
        )
    }

    /// `config`, launching its servers with [`Self::isolated_environment`].
    pub fn isolate(&self, config: ManagedClientConfig) -> ManagedClientConfig {
        self.isolated_environment()
            .into_iter()
            .fold(config, |config, (key, value)| {
                config.with_server_env(key, value)
            })
    }

    /// How many `suru __server` processes launched against the state
    /// directory are running now: an election's winner, and every server
    /// still waiting in one or starting towards it.
    pub fn servers_running(&self) -> usize {
        let directory = self.state_dir().as_os_str();
        Processes::new()
            .pids(|process| launched_against(process, directory) && is_server(process))
            .len()
    }

    /// The environment that keeps a server, or a CLI or TUI that may launch
    /// one, from any Provider installed on the machine: every Provider's
    /// executable is a path where nothing exists, so a server asked for one
    /// finds it unavailable rather than launching the real thing.
    pub fn isolated_environment(&self) -> [(&'static str, PathBuf); 3] {
        let absent = self.state_dir().join("absent-provider");
        PROVIDER_PATH_VARIABLES.map(|variable| (variable, absent.clone()))
    }
}

impl Drop for DetachedServers {
    fn drop(&mut self) {
        let survivors = stop_launched_against(self.state_dir());
        if !survivors.is_empty() {
            let problem = format!(
                "processes launched against {:?} survived being killed: {survivors:?}",
                self.state_dir()
            );
            // A second panic while the test unwinds would abort the whole
            // binary, taking the report of the first one with it.
            if std::thread::panicking() {
                eprintln!("{problem}");
            } else {
                panic!("{problem}");
            }
            return;
        }
        watcher::tell(watcher::Change::Stopped, &self.token, self.state_dir());
        // A test unwinding may have dropped what would have collected a
        // server of its own, so the check is for a test that ends cleanly.
        if !std::thread::panicking()
            && let Some(pid) = uncollected_server(self.state_dir())
        {
            panic!("server {pid} the test launched exited and was never collected");
        }
    }
}

/// Takes down everything launched against `state_dir`, in the order
/// [`DetachedServers`] describes, and answers whatever outlived being killed.
fn stop_launched_against(state_dir: &Path) -> Vec<Pid> {
    let directory = state_dir.as_os_str();
    let mut processes = Processes::new();

    let clients =
        processes.kill(|process| launched_against(process, directory) && is_suru_client(process));

    let servers =
        processes.pids(|process| launched_against(process, directory) && is_server(process));
    let registered = registered_servers(state_dir, &servers);
    if !registered.is_empty() {
        request_stop(&registered);
        let registered = registered
            .iter()
            .map(|descriptor| Pid::from_u32(descriptor.pid))
            .collect::<Vec<_>>();
        processes.wait_until_gone(&registered, GRACEFUL_STOP);
    }

    let killed = processes.kill(|process| launched_against(process, directory));
    let mut launched = [clients, servers, killed].concat();
    launched.sort_unstable();
    launched.dedup();
    processes.wait_until_gone(&launched, PROGRESS_DEADLINE)
}

/// A server launched against `state_dir` that is this process's child, has
/// exited, and is still not collected once [`PROGRESS_DEADLINE`] has passed:
/// a zombie for as long as the test binary runs. The managed client that
/// launched it collects it on a thread of its own, so nothing the test's
/// blocked runtime would have to do stands in the way.
///
/// Only the servers a managed client launched are looked for: those leading
/// a process group of their own, as it launches them. Any other child — a
/// server a test spawned itself as a `tokio::process::Child`, say — is
/// collected by its handle on the test's runtime, which cannot run while
/// this does, so would read as uncollected when it is only waiting its turn.
/// A server is found by the Log it writes as it starts, named for its pid.
#[cfg(unix)]
fn uncollected_server(state_dir: &Path) -> Option<libc::pid_t> {
    let launched = launched_server_pids(state_dir);
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    loop {
        let uncollected = launched
            .iter()
            .copied()
            .find(|pid| is_uncollected_group_leader(*pid));
        if uncollected.is_none() || Instant::now() >= deadline {
            return uncollected;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Windows keeps no exited process waiting on its parent.
#[cfg(windows)]
fn uncollected_server(_state_dir: &Path) -> Option<u32> {
    None
}

/// The pids of the servers launched against `state_dir`, read from the
/// names of the Logs they write — `<time>-server-<pid>.log` — in the `log`
/// directory of the `release` channel at its root and of every other channel
/// in a directory of its own.
#[cfg(unix)]
fn launched_server_pids(state_dir: &Path) -> Vec<libc::pid_t> {
    let channel_dirs = std::fs::read_dir(state_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path());
    std::iter::once(state_dir.to_path_buf())
        .chain(channel_dirs)
        .filter_map(|dir| std::fs::read_dir(dir.join("log")).ok())
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let (_, pid) = name.strip_suffix(".log")?.rsplit_once("-server-")?;
            pid.parse().ok()
        })
        .collect()
}

/// Whether `pid` is a child of this process that leads its own process
/// group, has exited, and has not been collected. `waitid` reports only on
/// this process's own children, and by process group only on those still in
/// the group they were launched into, so a `pid` that was collected long ago
/// and taken by some unrelated process does not count.
#[cfg(unix)]
fn is_uncollected_group_leader(pid: libc::pid_t) -> bool {
    loop {
        // SAFETY: `info` is plain data for `waitid` to fill, and `WNOWAIT`
        // leaves whatever it reports for its owner to collect.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let checked = unsafe {
            libc::waitid(
                libc::P_PGID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if checked == 0 {
            // With `WNOHANG`, no exited child leaves `info` as it was: zeroed.
            // SAFETY: `waitid` succeeded, so `info` describes a child or none.
            return unsafe { info.si_pid() } == pid;
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return false;
        }
    }
}

mod watcher;

/// Whether `process` was launched against `state_dir`: a server is handed it
/// as an argument, and a CLI or TUI finds it in `SURU_STATE_DIR`, which the
/// servers they launch inherit as well.
fn launched_against(process: &sysinfo::Process, state_dir: &OsStr) -> bool {
    if process.pid() == Pid::from_u32(std::process::id()) {
        return false;
    }
    let mut variable = OsString::from("SURU_STATE_DIR=");
    variable.push(state_dir);
    process.cmd().iter().any(|argument| argument == state_dir)
        || process.environ().contains(&variable)
}

fn is_server(process: &sysinfo::Process) -> bool {
    process.cmd().iter().any(|argument| argument == "__server")
}

/// A Suru CLI or TUI: the tested binary run as anything but a server.
fn is_suru_client(process: &sysinfo::Process) -> bool {
    let suru = Path::new(env!("CARGO_BIN_EXE_suru")).file_name();
    !is_server(process) && process.exe().and_then(Path::file_name) == suru
}

/// The runtime descriptors under `state_dir` — the `release` channel's at its
/// root and every other channel's in a directory of its own — whose server is
/// one of `servers`. Any other descriptor belongs to a fixture of the test's
/// own, or to a server that is gone.
fn registered_servers(state_dir: &Path, servers: &[Pid]) -> Vec<RuntimeDescriptor> {
    let channel_dirs = std::fs::read_dir(state_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path());
    std::iter::once(state_dir.to_path_buf())
        .chain(channel_dirs)
        .filter_map(|dir| std::fs::read(dir.join("runtime.json")).ok())
        .filter_map(|contents| serde_json::from_slice::<RuntimeDescriptor>(&contents).ok())
        .filter(|descriptor| servers.contains(&Pid::from_u32(descriptor.pid)))
        .collect()
}

/// Asks each of `servers` to stop, as `suru server stop` does, without
/// waiting to hear whether it will. The requests go out on a thread of their
/// own: this runs on the test's runtime thread, which can neither block on a
/// future nor start a runtime of its own.
fn request_stop(servers: &[RuntimeDescriptor]) {
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(async {
                let client = reqwest::Client::new();
                let requests = servers.iter().map(|descriptor| {
                    client
                        .post(format!("{}/v1/server/stop", descriptor.base_url))
                        .bearer_auth(&descriptor.token)
                        .json(&ServerShutdown {
                            instance_id: descriptor.instance_id,
                            reason: ShutdownReason::Manual,
                        })
                        .timeout(GRACEFUL_STOP)
                        .send()
                });
                futures_util::future::join_all(requests).await;
            });
        });
    });
}

/// The processes on the machine, read only as far as telling which were
/// launched against a state directory needs.
struct Processes {
    system: System,
}

impl Processes {
    fn new() -> Self {
        Self {
            system: System::new(),
        }
    }

    fn refresh(&mut self) {
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .without_tasks()
                .with_cmd(UpdateKind::Always)
                .with_environ(UpdateKind::Always)
                .with_exe(UpdateKind::Always),
        );
    }

    /// The running processes `matches` picks out, read afresh.
    fn pids(&mut self, matches: impl Fn(&sysinfo::Process) -> bool) -> Vec<Pid> {
        self.refresh();
        self.system
            .processes()
            .values()
            .filter(|process| process.status() != ProcessStatus::Zombie && matches(process))
            .map(sysinfo::Process::pid)
            .collect()
    }

    /// Kills every running process `matches` picks out, read afresh, and
    /// answers which they were.
    fn kill(&mut self, matches: impl Fn(&sysinfo::Process) -> bool) -> Vec<Pid> {
        let pids = self.pids(matches);
        for pid in &pids {
            if let Some(process) = self.system.process(*pid) {
                process.kill();
            }
        }
        pids
    }

    /// Waits up to `limit` for every one of `pids` to have exited, and
    /// answers those still running. Collecting one that is this process's
    /// child is left to whatever launched it.
    fn wait_until_gone(&mut self, pids: &[Pid], limit: Duration) -> Vec<Pid> {
        let deadline = Instant::now() + limit;
        loop {
            self.refresh();
            let running = pids
                .iter()
                .copied()
                .filter(|pid| {
                    self.system
                        .process(*pid)
                        .is_some_and(|process| process.status() != ProcessStatus::Zombie)
                })
                .collect::<Vec<_>>();
            if running.is_empty() || Instant::now() >= deadline {
                return running;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
