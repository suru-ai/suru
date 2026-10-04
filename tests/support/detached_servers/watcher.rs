//! The process that stops what a test launched should the test process end
//! without dropping its [`DetachedServers`]: one a runner times out, one
//! cancelled with Ctrl-C, or one killed outright. Nothing in the test process
//! can be relied on to run then, so this is the test binary itself, run again
//! as [`stop_what_tests_launched_once_their_process_ends`] in a process group
//! of its own, out of reach of whatever signal ends the test's.
//!
//! Each [`DetachedServers`] tells it of its state directory, and of having
//! stopped what was launched against it, on a pipe to its stdin. It acts
//! once the test process has ended, when nothing in it can launch a server
//! behind its back, and learns that two ways. The pipe closes when the test
//! process ends — unless a process the test launched inherited the test's
//! end of it in the moment before that end was marked to close on launch, as
//! it is on macOS, which can make a pipe only in two steps. So it also
//! watches the test process itself, and once that is gone reads whatever is
//! left in the pipe — everything the test process ever wrote is there by
//! then — until a look finds nothing more, rather than waiting for the pipe
//! to close.
//!
//! It acts on a directory only once it finds there the marker holding the
//! token it was told with, so nothing but a directory a [`DetachedServers`]
//! made is ever swept or removed.
//!
//! [`DetachedServers`]: super::DetachedServers

use std::{
    ffi::OsString,
    io::Write,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Mutex, OnceLock},
    time::Duration,
};

use super::{MARKER_FILE, Processes, launched_against, stop_launched_against};

/// Set, to the test process's identity, in the environment of the test
/// binary re-executed as the watcher.
const WATCHER_VARIABLE: &str = "SURU_DETACHED_SERVERS_WATCHER";

/// How often the watcher looks to see whether the test process is still
/// running, while it waits to be told anything more.
const PARENT_POLL: Duration = Duration::from_millis(200);

/// The random token a [`super::DetachedServers`] marks its state directory
/// with, and tells the watcher along with it: a v4 UUID, as 32 hex digits.
pub(super) struct Token(String);

const TOKEN_LENGTH: usize = 32;

impl Token {
    pub(super) fn new() -> Self {
        Self(uuid::Uuid::new_v4().simple().to_string())
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

/// What a [`super::DetachedServers`] tells the watcher of its directory.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Change {
    /// What is launched against the directory is to be stopped should the
    /// test process end.
    InUse = 1,
    /// What was launched against the directory has been stopped.
    Stopped = 2,
}

/// Tells the watcher, launched the first time it is needed, of `change` to
/// `state_dir`, marked with `token`.
pub(super) fn tell(change: Change, token: &Token, state_dir: &Path) {
    static WATCHER: OnceLock<Mutex<Watcher>> = OnceLock::new();
    let watcher = WATCHER.get_or_init(|| Mutex::new(launch()));
    let mut watcher = watcher
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut record = vec![change as u8];
    record.extend_from_slice(token.as_str().as_bytes());
    let path = path_bytes(state_dir);
    record.extend_from_slice(&u32::try_from(path.len()).expect("path fits").to_le_bytes());
    record.extend_from_slice(&path);
    // A watcher already gone has nothing left to be told.
    let _ = watcher.told.write_all(&record);
}

/// The watcher, kept for as long as the test process runs: it is never
/// waited on here, since it ends only once the test process has.
struct Watcher {
    _process: Child,
    told: ChildStdin,
}

fn launch() -> Watcher {
    // The name the test harness knows the watcher's test by: its path in this
    // binary, without the crate's name.
    let module = module_path!();
    let test = format!(
        "{}::stop_what_tests_launched_once_their_process_ends",
        module.split_once("::").map_or(module, |(_, path)| path)
    );
    let mut command = Command::new(std::env::current_exe().expect("locate the test binary"));
    command
        .args(["--exact", &test, "--ignored", "--nocapture"])
        .env(WATCHER_VARIABLE, TestProcess::current().to_string())
        .stdin(Stdio::piped())
        // Nothing is left holding a runner's output once the test process
        // has ended, so the runner does not wait on the watcher.
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Launched as a managed client launches a server, so it inherits none of
    // a runner's output handles to hold open through its sweep.
    let mut process = suru::managed_client::launch_detached(&mut command)
        .expect("launch the detached server watcher");
    let told = process.stdin.take().expect("the watcher's stdin is piped");
    Watcher {
        _process: process,
        told,
    }
}

/// A path's own bytes, whatever they are: its bytes on Unix, and on Windows
/// its UTF-16 units, each in little-endian order.
fn path_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str()).to_vec()
    }
    #[cfg(windows)]
    {
        std::os::windows::ffi::OsStrExt::encode_wide(path.as_os_str())
            .flat_map(u16::to_le_bytes)
            .collect()
    }
}

