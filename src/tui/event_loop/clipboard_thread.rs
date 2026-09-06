//! A lazy, single-owner native Clipboard with one replaceable waiting copy.

use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::NativeClipboardSink;

const DEFAULT_EXIT_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Default)]
struct PendingCopy {
    text: Option<String>,
    stopping: bool,
}

struct Worker {
    pending: Arc<(Mutex<PendingCopy>, Condvar)>,
    thread: JoinHandle<()>,
}

pub(super) struct ClipboardThread<F> {
    create_writer: Arc<F>,
    worker: Option<Worker>,
    failure_reported: bool,
    exit_timeout: Duration,
}

impl<F, S> ClipboardThread<F>
where
    F: Fn() -> S + Send + Sync + 'static,
    S: NativeClipboardSink + 'static,
{
    pub(super) fn new(create_writer: F) -> Self {
        Self {
            create_writer: Arc::new(create_writer),
            worker: None,
            failure_reported: false,
            exit_timeout: DEFAULT_EXIT_TIMEOUT,
        }
    }
}

impl<F, S> NativeClipboardSink for ClipboardThread<F>
where
    F: Fn() -> S + Send + Sync + 'static,
    S: NativeClipboardSink + 'static,
{
    fn copy(&mut self, text: &str) {
        if self.worker.is_none() {
            let pending = Arc::new((Mutex::new(PendingCopy::default()), Condvar::new()));
            let incoming = pending.clone();
            let create_writer = self.create_writer.clone();
            match thread::Builder::new()
                .name("suru-clipboard".to_owned())
                .spawn(move || {
                    // The handle is created, used, and dropped on this thread;
                    // the native writer need not even be Send.
                    let mut writer = create_writer();
                    loop {
                        let text = {
                            let (slot, wake) = &*incoming;
                            let mut pending = slot.lock().unwrap();
                            while pending.text.is_none() && !pending.stopping {
                                pending = wake.wait(pending).unwrap();
                            }
                            match pending.text.take() {
                                Some(text) => text,
                                None => break,
                            }
                        };
                        // Never hold the slot's lock across a display call.
                        writer.copy(&text);
                    }
                }) {
                Ok(thread) => self.worker = Some(Worker { pending, thread }),
                Err(error) => {
                    if self.failure_reported {
                        tracing::debug!(%error, "could not start native Clipboard thread");
                    } else {
                        self.failure_reported = true;
                        tracing::warn!(%error, "could not start native Clipboard thread");
                    }
                    return;
                }
            }
        }
        let (slot, wake) = &*self.worker.as_ref().unwrap().pending;
        slot.lock().unwrap().text = Some(text.to_owned());
        wake.notify_one();
    }
}

impl<F> ClipboardThread<F> {
    #[cfg(test)]
    pub(super) fn with_exit_timeout(mut self, timeout: Duration) -> Self {
        self.exit_timeout = timeout;
        self
    }

    /// Give the latest waiting copy and native handle's handover a bounded
    /// chance to finish. A stalled worker is detached when the bound expires.
    pub(super) fn shutdown(&mut self) -> bool {
        let began = Instant::now();
        let Some(worker) = self.worker.take() else {
            return true;
        };
        let (slot, wake) = &*worker.pending;
        slot.lock().unwrap().stopping = true;
        wake.notify_one();
        // The worker drops its native handle before returning. Wait for that
        // completion before calling the otherwise unbounded join.
        while !worker.thread.is_finished() {
            let remaining = self.exit_timeout.saturating_sub(began.elapsed());
            if remaining.is_zero() {
                tracing::debug!("native Clipboard thread exceeded its exit timeout");
                return false;
            }
            thread::sleep(remaining.min(Duration::from_millis(1)));
        }
        worker.thread.join().is_ok()
    }
}

impl<F> Drop for ClipboardThread<F> {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    struct Writer<F>(F);

    impl<F: FnMut(&str)> NativeClipboardSink for Writer<F> {
        fn copy(&mut self, text: &str) {
            (self.0)(text);
        }
    }

    #[test]
    fn a_run_without_copy_never_creates_a_native_writer() {
        let mut clipboard = ClipboardThread::new(|| -> Writer<fn(&str)> {
            panic!("a run without copying must not open a display");
        })
        .with_exit_timeout(Duration::ZERO);
        assert!(clipboard.shutdown());
    }

