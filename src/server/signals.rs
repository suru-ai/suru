//! The operating system's ways of asking a running Server to stop.

use anyhow::{Context, Result};

/// The signals a Server process answers with the same graceful shutdown a
/// client's stop request starts: its Providers are told to stop, settle the
/// Turns they were running while Sessions can still be written (ADR 0029),
/// and are taken down with their process trees before the process exits.
///
/// On Unix these are SIGINT, which Ctrl-C at a terminal sends; SIGTERM, which
/// `kill`, a service manager, and a system going down send; and SIGHUP, which
/// the terminal or login session a Server was started from sends as it ends.
/// Left to their default action SIGTERM and SIGHUP end the process outright:
/// nothing is dropped, and every Provider — each in a process group of its
/// own, so that stopping one reaches all it started — outlives the Server as
/// an orphan.
///
/// On Windows these are Ctrl-C, Ctrl-Break, and the console a Server runs in
/// closing, which allows a few seconds for the shutdown before Windows ends
/// the process regardless. Logging off and shutting down the system are
/// delivered only to services, which a Server is not. None of these reach a
/// Server the managed client launched, since it is detached from any
/// console; its Providers instead belong to a Job Object the kernel closes
/// however the Server ends, which takes them down with it.
///
/// Listening begins in [`ShutdownSignals::listen`], before the Server is
/// spawned, rather than when it starts waiting: a signal that arrives once a
/// client can find the Server — after its runtime descriptor is published but
/// before its run loop is polled — is then still answered gracefully rather
/// than by the default action.
///
/// Only the first signal is the runtime's to answer. A second, arriving while
/// the stop the first began is still under way, ends the process at once, as
/// someone pressing Ctrl-C again expects; that is heard apart from the
/// runtime, by the process's [`ProcessCutoff`](super::ProcessCutoff), since a
/// runtime whose workers are all held could never answer it. The listener is
/// held for as long as the Server runs and stops all the same, so a signal it
/// no longer waits on is absorbed rather than left to its default action.
///
/// On Windows a console closing allows the process a few seconds whichever
/// event it is, and holding the listener changes none of that: closing first,
/// it begins a graceful stop that Windows ends at its own limit should it
/// still be running — the Job Objects taking the Providers down as the
/// process ends — and closing second, it ends the process like any other.
pub struct ShutdownSignals {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(unix)]
    hangup: tokio::signal::unix::Signal,
    #[cfg(windows)]
    ctrl_c: tokio::signal::windows::CtrlC,
    #[cfg(windows)]
    ctrl_break: tokio::signal::windows::CtrlBreak,
    #[cfg(windows)]
    ctrl_close: tokio::signal::windows::CtrlClose,
}

impl ShutdownSignals {
    /// Takes these signals over from their default action for the rest of the
    /// process. Must be called inside the Tokio runtime.
    #[cfg(unix)]
    pub fn listen() -> Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            interrupt: signal(SignalKind::interrupt()).context("listen for SIGINT")?,
            terminate: signal(SignalKind::terminate()).context("listen for SIGTERM")?,
            hangup: signal(SignalKind::hangup()).context("listen for SIGHUP")?,
        })
    }

    /// Takes these console events over from their default action for as long
    /// as the returned listener lives. Must be called inside the Tokio
    /// runtime.
    #[cfg(windows)]
    pub fn listen() -> Result<Self> {
        use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close};
        Ok(Self {
            ctrl_c: ctrl_c().context("listen for Ctrl-C")?,
            ctrl_break: ctrl_break().context("listen for Ctrl-Break")?,
            ctrl_close: ctrl_close().context("listen for the console closing")?,
        })
    }

    /// Resolves with the next of these signals to arrive. Each is heard
    /// once, however many arrived since the last was heard, and one that
    /// arrives while nothing waits on this is heard the next time something
    /// does.
    pub(super) async fn next(&mut self) -> &'static str {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.interrupt.recv() => "SIGINT",
                _ = self.terminate.recv() => "SIGTERM",
                _ = self.hangup.recv() => "SIGHUP",
            }
        }
        #[cfg(windows)]
        {
            tokio::select! {
                _ = self.ctrl_c.recv() => "Ctrl-C",
                _ = self.ctrl_break.recv() => "Ctrl-Break",
                _ = self.ctrl_close.recv() => "console close",
            }
        }
    }
}