/// The path [`path_bytes`] made `bytes` of.
fn path_from_bytes(bytes: Vec<u8>) -> PathBuf {
    #[cfg(unix)]
    {
        PathBuf::from(<OsString as std::os::unix::ffi::OsStringExt>::from_vec(
            bytes,
        ))
    }
    #[cfg(windows)]
    {
        let wide = bytes
            .chunks_exact(2)
            .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
            .collect::<Vec<_>>();
        PathBuf::from(<OsString as std::os::windows::ffi::OsStringExt>::from_wide(
            &wide,
        ))
    }
}

/// One thing the watcher was told: a [`Change`], the token, and the path.
struct Told {
    change: u8,
    token: String,
    state_dir: PathBuf,
}

/// Takes the first whole record [`tell`] wrote off the front of `read`, or
/// nothing while the rest of it is still to be read.
fn take_told(read: &mut Vec<u8>) -> Option<Told> {
    const HEADER: usize = 1 + TOKEN_LENGTH + 4;
    let header = read.get(..HEADER)?;
    let length = u32::from_le_bytes(header[1 + TOKEN_LENGTH..].try_into().ok()?) as usize;
    if read.len() < HEADER + length {
        return None;
    }
    let record = read.drain(..HEADER + length).collect::<Vec<_>>();
    Some(Told {
        change: record[0],
        token: String::from_utf8_lossy(&record[1..1 + TOKEN_LENGTH]).into_owned(),
        state_dir: path_from_bytes(record[HEADER..].to_vec()),
    })
}

/// What the pipe from the test process gave on one look at it.
enum Pipe {
    Read(Vec<u8>),
    Empty,
    Closed,
}

/// The watcher's end of the pipe from the test process.
///
/// On Unix it is read without blocking past a look at what is there, since
/// another process may hold the test's end open, so neither its closing nor
/// a read can be waited for once the test process is gone. Read straight
/// from the descriptor, so nothing waits in a buffer a look would miss.
#[cfg(unix)]
struct TestPipe;

#[cfg(unix)]
impl TestPipe {
    fn new() -> Self {
        Self
    }

    /// Waits up to `wait` for the pipe to have something to read, and reads
    /// what it has.
    fn look(&mut self, wait: Duration) -> Pipe {
        let mut stdin = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let wait = libc::c_int::try_from(wait.as_millis()).unwrap_or(libc::c_int::MAX);
        // SAFETY: polls the one descriptor `stdin` describes.
        match unsafe { libc::poll(&mut stdin, 1, wait) } {
            0 => return Pipe::Empty,
            ready if ready < 0 => {
                return if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                {
                    Pipe::Empty
                } else {
                    Pipe::Closed
                };
            }
            _ => {}
        }
        let mut read = vec![0u8; 64 * 1024];
        // SAFETY: reads into `read`, at most its length; `poll` said a read
        // would not block.
        let count = unsafe { libc::read(libc::STDIN_FILENO, read.as_mut_ptr().cast(), read.len()) };
        match count {
            0 => Pipe::Closed,
            count if count < 0 => {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    Pipe::Empty
                } else {
                    Pipe::Closed
                }
            }
            count => {
                read.truncate(count as usize);
                Pipe::Read(read)
            }
        }
    }

    /// Everything left in the pipe once the test process is gone, read until
    /// a look finds nothing more: having ended, it can write nothing new.
    fn drain(&mut self) -> Vec<u8> {
        let mut drained = Vec::new();
        while let Pipe::Read(read) = self.look(Duration::ZERO) {
            drained.extend(read);
        }
        drained
    }
}

/// The watcher's end of the pipe from the test process, read on a thread of
/// its own. On Windows the test's end of the pipe was never inheritable, so
/// it closes when the test process ends, and draining it reads to its end.
#[cfg(windows)]
struct TestPipe(std::sync::mpsc::Receiver<Vec<u8>>);