    #[test]
    fn one_native_handle_is_created_used_and_dropped_on_the_clipboard_thread() {
        use std::rc::Rc;

        struct Handle {
            // Native handles may be thread-affine, so do not require Send.
            events: Rc<mpsc::Sender<(String, thread::ThreadId)>>,
        }

        impl NativeClipboardSink for Handle {
            fn copy(&mut self, text: &str) {
                self.events
                    .send((text.to_owned(), thread::current().id()))
                    .unwrap();
            }
        }

        impl Drop for Handle {
            fn drop(&mut self) {
                self.events
                    .send(("dropped".to_owned(), thread::current().id()))
                    .unwrap();
            }
        }

        let (events, received) = mpsc::channel();
        let mut clipboard = ClipboardThread::new(move || {
            events
                .send(("created".to_owned(), thread::current().id()))
                .unwrap();
            Handle {
                events: Rc::new(events.clone()),
            }
        })
        .with_exit_timeout(Duration::from_millis(100));
        clipboard.copy("first");
        let created = received.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(created.0, "created");
        assert_ne!(created.1, thread::current().id());
        assert_eq!(
            received.recv_timeout(Duration::from_secs(1)).unwrap(),
            ("first".to_owned(), created.1)
        );
        clipboard.copy("latest");
        assert!(clipboard.shutdown());
        assert_eq!(
            received.try_iter().collect::<Vec<_>>(),
            [
                ("latest".to_owned(), created.1),
                ("dropped".to_owned(), created.1),
            ]
        );
    }

    #[test]
    fn shutdown_bounds_a_stalled_native_write() {
        let (started, writing) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        let resume = Arc::new(Mutex::new(resume));
        let (stopped, shutdown) = mpsc::channel();
        let caller = thread::spawn(move || {
            let mut clipboard = ClipboardThread::new(move || {
                let started = started.clone();
                let resume = resume.clone();
                Writer(move |_: &str| {
                    started.send(()).unwrap();
                    let _ = resume.lock().unwrap().recv();
                })
            })
            .with_exit_timeout(Duration::from_millis(20));
            clipboard.copy("text");
            writing.recv_timeout(Duration::from_secs(1)).unwrap();
            let began = Instant::now();
            let joined = clipboard.shutdown();
            drop(clipboard);
            stopped.send((joined, began.elapsed())).unwrap();
        });
        // The watchdog is separate from the injected bound. Dropping release
        // also frees the fake writer if an assertion fails.
        let (joined, elapsed) = shutdown.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(!joined);
        assert!(elapsed < Duration::from_millis(120), "{elapsed:?}");
        release.send(()).unwrap();
        caller.join().unwrap();
    }

    #[test]
    fn dropping_the_clipboard_bounds_stalled_handle_creation_and_handover() {
        struct Gate {
            entered: mpsc::Sender<()>,
            resume: Arc<Mutex<mpsc::Receiver<()>>>,
        }

        struct Handle {
            gate: Option<Gate>,
            finished: mpsc::Sender<()>,
        }

        impl NativeClipboardSink for Handle {
            fn copy(&mut self, _: &str) {}
        }

        impl Drop for Handle {
            fn drop(&mut self) {
                if let Some(gate) = &self.gate {
                    gate.entered.send(()).unwrap();
                    let _ = gate.resume.lock().unwrap().recv();
                }
                let _ = self.finished.send(());
            }
        }

        for stall_creation in [true, false] {
            let (entered, stalled) = mpsc::channel();
            let (release, resume) = mpsc::channel();
            let resume = Arc::new(Mutex::new(resume));
            let (finished, released) = mpsc::channel();
            let (stopped, shutdown) = mpsc::channel();
            let caller = thread::spawn(move || {
                let mut clipboard = ClipboardThread::new(move || {
                    if stall_creation {
                        entered.send(()).unwrap();
                        let _ = resume.lock().unwrap().recv();
                    }
                    Handle {
                        gate: (!stall_creation).then(|| Gate {
                            entered: entered.clone(),
                            resume: resume.clone(),
                        }),
                        finished: finished.clone(),
                    }
                })
                .with_exit_timeout(Duration::from_millis(20));
                clipboard.copy("text");
                let began = Instant::now();
                drop(clipboard);
                stopped.send(began.elapsed()).unwrap();
            });
            stalled.recv_timeout(Duration::from_secs(1)).unwrap();
            let elapsed = shutdown.recv_timeout(Duration::from_secs(1)).unwrap();
            assert!(elapsed < Duration::from_millis(120), "{elapsed:?}");
            release.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(1)).unwrap();
            caller.join().unwrap();
        }
    }
}
