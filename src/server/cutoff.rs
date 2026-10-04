//! The hard end a Server process's stop is held to, kept apart from the
//! runtime the Server runs on.
//!
//! A stop's deadline and overrun (`ServerTimings::shutdown_deadline`) are kept
//! by the Server's own tasks, which can be kept by only as long as the runtime
//! has a worker free to run them. Its workers can all be held: a Session's
//! commit waits for storage to acknowledge a Turn's settling while it holds
//! the Session store, so one write to storage that does not answer holds a
//! worker, and every other actor reaching for the store holds another — one
//! active Turn is enough on a runtime of one worker. Nor is what follows the
//! stop safe: reporting how it went writes to the process's stderr, which a
//! managed client points at a Log in the state directory, and a directory
//! that does not answer holds that write for as long as it does not.
//!
//! So a `suru __server` process also keeps a [`ProcessCutoff`]: a thread of
//! its own, owing the runtime nothing, that hears the shutdown signals itself
//! and ends the process outright — skipping every exit handler and lock a
//! held thread could be keeping — should the process still be running at
//! the cutoff, or as a second signal arrives. Ending it outright loses
//! nothing the stop has not already given up on: every Provider process tree
//! is taken down as the process ends — by its group's anchor once the
//! process's lifeline closes on Unix, by its Job Object closing on Windows
//! (`process_tree`) — and SQLite's journal keeps storage whole, rolling back
//! whatever write was cut off. A Server run in some other process, as tests
//! run one, keeps none: only a process that is a Server's alone may be ended
//! this way.

use std::{
    io,
    sync::mpsc,
    time::{Duration, Instant},
};

use super::ServerTimings;

/// How long, by default, past its deadline and overrun a stop's process may
/// run before its cutoff ends it, and how long a process whose Server has
/// stopped may take to report how the stop went, flush its Log and exit
/// (`ServerTimings::shutdown_cutoff_margin`). Everything the stop waits on
/// is behind it by then; this covers only those last steps, which take
/// milliseconds when nothing holds them.
pub const CUTOFF_MARGIN: Duration = Duration::from_secs(1);

/// The status a process its cutoff ended exits with, as `timeout(1)` reports
/// a command it ended: distinct from the 0 of a stop that ran its course, the
/// 1 of one that failed or was cut short at its deadline, and that of a
/// process a second signal ended: 128 and the signal's number on Unix, as a
/// shell reports a process the signal ended, and on Windows
/// `STATUS_CONTROL_C_EXIT`, as a console's default handler leaves it.
pub const CUT_OFF_EXIT_STATUS: i32 = 124;

/// The hard end of a Server process's stop: started once, before the Server
/// is, by the process's `main`, and told as the stop begins and as it ends.
#[derive(Clone)]
pub struct ProcessCutoff {
    events: mpsc::Sender<CutoffEvent>,
    /// How long after a stop begins the process may run before it is ended.
    allowance: Duration,
    /// How long after its Server has stopped the process may run.
    margin: Duration,
}

enum CutoffEvent {
    /// The process must have ended by this.
    EndBy(Instant),
    /// A shutdown signal arrived, which a second of ends the process with
    /// this status.
    Signal { name: &'static str, status: i32 },
}

impl ProcessCutoff {
    /// Starts keeping the cutoff for a Server stopped within `timings`'
    /// deadline and overrun, and hearing the shutdown signals
    /// ([`ShutdownSignals`](super::ShutdownSignals)) apart from the runtime:
    /// the first arms the cutoff, as the stop it begins, and a second ends
    /// the process at once. Must be started after the `ShutdownSignals` are
    /// listened for, so that on Windows the console's events reach this
    /// first, and then the runtime's listener too.
    pub fn start(timings: &ServerTimings) -> io::Result<Self> {
        let margin = timings.shutdown_cutoff_margin;
        let allowance = timings.shutdown_deadline + timings.shutdown_overrun + margin;
        let (events, received) = mpsc::channel();
        listen_for_signals(events.clone())?;
        std::thread::Builder::new()
            .name("suru-cutoff".to_owned())
            .spawn(move || keep(received, allowance))?;
        Ok(Self {
            events,
            allowance,
            margin,
        })
    }

    /// Holds the process to the cutoff of a stop that began at `since`:
    /// ended outright should it still be running its deadline, overrun and
    /// margin later.
    pub fn stop_began(&self, since: Instant) {
        let _ = self.events.send(CutoffEvent::EndBy(since + self.allowance));
    }