#[cfg(windows)]
impl TestPipe {
    fn new() -> Self {
        use std::io::Read;

        let (sender, received) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut read = vec![0u8; 64 * 1024];
            while let Ok(count @ 1..) = stdin.read(&mut read) {
                if sender.send(read[..count].to_vec()).is_err() {
                    return;
                }
            }
        });
        Self(received)
    }

    fn look(&mut self, wait: Duration) -> Pipe {
        match self.0.recv_timeout(wait) {
            Ok(read) => Pipe::Read(read),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Pipe::Empty,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Pipe::Closed,
        }
    }

    fn drain(&mut self) -> Vec<u8> {
        self.0.iter().flatten().collect()
    }
}

/// The test process the watcher watches, told it as `<pid>:<start time>`:
/// the start time tells it apart from a later process given the same pid.
#[derive(Clone, Copy, PartialEq, Eq)]
struct TestProcess {
    pid: u32,
    started: u64,
}

impl TestProcess {
    fn current() -> Self {
        let pid = sysinfo::Pid::from_u32(std::process::id());
        let mut system = sysinfo::System::new();
        system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
        Self {
            pid: pid.as_u32(),
            started: system
                .process(pid)
                .expect("read the test process's own start time")
                .start_time(),
        }
    }

    fn parse(told: &str) -> Option<Self> {
        let (pid, started) = told.split_once(':')?;
        Some(Self {
            pid: pid.parse().ok()?,
            started: started.parse().ok()?,
        })
    }

    /// Whether the process is still running, not merely waiting on its own
    /// parent to collect it.
    fn is_running(self) -> bool {
        let pid = sysinfo::Pid::from_u32(self.pid);
        let mut system = sysinfo::System::new();
        system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
        system.process(pid).is_some_and(|process| {
            process.start_time() == self.started
                && process.status() != sysinfo::ProcessStatus::Zombie
        })
    }
}

impl std::fmt::Display for TestProcess {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}:{}", self.pid, self.started)
    }
}

/// Not a test of its own: [`launch`] re-executes the test binary to reach
/// this function, which reads the state directories in use from its stdin
/// until the test process ends, then stops what was launched against each.
/// Ignored so an ordinary run never reaches it, and does nothing unless it
/// was launched as the watcher.
#[test]
#[ignore = "re-executed by DetachedServers to outlive the test process"]
fn stop_what_tests_launched_once_their_process_ends() {
    let Some(test_process) = std::env::var(WATCHER_VARIABLE)
        .ok()
        .as_deref()
        .and_then(TestProcess::parse)
    else {
        return;
    };
    let mut pipe = TestPipe::new();
    let mut read = Vec::new();
    loop {
        match pipe.look(PARENT_POLL) {
            Pipe::Read(more) => read.extend(more),
            Pipe::Closed => break,
            Pipe::Empty if test_process.is_running() => {}
            Pipe::Empty => {
                read.extend(pipe.drain());
                break;
            }
        }
    }
    let mut in_use = Vec::<(PathBuf, String)>::new();
    while let Some(told) = take_told(&mut read) {
        in_use.retain(|(state_dir, _)| *state_dir != told.state_dir);
        if told.change == Change::InUse as u8 {
            in_use.push((told.state_dir, told.token));
        }
    }
    for (state_dir, token) in &in_use {
        if !made_by_detached_servers(state_dir, token) {
            continue;
        }
        // The test process is gone, so whatever a sweep finds was launched
        // before it ended; another only confirms there is nothing left.
        for _ in 0..3 {
            stop_launched_against(state_dir);
            let directory = state_dir.as_os_str();
            if Processes::new()
                .pids(|process| launched_against(process, directory))
                .is_empty()
            {
                let _ = std::fs::remove_dir_all(state_dir);
                break;
            }
        }
    }
}

/// Whether `state_dir` is a directory, not a link to one, holding the marker
/// a [`super::DetachedServers`] made it with, carrying `token`.
fn made_by_detached_servers(state_dir: &Path, token: &str) -> bool {
    state_dir.is_absolute()
        && std::fs::symlink_metadata(state_dir).is_ok_and(|metadata| metadata.is_dir())
        && std::fs::read(state_dir.join(MARKER_FILE)).is_ok_and(|marker| marker == token.as_bytes())
}