    /// The Server has stopped: what the process has left to do — report how
    /// the stop went, flush its Log and exit — it has the cutoff's margin to
    /// do, or less where the stop's own cutoff comes sooner.
    pub fn server_stopped(&self) {
        let _ = self
            .events
            .send(CutoffEvent::EndBy(Instant::now() + self.margin));
    }
}

/// Keeps the cutoff: waits for the soonest end it was given, or a second
/// shutdown signal, and ends the process at whichever comes first. The first
/// signal begins a stop, so it holds the process to that stop's cutoff
/// itself, should the runtime never get as far as beginning it.
///
/// What it logs as it ends the process is only queued for the Log's own
/// writer, which never makes a caller wait, so is kept only should that
/// writer reach it first.
fn keep(events: mpsc::Receiver<CutoffEvent>, allowance: Duration) {
    let mut end_by: Option<Instant> = None;
    let mut signalled = false;
    loop {
        let event = match end_by {
            Some(end_by) => {
                match events.recv_timeout(end_by.saturating_duration_since(Instant::now())) {
                    Ok(event) => event,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        tracing::error!(
                            "the server's stop ran past its cutoff; ending the process"
                        );
                        end_now(CUT_OFF_EXIT_STATUS);
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
            None => match events.recv() {
                Ok(event) => event,
                Err(_) => return,
            },
        };
        let at = match event {
            CutoffEvent::EndBy(at) => at,
            CutoffEvent::Signal { name, status } if signalled => {
                tracing::warn!(
                    signal = name,
                    "second shutdown signal received while stopping; ending the process"
                );
                end_now(status);
            }
            CutoffEvent::Signal { .. } => {
                signalled = true;
                Instant::now() + allowance
            }
        };
        end_by = Some(end_by.map_or(at, |end_by| end_by.min(at)));
    }
}

/// Ends the process at once with `status`, running nothing more: no exit
/// handler, no flush of a buffered stream, no lock a held thread may keep.
fn end_now(status: i32) -> ! {
    #[cfg(unix)]
    {
        // SAFETY: `_exit` takes no pointers and does not return.
        unsafe { libc::_exit(status) }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
        // SAFETY: the pseudo-handle of the current process is always valid.
        // Terminating it runs no DLL detach or other user-mode cleanup, and
        // does not return.
        unsafe {
            TerminateProcess(GetCurrentProcess(), status as u32);
        }
        unreachable!("the process was terminated")
    }
}

/// Hears SIGINT, SIGTERM and SIGHUP on a thread of its own, beside the
/// runtime's listener, which still hears them.
#[cfg(unix)]
fn listen_for_signals(events: mpsc::Sender<CutoffEvent>) -> io::Result<()> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    let mut signals = signal_hook::iterator::Signals::new([SIGINT, SIGTERM, SIGHUP])?;
    std::thread::Builder::new()
        .name("suru-cutoff-signals".to_owned())
        .spawn(move || {
            for number in signals.forever() {
                let name = match number {
                    SIGINT => "SIGINT",
                    SIGTERM => "SIGTERM",
                    _ => "SIGHUP",
                };
                // How a shell reports a process the signal ended.
                let status = 128 + number;
                if events.send(CutoffEvent::Signal { name, status }).is_err() {
                    return;
                }
            }
        })?;
    Ok(())
}

/// Hears Ctrl-C, Ctrl-Break and the console closing through a console
/// handler of its own, which Windows runs, on a thread it makes for the
/// purpose, before the runtime's listener registered earlier — which this
/// lets hear each event too.
#[cfg(windows)]
fn listen_for_signals(events: mpsc::Sender<CutoffEvent>) -> io::Result<()> {
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, SetConsoleCtrlHandler,
    };

    static EVENTS: std::sync::OnceLock<std::sync::Mutex<mpsc::Sender<CutoffEvent>>> =
        std::sync::OnceLock::new();

    unsafe extern "system" fn handler(event: u32) -> windows_sys::core::BOOL {
        let name = match event {
            CTRL_C_EVENT => "Ctrl-C",
            CTRL_BREAK_EVENT => "Ctrl-Break",
            CTRL_CLOSE_EVENT => "console close",
            _ => return 0,
        };
        if let Some(events) = EVENTS.get()
            && let Ok(events) = events.lock()
        {
            // How Windows reports a console process its console's default
            // handler ended.
            let _ = events.send(CutoffEvent::Signal {
                name,
                status: windows_sys::Win32::Foundation::STATUS_CONTROL_C_EXIT,
            });
        }
        // Not handled here, so the runtime's listener hears it too.
        0
    }

    if EVENTS.set(std::sync::Mutex::new(events)).is_err() {
        return Err(io::Error::other("the process cutoff is started once"));
    }
    // SAFETY: `handler` is a valid handler routine for the life of the
    // process.
    if unsafe { SetConsoleCtrlHandler(Some(handler), 1) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
